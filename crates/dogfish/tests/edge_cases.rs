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
    // A mapped input truncated by another process raises SIGBUS; deliver one directly
    // so the test is deterministic.
    let child = Command::new(env!("CARGO_BIN_EXE_dogfish"))
        .args(["sleep", "10"])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGBUS);
    }
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "sleep: input file was truncated while being read\n"
    );
}
