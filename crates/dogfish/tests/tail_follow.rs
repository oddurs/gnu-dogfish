//! `tail -f` / `-F` behaviour tests: each starts a following tail on files
//! in a fresh directory, changes the files (append, rename, truncate,
//! remove, recreate) and waits for GNU's output and diagnostics. They run
//! with inotify and with `---disable-inotify` (polling). Timing makes a
//! byte-for-byte comparison with GNU flaky, so these check Dogfish alone;
//! the deterministic follow cases are in `gnu_compat_text.rs`.

mod common;

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::dogfish_bin;

/// How long to wait for an expected output before failing.
const PATIENCE: Duration = Duration::from_secs(10);

/// A following tail whose standard output and error go to one file.
struct Follower {
    child: Child,
    log: PathBuf,
}

impl Follower {
    fn start(dir: &Path, args: &[&str]) -> Follower {
        let log = dir.join("tail.log");
        let out = File::create(&log).unwrap();
        let err = out.try_clone().unwrap();
        let child = Command::new(dogfish_bin())
            .arg("tail")
            .args(["-s.1", "--max-unchanged-stats=1"])
            .args(args)
            .current_dir(dir)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .spawn()
            .unwrap();
        Follower { child, log }
    }

    fn output(&self) -> String {
        String::from_utf8_lossy(&fs::read(&self.log).unwrap_or_default()).into_owned()
    }

    /// Waits until the output contains `needle`.
    fn expect(&self, needle: &str) {
        let start = Instant::now();
        while !self.output().contains(needle) {
            assert!(
                start.elapsed() < PATIENCE,
                "timed out waiting for {needle:?}; output so far:\n{}",
                self.output()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits for tail to exit; returns its status code.
    fn wait_exit(&mut self) -> Option<i32> {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            assert!(
                start.elapsed() < PATIENCE,
                "tail did not exit; output:\n{}",
                self.output()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("tail_follow")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn append(path: &Path, data: &str) {
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    f.write_all(data.as_bytes()).unwrap();
}

/// Runs `test` with inotify and with polling.
fn both_modes(name: &str, test: impl Fn(&Path, &[&str])) {
    test(&scratch(&format!("{name}-inotify")), &[]);
    test(&scratch(&format!("{name}-poll")), &["---disable-inotify"]);
}

#[test]
fn follow_descriptor_appends_and_survives_rename() {
    both_modes("descriptor-rename", |dir, mode| {
        fs::write(dir.join("a"), "1\n").unwrap();
        let t = Follower::start(dir, &[mode, &["-f", "a"]].concat());
        t.expect("1\n");
        append(&dir.join("a"), "2\n");
        t.expect("1\n2\n");
        fs::rename(dir.join("a"), dir.join("b")).unwrap();
        append(&dir.join("b"), "3\n");
        t.expect("1\n2\n3\n");
    });
}

#[test]
fn follow_truncation_is_reported() {
    both_modes("truncate", |dir, mode| {
        fs::write(dir.join("f"), "1\n2\n3\n").unwrap();
        let t = Follower::start(dir, &[mode, &["-f", "f"]].concat());
        t.expect("3\n");
        fs::write(dir.join("f"), "x\n").unwrap();
        t.expect("tail: f: file truncated\n");
        t.expect("x\n");
    });
}

#[test]
fn follow_name_rotation() {
    both_modes("rotate", |dir, mode| {
        fs::write(dir.join("log"), "old\n").unwrap();
        let t = Follower::start(dir, &[mode, &["-F", "log"]].concat());
        t.expect("old\n");
        fs::rename(dir.join("log"), dir.join("log.1")).unwrap();
        t.expect("tail: 'log' has become inaccessible: No such file or directory\n");
        fs::write(dir.join("log"), "new\n").unwrap();
        t.expect("tail: 'log' has appeared;  following new file\nnew\n");
        // Replaced while present, as `mv` does.
        fs::write(dir.join("next"), "next\n").unwrap();
        fs::rename(dir.join("next"), dir.join("log")).unwrap();
        t.expect("tail: 'log' has been replaced;  following new file\nnext\n");
        append(&dir.join("log"), "more\n");
        t.expect("next\nmore\n");
    });
}

#[test]
fn follow_name_missing_file_appears() {
    both_modes("appears", |dir, mode| {
        let t = Follower::start(dir, &[mode, &["-F", "sub/f"]].concat());
        t.expect("tail: cannot open 'sub/f' for reading: No such file or directory\n");
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/f"), "hi\n").unwrap();
        t.expect("tail: 'sub/f' has appeared;  following new file\nhi\n");
    });
}

#[test]
fn follow_headers_when_switching_files() {
    both_modes("headers", |dir, mode| {
        fs::write(dir.join("a"), "a1\n").unwrap();
        fs::write(dir.join("b"), "b1\n").unwrap();
        let t = Follower::start(dir, &[mode, &["-f", "a", "b"]].concat());
        t.expect("==> a <==\na1\n\n==> b <==\nb1\n");
        append(&dir.join("a"), "a2\n");
        t.expect("b1\n\n==> a <==\na2\n");
        append(&dir.join("b"), "b2\n");
        t.expect("a2\n\n==> b <==\nb2\n");
    });
}

#[test]
fn follow_name_without_retry_gives_up() {
    both_modes("no-retry", |dir, mode| {
        fs::write(dir.join("f"), "x\n").unwrap();
        let mut t = Follower::start(dir, &[mode, &["--follow=name", "f"]].concat());
        t.expect("x\n");
        fs::remove_file(dir.join("f")).unwrap();
        // As in GNU, the inotify loop fails while polling ends the loop and
        // exits with the status of the initial reads.
        let status = if mode.is_empty() { 1 } else { 0 };
        assert_eq!(t.wait_exit(), Some(status));
        assert!(
            t.output().ends_with("tail: no files remaining\n"),
            "{}",
            t.output()
        );
    });
}

#[test]
fn follow_stops_when_pid_dies() {
    both_modes("pid", |dir, mode| {
        fs::write(dir.join("f"), "x\n").unwrap();
        let mut writer = Command::new("sleep").arg("1").spawn().unwrap();
        let pid = format!("--pid={}", writer.id());
        let mut t = Follower::start(dir, &[mode, &["-f", &pid, "f"]].concat());
        t.expect("x\n");
        assert!(t.child.try_wait().unwrap().is_none(), "tail exited early");
        writer.wait().unwrap();
        assert_eq!(t.wait_exit(), Some(0));
    });
}

#[test]
fn follow_exits_when_output_reader_goes() {
    both_modes("reader-gone", |dir, mode| {
        fs::write(dir.join("f"), "x\n").unwrap();
        let mut child = Command::new(dogfish_bin())
            .arg("tail")
            .args(mode)
            .args(["-s.1", "-f", "f"])
            .current_dir(dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        drop(child.stdout.take());
        let start = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if start.elapsed() > PATIENCE {
                let _ = child.kill();
                panic!("tail kept running after its reader went away");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        // Killed by SIGPIPE (the default disposition).
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGPIPE));
    });
}
