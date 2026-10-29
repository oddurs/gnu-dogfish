//! Numeric and boundary edge cases that must neither panic (debug) nor wrap (release).

use std::process::Command;

fn run(args: &[&str]) -> (String, Option<i32>) {
    let out = Command::new(env!("CARGO_BIN_EXE_dogfish"))
        .args(args)
        .output()
        .expect("failed to execute dogfish");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.code(),
    )
}

#[test]
fn seq_stops_at_i64_bounds_without_overflow() {
    let (out, code) = run(&["seq", "9223372036854775805", "9223372036854775807"]);
    assert_eq!(code, Some(0));
    assert_eq!(
        out,
        "9223372036854775805\n9223372036854775806\n9223372036854775807\n"
    );

    let (out, code) = run(&["seq", "-9223372036854775807", "-1", "-9223372036854775808"]);
    assert_eq!(code, Some(0));
    assert_eq!(out, "-9223372036854775807\n-9223372036854775808\n");
}

#[test]
fn seq_large_integers_keep_full_precision() {
    let (out, _) = run(&["seq", "9007199254740993", "9007199254740994"]);
    assert_eq!(out, "9007199254740993\n9007199254740994\n");
}

#[test]
fn sigbus_from_truncated_mmap_is_a_clean_error() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;
    // A mapped input truncated by another process raises SIGBUS; deliver one directly
    // so the test is deterministic, once `cat` has mapped the file and waits on
    // standard input.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("mapped");
    // Larger than the 16 KiB below which files are read, not mapped.
    let data = b"mapped\n".repeat(4096);
    std::fs::write(&file, &data).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_dogfish"))
        .arg("cat")
        .arg(&file)
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    // SAFETY: signalling our own child, which has not been waited for yet,
    // so its pid cannot have been reused.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGBUS);
    }
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "cat: input file was truncated while being read\n"
    );

    // A tool that maps nothing keeps the default action, as GNU's do (and
    // pays no system call for the handler).
    let child = Command::new(env!("CARGO_BIN_EXE_dogfish"))
        .args(["sleep", "10"])
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    // SAFETY: as above.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGBUS);
    }
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.signal(), Some(libc::SIGBUS));
}

#[test]
fn every_tool_answers_version() {
    for tool in ["ls", "cp", "sort", "wc", "echo", "yes", "true", "sha256sum"] {
        let (out, code) = run(&[tool, "--version"]);
        assert_eq!(code, Some(0), "{tool}");
        assert_eq!(
            out,
            format!("{tool} (GNU/Dogfish) {}\n", env!("CARGO_PKG_VERSION"))
        );
    }
    // Like GNU, `false` still fails.
    assert_eq!(run(&["false", "--version"]).1, Some(1));
    // Only a lone `--version` is special; elsewhere it is ordinary data.
    assert_eq!(run(&["echo", "a", "--version"]).0, "a --version\n");
}
