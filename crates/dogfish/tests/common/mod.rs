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
    if let Some(known) = cache.lock().unwrap().get(tool) {
        return known.clone();
    }
    // Probe without holding the lock: one slow probe must not stall every test.
    let found = gnu_path(tool).filter(|path| is_gnu_coreutils(path));
    cache
        .lock()
        .unwrap()
        .entry(tool.to_string())
        .or_insert(found)
        .clone()
}

/// Runs `path --version` and checks for GNU's banner, reading only the first few
/// KiB: BSD `yes` (macOS) treats `--version` as the text to repeat and never stops.
fn is_gnu_coreutils(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut child) = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let mut banner = Vec::new();
    let _ = child
        .stdout
        .take()
        .unwrap()
        .take(4096)
        .read_to_end(&mut banner);
    let _ = child.kill();
    let _ = child.wait();
    String::from_utf8_lossy(&banner).contains("GNU coreutils")
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
    let what = format!("{cmd:?}");
    let mut child = cmd.spawn().expect("spawn");
    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().unwrap();
        let input = input.to_vec();
        // Write from a thread so large outputs can't deadlock us.
        std::thread::spawn(move || {
            let _ = pipe.write_all(&input);
        });
    }
    wait_bounded(child, &what)
}

/// `wait_with_output`, but a command that hangs or floods its output fails the
/// test with its command line instead of stalling (or OOM-killing) the whole run.
fn wait_bounded(mut child: std::process::Child, what: &str) -> Output {
    use std::io::Read;
    use std::sync::mpsc;
    use std::time::Duration;

    const MAX_OUTPUT: u64 = 64 << 20;
    const DEADLINE: Duration = Duration::from_secs(60);

    // Each reader stops (closing its end, so the child gets SIGPIPE) past the cap.
    fn drain(pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.take(MAX_OUTPUT + 1).read_to_end(&mut buf);
            buf
        })
    }
    let stdout = drain(child.stdout.take().unwrap());
    let stderr = drain(child.stderr.take().unwrap());

    let pid = child.id() as libc::pid_t;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait());
    });
    let status = match rx.recv_timeout(DEADLINE) {
        Ok(status) => status.expect("wait"),
        Err(_) => {
            // SAFETY: signalling our own child, which has not been reaped yet.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
            panic!("{what} did not finish within {DEADLINE:?}");
        }
    };
    let (stdout, stderr) = (stdout.join().unwrap(), stderr.join().unwrap());
    for (name, buf) in [("stdout", &stdout), ("stderr", &stderr)] {
        assert!(
            buf.len() as u64 <= MAX_OUTPUT,
            "{what} wrote more than {MAX_OUTPUT} bytes to {name}"
        );
    }
    Output {
        status,
        stdout,
        stderr,
    }
}

pub struct Case<'a> {
    pub tool: &'a str,
    pub args: &'a [&'a str],
    pub stdin: Option<&'a [u8]>,
    pub compare_stderr: bool,
    pub locale: &'a str,
    /// Extra environment variables (e.g. a fixed `TZ`) set for both runs.
    pub env: &'a [(&'a str, &'a str)],
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

/// Like [`check`], but only builds the fixtures once the case is known to run, so a
/// platform without GNU coreutils (e.g. macOS) never creates them — some fixtures
/// (non-UTF-8 names, say) are not even representable on every filesystem.
pub fn check_lazy(fixtures: impl FnOnce() -> &'static Path, case: Case<'_>) {
    if !will_run(&case) {
        return;
    }
    check(fixtures(), case);
}

/// Whether `case` can be compared here; prints why not when it can't.
fn will_run(case: &Case<'_>) -> bool {
    if gnu_tool(case.tool).is_none() {
        eprintln!(
            "note: skipping — system `{}` is missing or not GNU coreutils",
            case.tool
        );
        return false;
    }
    if case.locale != "C" && !locale_is_utf8(case.locale) {
        eprintln!("note: skipping — locale `{}` is unavailable", case.locale);
        return false;
    }
    true
}

/// A case argument as passed: `@xff@` in it stands for the byte 0xff, so
/// that cases can name files that are not valid UTF-8.
pub fn case_arg(arg: &str) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    let mut out = Vec::with_capacity(arg.len());
    let mut rest = arg.as_bytes();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix(b"@xff@") {
            out.push(0xff);
            rest = after;
        } else {
            out.push(rest[0]);
            rest = &rest[1..];
        }
    }
    std::ffi::OsString::from_vec(out)
}

/// Runs one case in `dir` through both implementations and asserts they match.
pub fn check(dir: &Path, case: Case<'_>) {
    if !will_run(&case) {
        return;
    }
    let gnu = gnu_tool(case.tool).expect("checked by will_run");

    let args: Vec<_> = case.args.iter().map(|a| case_arg(a)).collect();
    let mut ours = Command::new(dogfish_bin());
    ours.arg(case.tool)
        .args(&args)
        .envs(case.env.iter().copied());
    let mut theirs = Command::new(&gnu);
    // GNU prints argv[0] in diagnostics; make it the bare tool name.
    theirs
        .arg0(case.tool)
        .args(&args)
        .envs(case.env.iter().copied());

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
        "\ncase: {:?} {} {:?} (stdin: {:?})\n=== dogfish ===\n{}\n=== GNU ===\n{}\n",
        case.env,
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

/// `name: tool [args...] (@ "LOCALE")? (env [("VAR", "value"), ...])? (stdin b"...")? (=> nostderr)?;`
///
/// Cases run under `LC_ALL=C` unless a locale is given; `env` adds variables
/// (such as a fixed `TZ`) to both runs. Dogfish is always
/// UTF-8 aware, so multibyte cases (`wc -m`, `wc -L`) compare against GNU in
/// `C.UTF-8`.
#[macro_export]
macro_rules! gnu_cases {
    ($(
        $(#[$meta:meta])*
        $name:ident : $tool:literal [$($arg:expr),* $(,)?]
            $(@ $locale:literal)?
            $(env [$(($var:literal, $val:expr)),* $(,)?])?
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
                $crate::common::check_lazy(fixtures, $crate::common::Case {
                    tool: $tool,
                    args: &[$($arg),*],
                    stdin,
                    compare_stderr,
                    locale,
                    env: &[$($(($var, $val)),*)?],
                });
            }
        )*
    };
}
