//! `cargo xtask <step>`: the project's checks, so that a local run and CI run
//! exactly the same commands. `cargo xtask ci` runs every step in CI's order
//! and stops at the first failure.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

mod dist;
mod sha256;
mod unsafe_audit;

/// The cargo-deny version CI installs (.github/workflows/supply-chain.yml).
const CARGO_DENY_VERSION: &str = "0.20.2";

/// The nightly the AddressSanitizer job uses; supply-chain.yml installs the
/// same one. Pinned by date so a nightly regression cannot fail the job.
const ASAN_TOOLCHAIN: &str = "nightly-2026-10-20";

/// The steps `ci` runs, in order: cheap and likely-to-fail first.
const CI: &[&str] = &[
    "fmt",
    "clippy",
    "unsafe-audit",
    "msrv",
    "macos",
    "test",
    "test-release",
    "unprivileged",
    "doc",
    "package",
];

const STEPS: &[(&str, &str)] = &[
    ("ci", "fmt through package below, in order"),
    ("fmt", "cargo fmt --check"),
    ("clippy", "Clippy on every target, warnings denied"),
    ("unsafe-audit", "every `unsafe` is listed in doc/unsafe.md"),
    (
        "msrv",
        "type-check with the rust-version toolchain (rustup)",
    ),
    (
        "macos",
        "type-check and lint for aarch64-apple-darwin from another OS",
    ),
    ("test", "cargo test (debug: overflow checks on)"),
    ("test-release", "cargo test --release"),
    (
        "unprivileged",
        "as root: run every test binary again as nobody",
    ),
    ("doc", "cargo doc, warnings denied"),
    ("package", "cargo package (crates.io dry run)"),
    (
        "gnu",
        "GNU coreutils' own test suite (util/gnu-tests.sh; slow, non-root)",
    ),
    (
        "deny",
        "cargo deny check: advisories, licenses, bans, sources (deny.toml)",
    ),
    (
        "asan",
        "the test suites under AddressSanitizer (pinned nightly + rust-src)",
    ),
    (
        "dist",
        "reproducible release archive in target/dist/out [--target T]",
    ),
    (
        "repro",
        "build the release twice in two trees, require identical output [--target T]",
    ),
];

/// Steps that take arguments; every other step takes none.
const TAKES_ARGS: &[&str] = &["dist", "repro"];

