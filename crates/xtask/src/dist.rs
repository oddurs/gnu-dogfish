//! `cargo xtask dist` builds a release archive reproducibly; `cargo xtask
//! repro` builds it twice, from two copies of the tree in different
//! directories, and fails unless the binaries and archives are identical.
//!
//! What makes the build reproducible, given the same source, toolchain and
//! target:
//! - `--remap-path-prefix` maps the workspace and `$CARGO_HOME` to fixed
//!   names (`/gnu-dogfish`, `/cargo`), so no build-machine path reaches the
//!   binary (panic locations and the like embed source paths);
//! - the release profile already strips symbols, uses one codegen unit and
//!   no incremental compilation;
//! - `SOURCE_DATE_EPOCH` (the commit time) is the only timestamp: the archive
//!   is written here, entries sorted by name, owned by 0:0, with fixed modes
//!   and that mtime, then compressed with `gzip -n` (no name or time stamp).

use crate::sha256;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Files shipped next to the binary, from the workspace root. The archive
/// also holds `share/`: the manual pages and completions (`docs_gen`).
const DOCS: &[&str] = &[
    "CHANGELOG.md",
    "LICENSE",
    "LICENSE-GPL",
    "LICENSE-MIT",
    "README.md",
];

/// What `dist` produced.
pub struct Dist {
    pub binary: PathBuf,
    pub archive: PathBuf,
}

/// The archive name for a target triple; the two release targets keep the
/// names earlier releases used.
pub fn archive_name(target: &str) -> String {
    match target {
        "x86_64-unknown-linux-gnu" => "gnu-dogfish-linux-x86_64.tar.gz".into(),
        "aarch64-unknown-linux-gnu" => "gnu-dogfish-linux-arm64.tar.gz".into(),
        "aarch64-apple-darwin" => "gnu-dogfish-macos-arm64.tar.gz".into(),
        "x86_64-apple-darwin" => "gnu-dogfish-macos-x86_64.tar.gz".into(),
        other => format!("gnu-dogfish-{other}.tar.gz"),
    }
}

/// `--target T` from the step's arguments, or the host triple.
pub fn target_arg(args: &[String]) -> Result<String, String> {
    match args {
        [] => host_target(),
        [flag, t] if flag == "--target" => Ok(t.clone()),
        _ => Err(format!(
            "unexpected arguments {args:?} (usage: --target <triple>)"
        )),
    }
}

pub fn host_target() -> Result<String, String> {
    let out = crate::output(Command::new("rustc").arg("-vV"))?;
    out.lines()
        .find_map(|l| l.strip_prefix("host: "))
        .map(str::to_string)
        .ok_or_else(|| "`rustc -vV` printed no host".into())
}

/// `SOURCE_DATE_EPOCH` if set, else the commit time of HEAD.
pub fn source_date_epoch(root: &Path) -> Result<u64, String> {
    if let Ok(v) = env::var("SOURCE_DATE_EPOCH") {
        return v
            .trim()
            .parse()
            .map_err(|_| format!("SOURCE_DATE_EPOCH={v:?} is not a number of seconds"));
    }
    let out = crate::output(
        Command::new("git")
            .args(["log", "-1", "--format=%ct", "HEAD"])
            .current_dir(root),
    )
    .map_err(|e| format!("{e} (outside a git checkout, set SOURCE_DATE_EPOCH)"))?;
    out.trim()
        .parse()
        .map_err(|_| format!("git printed {out:?} for the commit time"))
}

fn cargo_home() -> Result<PathBuf, String> {
    if let Some(h) = env::var_os("CARGO_HOME") {
        return Ok(PathBuf::from(h));
    }
    env::var_os("HOME")
        .map(|h| Path::new(&h).join(".cargo"))
        .ok_or_else(|| "neither CARGO_HOME nor HOME is set".into())
}

