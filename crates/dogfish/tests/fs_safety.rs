//! Regression tests for destructive file operations: `cp`, `mv`, `rm`, and `undo`
//! must never lose data, including on edge cases GNU coreutils guards against.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Command, Output};

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self, rel: &str) -> std::path::PathBuf {
        self.dir.path().join(rel)
    }

    fn write(&self, rel: &str, content: &str) {
        let p = self.path(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, content).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.path(rel)).unwrap()
    }

    fn exists(&self, rel: &str) -> bool {
        self.path(rel).symlink_metadata().is_ok()
    }

    /// Runs dogfish inside the sandbox with an isolated undo journal.
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_dogfish"))
            .current_dir(self.dir.path())
            .env("DOGFISH_HOME", self.path(".dogfish"))
            .args(args)
            .output()
            .expect("failed to execute dogfish")
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn cp_onto_itself_is_refused_and_preserves_data() {
    let sb = Sandbox::new();
    sb.write("z", "important");
    fs::hard_link(sb.path("z"), sb.path("z_link")).unwrap();
    symlink("z", sb.path("z_sym")).unwrap();

    for dst in ["z", "./z", "z_link", "z_sym"] {
        let out = sb.run(&["cp", "z", dst]);
        assert_eq!(out.status.code(), Some(1), "cp z {dst} must fail");
        assert!(
            stderr(&out).contains("are the same file"),
            "{}",
            stderr(&out)
        );
        assert_eq!(sb.read("z"), "important", "cp z {dst} destroyed data");
    }
}

