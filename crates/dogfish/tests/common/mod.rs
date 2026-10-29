//! Shared harness for the differential GNU-compatibility suites
//! (`gnu_compat*.rs`).
//!
//! Every case runs the same arguments (and stdin) through `dogfish <tool>` and
//! through the system GNU coreutils binary, inside a fixture directory, then
//! compares stdout, exit status and (unless a case opts out) stderr byte-for-byte.
//! When the system tool is missing or is not GNU coreutils (e.g. BSD tools on
//! macOS), the case prints a note and passes.
//!
//! Each suite defines its own `fn fixtures() -> &'static Path` (usually built
//! once with [`fresh_fixture_dir`]); the [`gnu_cases!`] macro calls the
//! `fixtures` in scope where it is expanded.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, OnceLock};

pub fn dogfish_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dogfish")
}

/// Creates an empty directory under `target/tmp` (cleaned by `cargo clean`),
/// unique to the calling test binary, replacing any leftover from a previous run.
pub fn fresh_fixture_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("{}-fixtures", env!("CARGO_CRATE_NAME")));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes `(relative path, contents)` pairs under `dir`, creating parents.
pub fn write_files(dir: &Path, files: &[(&str, &[u8])]) {
    for (name, data) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, data).unwrap();
    }
}

pub fn gnu_path(tool: &str) -> Option<PathBuf> {
    ["/usr/bin", "/bin", "/usr/local/bin"]
        .iter()
        .map(|d| Path::new(d).join(tool))
        .find(|p| p.is_file())
}

/// Returns the path of the system tool if it identifies as GNU coreutils.
pub fn gnu_tool(tool: &str) -> Option<PathBuf> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<PathBuf>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().unwrap();
    guard
        .entry(tool.to_string())
        .or_insert_with(|| {
            let path = gnu_path(tool)?;
            let out = Command::new(&path).arg("--version").output().ok()?;
            String::from_utf8_lossy(&out.stdout)
                .contains("GNU coreutils")
                .then_some(path)
        })
        .clone()
}

/// Runs `cmd` in `dir` with a pinned, colorless locale environment.
pub fn exec(mut cmd: Command, dir: &Path, stdin: Option<&[u8]>, locale: &str) -> Output {
    cmd.current_dir(dir)
        .env("LC_ALL", locale)
        .env("LANG", "C")
        .env("NO_COLOR", "1")
        .env_remove("POSIXLY_CORRECT")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn");
    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().unwrap();
        let input = input.to_vec();
        // Write from a thread so large outputs can't deadlock us.
        std::thread::spawn(move || {
            let _ = pipe.write_all(&input);
        });
    }
    child.wait_with_output().expect("wait")
}

pub struct Case<'a> {
    pub tool: &'a str,
    pub args: &'a [&'a str],
    pub stdin: Option<&'a [u8]>,
    pub compare_stderr: bool,
    pub locale: &'a str,
}

/// True when the system C library actually provides `locale` with UTF-8
/// semantics (GNU tools silently fall back to "C" otherwise).
pub fn locale_is_utf8(locale: &str) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().unwrap();
    *guard.entry(locale.to_string()).or_insert_with(|| {
        let Some(wc) = gnu_tool("wc") else {
            return false;
        };
        let mut cmd = Command::new(wc);
        cmd.arg("-m");
        let out = exec(
            cmd,
            &std::env::temp_dir(),
            Some("\u{e9}".as_bytes()),
            locale,
        );
        String::from_utf8_lossy(&out.stdout).trim() == "1"
    })
}

/// Runs one case in `dir` through both implementations and asserts they match.
pub fn check(dir: &Path, case: Case<'_>) {
    let Some(gnu) = gnu_tool(case.tool) else {
        eprintln!(
            "note: skipping — system `{}` is missing or not GNU coreutils",
            case.tool
        );
        return;
    };
    if case.locale != "C" && !locale_is_utf8(case.locale) {
        eprintln!("note: skipping — locale `{}` is unavailable", case.locale);
        return;
    }

    let mut ours = Command::new(dogfish_bin());
    ours.arg(case.tool).args(case.args);
    let mut theirs = Command::new(&gnu);
    // GNU prints argv[0] in diagnostics; make it the bare tool name.
    theirs.arg0(case.tool).args(case.args);

    let a = exec(ours, dir, case.stdin, case.locale);
    let b = exec(theirs, dir, case.stdin, case.locale);

    let show = |o: &Output| {
        format!(
            "status={:?}\n--- stdout ---\n{:?}\n--- stderr ---\n{:?}",
            o.status.code(),
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        )
    };
    let ctx = format!(
        "\ncase: {} {:?} (stdin: {:?})\n=== dogfish ===\n{}\n=== GNU ===\n{}\n",
        case.tool,
        case.args,
        case.stdin.map(String::from_utf8_lossy),
        show(&a),
        show(&b)
    );
    assert_eq!(a.stdout, b.stdout, "stdout differs{ctx}");
    assert_eq!(a.status.code(), b.status.code(), "exit status differs{ctx}");
    if case.compare_stderr {
        assert_eq!(a.stderr, b.stderr, "stderr differs{ctx}");
    }
}

/// `name: tool [args...] (@ "LOCALE")? (stdin b"...")? (=> nostderr)?;`
///
/// Cases run under `LC_ALL=C` unless a locale is given. Dogfish is always
/// UTF-8 aware, so multibyte cases (`wc -m`, `wc -L`) compare against GNU in
/// `C.UTF-8`.
#[macro_export]
macro_rules! gnu_cases {
    ($(
        $(#[$meta:meta])*
        $name:ident : $tool:literal [$($arg:expr),* $(,)?]
            $(@ $locale:literal)?
            $(stdin $input:expr)?
            $(=> $nostderr:ident)?
        ;
    )*) => {
        $(
            $(#[$meta])*
            #[test]
            fn $name() {
                #[allow(unused_mut, unused_assignments)]
                let mut stdin: Option<&[u8]> = None;
                $( stdin = Some($input); )?
                #[allow(unused_mut, unused_assignments)]
                let mut locale = "C";
                $( locale = $locale; )?
                #[allow(unused_mut, unused_assignments)]
                let mut compare_stderr = true;
                $( let _ = stringify!($nostderr); compare_stderr = false; )?
                $crate::common::check(fixtures(), $crate::common::Case {
                    tool: $tool,
                    args: &[$($arg),*],
                    stdin,
                    compare_stderr,
                    locale,
                });
            }
        )*
    };
}