/// The `-C`/`--remap-path-prefix` flags for a build of the tree at `root`.
fn rustflags(root: &Path) -> Result<Vec<String>, String> {
    let home = cargo_home()?;
    let mut maps = vec![
        (root.to_path_buf(), "/gnu-dogfish"),
        (home.clone(), "/cargo"),
    ];
    // Cargo may name either spelling of a path that crosses a symlink.
    for (path, to) in [(root.to_path_buf(), "/gnu-dogfish"), (home, "/cargo")] {
        if let Ok(real) = path.canonicalize()
            && real != path
        {
            maps.push((real, to));
        }
    }
    Ok(maps
        .iter()
        .map(|(from, to)| format!("--remap-path-prefix={}={to}", from.display()))
        .collect())
}

/// Builds `dogfish` for `target` from the tree at `root` and writes
/// `target/dist/out/<archive>` and `<archive>.sha256`.
pub fn dist(root: &Path, target: &str, epoch: u64) -> Result<Dist, String> {
    let target_dir = root.join("target/dist");
    let flags = rustflags(root)?;
    eprintln!("xtask: dist: {target}, SOURCE_DATE_EPOCH={epoch}");
    crate::run(
        Command::new(crate::cargo_bin())
            .args(["build", "--release", "--locked", "--package", "dogfish"])
            .args(["--bin", "dogfish", "--target", target])
            .arg("--target-dir")
            .arg(&target_dir)
            // CARGO_ENCODED_RUSTFLAGS wins over RUSTFLAGS and over
            // build.rustflags, so nothing from the environment leaks in.
            .env("CARGO_ENCODED_RUSTFLAGS", flags.join("\x1f"))
            .env_remove("RUSTFLAGS")
            .env("SOURCE_DATE_EPOCH", epoch.to_string())
            .current_dir(root),
    )?;
    let binary = target_dir.join(target).join("release/dogfish");

    let mut entries = vec![(
        "dogfish".to_string(),
        fs::read(&binary).map_err(|e| format!("{}: {e}", binary.display()))?,
        0o755,
    )];
    for doc in DOCS {
        let path = root.join(doc);
        let data = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        entries.push((doc.to_string(), data, 0o644));
    }
    // share/man/man1 and the completions, generated by a dogfish that runs
    // here: the one just built, or one built for this machine.
    let generator = if target == host_target()? {
        binary.clone()
    } else {
        crate::run(
            Command::new(crate::cargo_bin())
                .args(["build", "--release", "--locked", "--package", "dogfish"])
                .args(["--bin", "dogfish", "--target-dir"])
                .arg(&target_dir)
                .current_dir(root),
        )?;
        target_dir.join("release/dogfish")
    };
    for (path, data) in crate::docs_gen::generate(&generator, epoch)? {
        entries.push((path, data, 0o644));
    }
    let tar = tar(entries, epoch)?;

    let out_dir = target_dir.join("out");
    let _ = fs::remove_dir_all(&out_dir);
    fs::create_dir_all(&out_dir).map_err(|e| format!("{}: {e}", out_dir.display()))?;
    let name = archive_name(target);
    let archive = out_dir.join(&name);
    let gz = gzip(&tar)?;
    fs::write(&archive, &gz).map_err(|e| format!("{}: {e}", archive.display()))?;
    // The format of `sha256sum`, as earlier releases shipped it.
    let sum = format!("{}  {name}\n", sha256::hex(&gz));
    let sum_path = out_dir.join(format!("{name}.sha256"));
    fs::write(&sum_path, sum).map_err(|e| format!("{}: {e}", sum_path.display()))?;
    eprintln!("xtask: dist: wrote {}", archive.display());
    Ok(Dist { binary, archive })
}