#[test]
fn mv_onto_itself_is_refused() {
    let sb = Sandbox::new();
    sb.write("a", "keep");
    let out = sb.run(&["mv", "a", "./a"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(sb.read("a"), "keep");
}

#[test]
fn undo_of_overwriting_cp_restores_previous_contents() {
    let sb = Sandbox::new();
    sb.write("src", "new");
    sb.write("dst", "original");
    assert!(sb.run(&["cp", "src", "dst"]).status.success());
    assert_eq!(sb.read("dst"), "new");
    assert!(sb.run(&["undo"]).status.success());
    assert_eq!(sb.read("dst"), "original");
    assert_eq!(sb.read("src"), "new");
}

#[test]
fn undo_of_cp_merging_into_directory_keeps_existing_files() {
    let sb = Sandbox::new();
    sb.write("t1/a", "1");
    sb.write("dest/t1/keep", "keep");
    assert!(sb.run(&["cp", "-r", "t1", "dest"]).status.success());
    assert!(sb.exists("dest/t1/a"));
    assert!(sb.run(&["undo"]).status.success());
    assert!(!sb.exists("dest/t1/a"));
    assert_eq!(sb.read("dest/t1/keep"), "keep");
}

#[test]
fn undo_of_overwriting_mv_restores_both_files() {
    let sb = Sandbox::new();
    sb.write("a", "A");
    sb.write("b", "B");
    assert!(sb.run(&["mv", "a", "b"]).status.success());
    assert_eq!(sb.read("b"), "A");
    assert!(sb.run(&["undo"]).status.success());
    assert_eq!(sb.read("a"), "A");
    assert_eq!(sb.read("b"), "B");
}

#[test]
fn undo_refuses_to_clobber_new_files() {
    let sb = Sandbox::new();
    sb.write("a", "A");
    assert!(sb.run(&["mv", "a", "c"]).status.success());
    sb.write("a", "blocker");
    let out = sb.run(&["undo"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("refusing to undo"),
        "{}",
        stderr(&out)
    );
    assert_eq!(sb.read("a"), "blocker");
    assert_eq!(sb.read("c"), "A");

    // The entry is kept, so undo succeeds once the path is free again.
    fs::remove_file(sb.path("a")).unwrap();
    assert!(sb.run(&["undo"]).status.success());
    assert_eq!(sb.read("a"), "A");
}

#[test]
fn undo_of_rm_safe_does_not_clobber() {
    let sb = Sandbox::new();
    sb.write("f", "old");
    assert!(sb.run(&["rm", "--safe", "f"]).status.success());
    sb.write("f", "new");
    assert!(!sb.run(&["undo"]).status.success());
    assert_eq!(sb.read("f"), "new");
}

#[test]
fn rm_safe_restores_symlinks_as_symlinks() {
    let sb = Sandbox::new();
    sb.write("target", "data");
    symlink("target", sb.path("link")).unwrap();
    assert!(sb.run(&["rm", "--safe", "link"]).status.success());
    assert!(!sb.exists("link"));
    assert_eq!(sb.read("target"), "data");
    assert!(sb.run(&["undo"]).status.success());
    assert!(sb.path("link").symlink_metadata().unwrap().is_symlink());
    assert_eq!(sb.read("target"), "data");
}

#[test]
fn rm_safe_refuses_directories_without_recursive() {
    let sb = Sandbox::new();
    sb.write("d/f", "x");
    let out = sb.run(&["rm", "--safe", "d"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(sb.exists("d/f"));
}

#[test]
fn rm_refuses_dot_and_dotdot() {
    let sb = Sandbox::new();
    sb.write("d/f", "x");
    for arg in [".", "..", "d/.", "d/..", "./"] {
        let out = sb.run(&["rm", "-rf", arg]);
        assert_eq!(out.status.code(), Some(1), "rm -rf {arg}");
        assert!(
            stderr(&out).contains("refusing to remove"),
            "{}",
            stderr(&out)
        );
    }
    assert!(sb.exists("d/f"));
}

#[test]
fn rm_r_on_symlink_to_directory_only_removes_link() {
    let sb = Sandbox::new();
    sb.write("real/keep", "keep");
    symlink("real", sb.path("link")).unwrap();
    assert!(sb.run(&["rm", "-r", "link"]).status.success());
    assert!(!sb.exists("link"));
    assert_eq!(sb.read("real/keep"), "keep");
}

#[test]
fn cp_and_mv_refuse_directory_into_itself() {
    let sb = Sandbox::new();
    sb.write("d/sub/f", "x");
    let out = sb.run(&["cp", "-r", "d", "d/sub"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("into itself"), "{}", stderr(&out));
    assert!(!sb.exists("d/sub/d"));

    let out = sb.run(&["mv", "d", "d/sub"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("subdirectory of itself"),
        "{}",
        stderr(&out)
    );
    assert!(sb.exists("d/sub/f"));
}

#[test]
fn cp_recursive_preserves_symlinks() {
    let sb = Sandbox::new();
    sb.write("d/f", "x");
    symlink("f", sb.path("d/l")).unwrap();
    symlink("/nonexistent/dangling", sb.path("d/dangling")).unwrap();
    assert!(sb.run(&["cp", "-r", "d", "e"]).status.success());
    assert_eq!(fs::read_link(sb.path("e/l")).unwrap(), Path::new("f"));
    assert_eq!(
        fs::read_link(sb.path("e/dangling")).unwrap(),
        Path::new("/nonexistent/dangling")
    );
}

#[test]
fn cp_continues_after_missing_source() {
    let sb = Sandbox::new();
    sb.write("a", "A");
    fs::create_dir(sb.path("out")).unwrap();
    let out = sb.run(&["cp", "missing", "a", "out"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("cannot stat 'missing': No such file or directory"),
        "{}",
        stderr(&out)
    );
    assert!(!stderr(&out).contains("os error"));
    assert_eq!(sb.read("out/a"), "A");
}

#[test]
fn mv_refuses_to_replace_non_empty_directory() {
    let sb = Sandbox::new();
    sb.write("src/a", "A");
    sb.write("dst/src/keep", "keep");
    let out = sb.run(&["mv", "src", "dst"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(sb.read("dst/src/keep"), "keep");
    assert_eq!(sb.read("src/a"), "A");
}

#[test]
fn corrupt_journal_is_reported_not_overwritten() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.path(".dogfish/trash")).unwrap();
    fs::write(sb.path(".dogfish/journal.json"), "{not json").unwrap();
    let out = sb.run(&["history"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("corrupt undo journal"),
        "{}",
        stderr(&out)
    );
    sb.write("a", "A");
    sb.run(&["cp", "a", "b"]);
    assert_eq!(sb.read(".dogfish/journal.json"), "{not json");
}

#[test]
fn concurrent_journal_writers_do_not_lose_entries() {
    let sb = Sandbox::new();
    for i in 0..16 {
        sb.write(&format!("f{i}"), "x");
    }
    let children: Vec<_> = (0..16)
        .map(|i| {
            Command::new(env!("CARGO_BIN_EXE_dogfish"))
                .current_dir(sb.dir.path())
                .env("DOGFISH_HOME", sb.path(".dogfish"))
                .args(["mv", &format!("f{i}"), &format!("g{i}")])
                .spawn()
                .unwrap()
        })
        .collect();
    for mut c in children {
        assert!(c.wait().unwrap().success());
    }
    let out = sb.run(&["history", "--json"]);
    let entries: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(entries.as_array().unwrap().len(), 16);
}

#[test]
fn ln_force_onto_itself_is_refused() {
    let sb = Sandbox::new();
    sb.write("a", "keep");
    for args in [&["ln", "-f", "a", "a"][..], &["ln", "-sf", "a", "a"][..]] {
        let out = sb.run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(
            stderr(&out).contains("are the same file"),
            "{}",
            stderr(&out)
        );
        assert_eq!(sb.read("a"), "keep");
    }
    // Replacing an existing symlink is fine.
    symlink("a", sb.path("l")).unwrap();
    assert!(sb.run(&["ln", "-sf", "a", "l"]).status.success());
    assert_eq!(sb.read("l"), "keep");
}

#[test]
fn mktemp_is_exclusive_private_and_honours_template() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::new();
    let out = sb.run(&["mktemp", "foo.XXXXXX"]);
    assert!(out.status.success());
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(name.starts_with("foo.") && name.len() == 10, "{name}");
    let mode = fs::metadata(sb.path(&name)).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    let out = sb.run(&["mktemp", "-d", "-p", ".", "dir.XXXX"]);
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let mode = fs::metadata(sb.path(&name)).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);

    assert_eq!(sb.run(&["mktemp", "foo.XX"]).status.code(), Some(1));

    let names: std::collections::HashSet<_> = (0..20)
        .map(|_| String::from_utf8_lossy(&sb.run(&["mktemp", "u.XXX"]).stdout).into_owned())
        .collect();
    assert_eq!(names.len(), 20);
}

#[test]
fn touch_never_truncates() {
    let sb = Sandbox::new();
    sb.write("f", "data");
    assert!(sb.run(&["touch", "f"]).status.success());
    assert_eq!(sb.read("f"), "data");
    assert!(sb.run(&["touch", "new"]).status.success());
    assert_eq!(sb.read("new"), "");
}