type Step = Result<(), String>;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(step) = args.first() else {
        usage();
        return ExitCode::from(2);
    };
    let root = workspace_root();
    let rest = &args[1..];
    if !rest.is_empty() && !TAKES_ARGS.contains(&step.as_str()) {
        eprintln!("xtask: {step} takes no arguments");
        usage();
        return ExitCode::from(2);
    }
    let result = if step == "ci" {
        CI.iter().try_for_each(|s| run_step(&root, s, &[]))
    } else if STEPS.iter().any(|(name, _)| name == step) {
        run_step(&root, step, rest)
    } else {
        usage();
        return ExitCode::from(2);
    };
    match result {
        Ok(()) => {
            eprintln!("xtask: {step}: ok");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("xtask: FAILED: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage() {
    eprintln!("usage: cargo xtask <step> [args]\n\nsteps:");
    for (name, what) in STEPS {
        eprintln!("  {name:<14} {what}");
    }
}

fn run_step(root: &Path, step: &str, args: &[String]) -> Step {
    eprintln!("xtask: ==> {step}");
    let r = match step {
        "fmt" => cargo(root, &["fmt", "--all", "--check"]),
        "clippy" => cargo(
            root,
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
        ),
        "msrv" => msrv(root),
        "macos" => macos(root),
        "test" => tests(root, &["test", "--workspace", "--locked"]),
        "test-release" => tests(root, &["test", "--workspace", "--locked", "--release"]),
        "unprivileged" => unprivileged(root),
        "doc" => run(Command::new(cargo_bin())
            .args(["doc", "--workspace", "--no-deps", "--locked"])
            .env("RUSTDOCFLAGS", "-D warnings")
            .current_dir(root)),
        "package" => package(root),
        "gnu" => run(Command::new(root.join("util/gnu-tests.sh")).current_dir(root)),
        "unsafe-audit" => unsafe_audit::audit(root).map(|summary| eprintln!("xtask: {summary}")),
        "deny" => deny(root),
        "asan" => asan(root),
        "dist" => dist::target_arg(args).and_then(|t| {
            let epoch = dist::source_date_epoch(root)?;
            dist::dist(root, &t, epoch).map(drop)
        }),
        "repro" => dist::target_arg(args).and_then(|t| dist::repro(root, &t)),
        _ => unreachable!("unknown step {step}"),
    };
    r.map_err(|e| format!("{step}: {e}"))
}

/// `cargo deny check`, with the advisory database fetched fresh.
fn deny(root: &Path) -> Step {
    let installed = Command::new(cargo_bin())
        .args(["deny", "--version"])
        .output()
        .ok()
        .filter(|o| o.status.success());
    let Some(version) = installed else {
        return Err(format!(
            "cargo-deny is not installed: `cargo install --locked cargo-deny@{CARGO_DENY_VERSION}`"
        ));
    };
    let version = String::from_utf8_lossy(&version.stdout);
    if !version.contains(CARGO_DENY_VERSION) {
        eprintln!(
            "xtask: deny: CI uses cargo-deny {CARGO_DENY_VERSION}; this is {}",
            version.trim()
        );
    }
    cargo(root, &["deny", "--locked", "check"])
}

/// Every test binary, built with AddressSanitizer, std included
/// (`-Zbuild-std`, so std's own code is instrumented too). There are no
/// doctests, so `--lib --bins --tests` is everything.
fn asan(root: &Path) -> Step {
    let target = dist::host_target()?;
    // The rustup proxy, not $CARGO: only the proxy understands `+toolchain`.
    run(Command::new("cargo")
        .arg(format!("+{ASAN_TOOLCHAIN}"))
        .args(["test", "-Zbuild-std", "--target", &target])
        .args(["--workspace", "--locked", "--lib", "--bins", "--tests"])
        .arg("--target-dir")
        .arg(root.join("target/asan"))
        .env("RUSTFLAGS", "-Zsanitizer=address")
        .env("RUSTDOCFLAGS", "-Zsanitizer=address")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .current_dir(root))
    .map_err(|e| {
        format!(
            "{e} (missing toolchain? `rustup toolchain install {ASAN_TOOLCHAIN} --component rust-src`)"
        )
    })
}

fn cargo_bin() -> String {
    env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

/// Runs the test suites in the locale GitHub's runners have (`C.UTF-8`) unless
/// the caller chose one: messages quoted by locale differ between C and UTF-8,
/// and a test that only passes in one of them should fail here first.
fn tests(root: &Path, args: &[&str]) -> Step {
    let mut cmd = Command::new(cargo_bin());
    cmd.args(args).current_dir(root);
    if ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .all(|v| env::var_os(v).is_none())
    {
        cmd.env("LANG", "C.UTF-8");
    }
    run(&mut cmd)
}

fn cargo(root: &Path, args: &[&str]) -> Step {
    run(Command::new(cargo_bin()).args(args).current_dir(root))
}

fn run(cmd: &mut Command) -> Step {
    let status = cmd
        .status()
        .map_err(|e| format!("cannot run {:?}: {e}", cmd.get_program()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`{}` exited with {status}", describe(cmd)))
    }
}

fn describe(cmd: &Command) -> String {
    let mut s = cmd.get_program().to_string_lossy().into_owned();
    for a in cmd.get_args() {
        s.push(' ');
        s.push_str(&a.to_string_lossy());
    }
    s
}

fn output(cmd: &mut Command) -> Result<String, String> {
    let out = cmd
        .output()
        .map_err(|e| format!("cannot run {:?}: {e}", cmd.get_program()))?;
    if !out.status.success() {
        return Err(format!(
            "`{}` exited with {}: {}",
            describe(cmd),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn workspace_root() -> PathBuf {
    // crates/xtask -> the workspace root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("xtask lives two levels below the workspace root")
        .to_path_buf()
}

/// `rust-version` from the workspace manifest.
fn rust_version(root: &Path) -> Result<String, String> {
    let manifest = fs::read_to_string(root.join("Cargo.toml")).map_err(|e| e.to_string())?;
    manifest
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == "rust-version").then(|| v.trim().trim_matches('"').to_string())
        })
        .ok_or_else(|| "no rust-version in Cargo.toml".into())
}

fn msrv(root: &Path) -> Step {
    let version = rust_version(root)?;
    // In CI the MSRV job runs on that toolchain already (RUSTUP_TOOLCHAIN).
    let toolchain = env::var("RUSTUP_TOOLCHAIN").unwrap_or_default();
    let mut cmd = Command::new("cargo");
    if !toolchain.starts_with(&version) {
        cmd.arg(format!("+{version}"));
    }
    cmd.args(["check", "--workspace", "--all-targets", "--locked"])
        .current_dir(root);
    run(&mut cmd)
        .map_err(|e| format!("{e} (missing toolchain? `rustup toolchain install {version}`)"))
}

/// Restores a file's original contents when dropped, however the step ends.
struct Restore(PathBuf, Vec<u8>);

impl Drop for Restore {
    fn drop(&mut self) {
        if let Err(e) = fs::write(&self.0, &self.1) {
            eprintln!("xtask: could not restore {}: {e}", self.0.display());
        }
    }
}

fn macos(root: &Path) -> Step {
    if cfg!(target_os = "macos") {
        eprintln!("xtask: macos: skipped on macOS (clippy already checks this target)");
        return Ok(());
    }
    let target = "aarch64-apple-darwin";
    // ring's build script needs an Apple SDK, so a type-only stub stands in
    // for it. Patching rewrites Cargo.lock; put it back afterwards.
    let lock = root.join("Cargo.lock");
    let _restore = Restore(lock.clone(), fs::read(&lock).map_err(|e| e.to_string())?);
    let stub = root.join("crates/xtask/ring-stub");
    let patch = format!("patch.crates-io.ring.path={:?}", stub.display().to_string());
    run(Command::new(cargo_bin())
        .args(["clippy", "--target", target, "--workspace", "--all-targets"])
        .args(["--config", &patch])
        .arg("--target-dir")
        .arg(root.join("target/xtask-macos"))
        .args(["--", "-D", "warnings"])
        .current_dir(root))
    .map_err(|e| format!("{e} (missing target? `rustup target add {target}`)"))
}

/// `cargo package` verifies the packaged crates against each other through a
/// local registry, and cargo never rebuilds a registry crate whose version is
/// unchanged, so a reused target directory verifies against stale builds of
/// `dogfish-core`. A fresh one, as on a CI runner, cannot.
fn package(root: &Path) -> Step {
    let dir = env::temp_dir().join(format!("xtask-package-{}", std::process::id()));
    let r = run(Command::new(cargo_bin())
        .args([
            "package",
            "--workspace",
            "--locked",
            "--allow-dirty",
            "--target-dir",
        ])
        .arg(&dir)
        .current_dir(root));
    let _ = fs::remove_dir_all(&dir);
    r
}

/// The unprivileged user the tests run as.
const NOBODY: &str = "65534";

/// True when the caller is root, so the suites need a second, unprivileged run
/// (CI runners are not root, and many fixtures behave differently as root).
fn is_root() -> Result<bool, String> {
    Ok(output(Command::new("id").arg("-u"))?.trim() == "0")
}

fn unprivileged(root: &Path) -> Step {
    if !is_root()? {
        eprintln!("xtask: unprivileged: skipped, `test` already ran unprivileged");
        return Ok(());
    }
    let json = output(
        Command::new(cargo_bin())
            .args([
                "test",
                "--workspace",
                "--locked",
                "--no-run",
                "--message-format=json",
            ])
            .current_dir(root),
    )?;
    let binaries = test_binaries(&json);
    if binaries.is_empty() {
        return Err("cargo listed no test binaries".into());
    }
    // Fixtures live in target/tmp (CARGO_TARGET_TMPDIR); give that to nobody,
    // and let it reach the binaries.
    let tmp = root.join("target/tmp");
    fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    run(Command::new("chown")
        .args(["-R", &format!("{NOBODY}:{NOBODY}")])
        .arg(&tmp))?;
    for dir in ["target", "target/debug", "target/debug/deps"] {
        run(Command::new("chmod").arg("a+rx").arg(root.join(dir)))?;
    }
    let home = env::temp_dir().join(format!("xtask-nobody-{}", std::process::id()));
    fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    run(Command::new("chmod").arg("0777").arg(&home))?;
    let result = binaries.iter().try_for_each(|(crate_dir, exe)| {
        run(Command::new("setpriv")
            .args([
                &format!("--reuid={NOBODY}"),
                &format!("--regid={NOBODY}"),
                "--clear-groups",
            ])
            .arg(exe)
            .arg("-q")
            .env("HOME", &home)
            .env("TMPDIR", &home)
            // cargo runs each test binary from its package's directory.
            .current_dir(crate_dir))
    });
    let _ = fs::remove_dir_all(&home);
    result
}

/// `(package directory, executable)` for each test binary in cargo's JSON
/// messages. Paths are taken verbatim; cargo escapes only `"` and `\`, which
/// no path here contains.
fn test_binaries(json: &str) -> Vec<(PathBuf, PathBuf)> {
    json.lines()
        .filter(|l| l.contains(r#""reason":"compiler-artifact""#) && is_test_profile(l))
        .filter_map(|l| {
            let exe = string_field(l, "executable")?;
            let manifest = string_field(l, "manifest_path")?;
            Some((
                Path::new(&manifest).parent()?.to_path_buf(),
                PathBuf::from(exe),
            ))
        })
        .collect()
}

/// The artifact was built with the test profile. A plain binary's *target*
/// also says `"test":true` (it can be tested), so only `profile` counts.
fn is_test_profile(line: &str) -> bool {
    line.split_once(r#""profile":{"#)
        .and_then(|(_, rest)| rest.split_once('}'))
        .is_some_and(|(profile, _)| profile.contains(r#""test":true"#))
}

fn string_field(line: &str, key: &str) -> Option<String> {
    let start = line.find(&format!(r#""{key}":""#))? + key.len() + 4;
    let len = line[start..].find('"')?;
    Some(line[start..start + len].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_test_binaries_in_cargo_messages() {
        let json = concat!(
            r#"{"reason":"compiler-artifact","manifest_path":"/w/crates/a/Cargo.toml","profile":{"test":true},"executable":"/w/target/debug/deps/a-1"}"#,
            "\n",
            r#"{"reason":"compiler-artifact","manifest_path":"/w/crates/a/Cargo.toml","target":{"kind":["bin"],"test":true},"profile":{"opt_level":"0","test":false},"executable":"/w/target/debug/a"}"#,
            "\n",
            r#"{"reason":"compiler-artifact","manifest_path":"/w/crates/b/Cargo.toml","profile":{"test":true},"executable":null}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
        );
        assert_eq!(
            test_binaries(json),
            vec![(
                PathBuf::from("/w/crates/a"),
                PathBuf::from("/w/target/debug/deps/a-1")
            )]
        );
    }

    #[test]
    fn ci_runs_only_known_quick_steps() {
        for step in CI {
            assert!(STEPS.iter().any(|(name, _)| name == step), "{step}");
        }
        // Slow, or needing tools and network access a contributor may not have.
        for step in ["gnu", "deny", "asan", "dist", "repro"] {
            assert!(!CI.contains(&step), "{step}");
        }
        for step in TAKES_ARGS {
            assert!(STEPS.iter().any(|(name, _)| name == step), "{step}");
        }
    }

    #[test]
    fn reads_rust_version() {
        assert!(rust_version(&workspace_root()).unwrap().starts_with("1."));
    }
}