/// A ustar archive of `entries` (name, contents, mode), sorted by name, every
/// entry owned by 0:0 with mtime `epoch`.
fn tar(mut entries: Vec<(String, Vec<u8>, u32)>, epoch: u64) -> Result<Vec<u8>, String> {
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    for (name, data, mode) in &entries {
        if name.len() > 100 {
            return Err(format!("{name}: name too long for a ustar header"));
        }
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        octal(&mut h[100..108], u64::from(*mode));
        octal(&mut h[108..116], 0); // uid
        octal(&mut h[116..124], 0); // gid
        octal(&mut h[124..136], data.len() as u64);
        octal(&mut h[136..148], epoch);
        h[156] = b'0'; // regular file
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        octal(&mut h[329..337], 0); // device major
        octal(&mut h[337..345], 0); // device minor
        // The checksum is computed with its own field as eight spaces.
        h[148..156].fill(b' ');
        let sum: u64 = h.iter().map(|&b| u64::from(b)).sum();
        octal(&mut h[148..155], sum);
        h[155] = b' ';
        out.extend_from_slice(&h);
        out.extend_from_slice(data);
        out.resize(out.len().next_multiple_of(512), 0);
    }
    // Two zero blocks end the archive; pad to tar's 10 KiB record size.
    out.resize(out.len() + 1024, 0);
    out.resize(out.len().next_multiple_of(10240), 0);
    Ok(out)
}

/// Writes `value` as zero-padded octal digits filling all but the last byte
/// of `field`, then a NUL.
fn octal(field: &mut [u8], value: u64) {
    let digits = field.len() - 1;
    let s = format!("{value:0digits$o}");
    field[..digits].copy_from_slice(&s.as_bytes()[s.len() - digits..]);
    field[digits] = 0;
}

/// `gzip -n -9`: no file name or time stamp in the header.
fn gzip(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut child = Command::new("gzip")
        .args(["-n", "-9", "-c"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run gzip: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("gzip: no stdin")?;
    // Write from another thread so a full stdout pipe cannot deadlock us.
    let input = data.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output().map_err(|e| format!("gzip: {e}"))?;
    writer
        .join()
        .map_err(|_| "gzip: writer thread panicked")?
        .map_err(|e| format!("gzip: {e}"))?;
    if !out.status.success() {
        return Err(format!("gzip exited with {}", out.status));
    }
    Ok(out.stdout)
}

/// Builds the release archive in two copies of the tree and compares them.
pub fn repro(root: &Path, target: &str) -> Result<(), String> {
    let epoch = source_date_epoch(root)?;
    let files = tracked_files(root)?;
    let base = env::temp_dir().join(format!("xtask-repro-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    // Different depths and names, so a leaked path changes the output.
    let trees = [base.join("a/gnu-dogfish"), base.join("second-build/src")];
    let mut results = Vec::new();
    for tree in &trees {
        copy_files(root, tree, &files)?;
        let tree = tree.canonicalize().map_err(|e| e.to_string())?;
        let d = dist(&tree, target, epoch)?;
        let bin = fs::read(&d.binary).map_err(|e| format!("{}: {e}", d.binary.display()))?;
        let archive = fs::read(&d.archive).map_err(|e| format!("{}: {e}", d.archive.display()))?;
        if let Some(leak) = find_path(&bin, &tree) {
            return Err(format!(
                "{} contains its build path {leak:?}; trees kept in {}",
                d.binary.display(),
                base.display()
            ));
        }
        results.push((tree, sha256::hex(&bin), sha256::hex(&archive)));
    }
    eprintln!("xtask: repro: {target}, SOURCE_DATE_EPOCH={epoch}");
    for (tree, bin, archive) in &results {
        eprintln!("  {}", tree.display());
        eprintln!("    dogfish  {bin}");
        eprintln!("    {}  {archive}", archive_name(target));
    }
    let (a, b) = (&results[0], &results[1]);
    if a.1 != b.1 || a.2 != b.2 {
        return Err(format!(
            "the two builds differ; trees kept in {} (compare them with diffoscope)",
            base.display()
        ));
    }
    let _ = fs::remove_dir_all(&base);
    eprintln!("xtask: repro: identical");
    Ok(())
}

/// The files git tracks, plus untracked files it does not ignore (so local
/// changes are built too).
fn tracked_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let out = Command::new("git")
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err("`git ls-files` failed: repro needs a git checkout".into());
    }
    Ok(out
        .stdout
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| PathBuf::from(String::from_utf8_lossy(p).into_owned()))
        .collect())
}

fn copy_files(root: &Path, dest: &Path, files: &[PathBuf]) -> Result<(), String> {
    for rel in files {
        let (from, to) = (root.join(rel), dest.join(rel));
        // Deleted in the working tree but still in the index.
        let Ok(meta) = fs::symlink_metadata(&from) else {
            continue;
        };
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let r = if meta.file_type().is_symlink() {
            fs::read_link(&from).and_then(|link| std::os::unix::fs::symlink(link, &to))
        } else {
            fs::copy(&from, &to).map(drop)
        };
        r.map_err(|e| format!("copying {} to {}: {e}", from.display(), to.display()))?;
    }
    Ok(())
}

/// The first occurrence of `tree`'s path in `bin`, if any.
fn find_path(bin: &[u8], tree: &Path) -> Option<String> {
    let needle = tree.to_string_lossy();
    bin.windows(needle.len())
        .any(|w| w == needle.as_bytes())
        .then(|| needle.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tar_headers_are_fixed_and_checksummed() {
        let t = tar(
            vec![
                ("b".into(), b"hello".to_vec(), 0o644),
                ("a".into(), vec![], 0o755),
            ],
            1_700_000_000,
        )
        .unwrap();
        assert_eq!(t.len(), 10240);
        // Sorted: "a" (empty) first, then "b" and its one data block.
        assert_eq!(&t[..2], b"a\0");
        assert_eq!(&t[512..514], b"b\0");
        assert_eq!(&t[1024..1029], b"hello");
        let h = &t[512..1024];
        assert_eq!(&h[100..108], b"0000644\0");
        assert_eq!(&h[108..124], b"0000000\x000000000\0");
        assert_eq!(&h[124..136], b"00000000005\0");
        assert_eq!(&h[136..148], b"14524770400\0");
        assert_eq!(&h[257..265], b"ustar\x0000");
        let mut blank = h.to_vec();
        blank[148..156].fill(b' ');
        let sum: u64 = blank.iter().map(|&b| u64::from(b)).sum();
        assert_eq!(&h[148..156], format!("{sum:06o}\0 ").as_bytes());
    }

    #[test]
    fn tar_is_deterministic() {
        let entries = || vec![("x".to_string(), vec![1u8; 700], 0o600)];
        assert_eq!(tar(entries(), 7).unwrap(), tar(entries(), 7).unwrap());
        assert_ne!(tar(entries(), 7).unwrap(), tar(entries(), 8).unwrap());
    }

    #[test]
    fn release_archives_keep_their_names() {
        assert_eq!(
            archive_name("x86_64-unknown-linux-gnu"),
            "gnu-dogfish-linux-x86_64.tar.gz"
        );
        assert_eq!(
            archive_name("aarch64-apple-darwin"),
            "gnu-dogfish-macos-arm64.tar.gz"
        );
    }

    #[test]
    fn parses_the_target_argument() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            target_arg(&args(&["--target", "aarch64-apple-darwin"])).unwrap(),
            "aarch64-apple-darwin"
        );
        assert!(target_arg(&args(&["--target"])).is_err());
        assert!(target_arg(&args(&["x86_64-unknown-linux-gnu"])).is_err());
    }

    #[test]
    fn finds_a_leaked_path() {
        let tree = Path::new("/tmp/build/src");
        assert!(find_path(b"..../tmp/build/src/main.rs..", tree).is_some());
        assert!(find_path(b"/gnu-dogfish/src/main.rs", tree).is_none());
    }
}
