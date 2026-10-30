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
        self.run_env(args, &[])
    }

    fn run_env(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_dogfish"))
            .current_dir(self.dir.path())
            .env("DOGFISH_HOME", self.path(".dogfish"))
            .envs(envs.iter().copied())
            .args(args)
            .output()
            .expect("failed to execute dogfish")
    }

    fn trash_len(&self) -> usize {
        fs::read_dir(self.path(".dogfish/trash")).unwrap().count()
    }

    /// The journal, one `history --json` record per entry.
    fn history(&self) -> Vec<serde_json::Value> {
        let out = self.run(&["history", "--json"]);
        assert!(out.status.success(), "{}", stderr(&out));
        out.stdout
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_slice(l).unwrap())
            .collect()
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

/// `mv a b` replaces `b` with one rename, as GNU's does: killed at any step,
/// `b` is there with either its old or its new contents, never missing (a
/// `tail -F b` would otherwise report it gone), and undo restores both files.
#[cfg(debug_assertions)]
#[test]
fn overwriting_mv_never_leaves_the_destination_missing() {
    for n in 1..100 {
        let sb = Sandbox::new();
        sb.write("a", "A");
        sb.write("b", "B");
        let n = n.to_string();
        let out = sb.run_env(&["mv", "a", "b"], &[("DOGFISH_CRASH_AT", &n)]);
        let b = sb.read("b");
        assert!(b == "A" || b == "B", "crash at {n}: b = {b:?}");
        assert!(sb.run(&["undo"]).status.success(), "crash at {n}");
        assert_eq!(sb.read("a"), "A", "crash at {n}");
        assert_eq!(sb.read("b"), "B", "crash at {n}");
        if out.status.success() {
            return;
        }
    }
    panic!("mv a b never ran to completion");
}

/// `mv c c/c`, with `c` a symlink to a directory, moves the symlink into that
/// directory: the rename takes away what `c/c` resolved through, so the move
/// must be recorded by where it went, not by the path as given.
#[test]
fn undo_of_mv_through_the_moved_symlink() {
    let sb = Sandbox::new();
    sb.write("b/x", "x");
    symlink("b", sb.path("c")).unwrap();
    assert!(sb.run(&["mv", "c", "c/c"]).status.success());
    assert!(!sb.exists("c") && sb.exists("b/c"));
    let out = sb.run(&["undo"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(fs::read_link(sb.path("c")).unwrap(), Path::new("b"));
    assert!(!sb.exists("b/c"));
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
    for name in ["journal.ndjson", "journal.json"] {
        let sb = Sandbox::new();
        fs::create_dir_all(sb.path(".dogfish/trash")).unwrap();
        let journal = format!(".dogfish/{name}");
        fs::write(sb.path(&journal), "{not json\n").unwrap();
        let out = sb.run(&["history"]);
        assert!(!out.status.success(), "{name}");
        assert!(
            stderr(&out).contains("corrupt undo journal"),
            "{}",
            stderr(&out)
        );
        sb.write("a", "A");
        sb.run(&["cp", "a", "b"]);
        assert_eq!(sb.read(&journal), "{not json\n", "{name} was modified");
    }
}

#[test]
fn legacy_json_journal_is_migrated_and_still_undoable() {
    let sb = Sandbox::new();
    sb.write("moved", "M");
    let src = sb.path("orig");
    let dst = sb.path("moved");
    fs::create_dir_all(sb.path(".dogfish/trash")).unwrap();
    let legacy = serde_json::json!([{
        "id": 7,
        "timestamp_unix": 1,
        "tool": "mv",
        "action": { "action": "move", "src": src, "dst": dst }
    }]);
    fs::write(sb.path(".dogfish/journal.json"), legacy.to_string()).unwrap();

    assert_eq!(sb.history()[0]["id"], 7);
    assert!(sb.exists(".dogfish/journal.json.migrated"));
    assert!(!sb.exists(".dogfish/journal.json"));
    assert!(sb.run(&["undo"]).status.success());
    assert_eq!(sb.read("orig"), "M");
}

#[test]
fn torn_final_line_from_a_crash_is_ignored_and_repaired() {
    let sb = Sandbox::new();
    sb.write("a", "A");
    assert!(sb.run(&["mv", "a", "b"]).status.success());
    let journal = sb.path(".dogfish/journal.ndjson");
    let mut content = fs::read_to_string(&journal).unwrap();
    content.push_str("{\"id\":2,\"timest");
    fs::write(&journal, content).unwrap();

    assert_eq!(sb.history().len(), 1);
    assert!(sb.run(&["mv", "b", "c"]).status.success());
    let ids: Vec<_> = sb.history().iter().map(|e| e["id"].clone()).collect();
    assert_eq!(ids, [1, 2]);
    assert!(sb.run(&["undo", "-n", "2"]).status.success());
    assert_eq!(sb.read("a"), "A");
}

#[test]
fn retention_drops_oldest_entries_and_their_trash() {
    let sb = Sandbox::new();
    let limit = [("DOGFISH_JOURNAL_MAX", "3")];
    for i in 0..5 {
        sb.write(&format!("f{i}"), "x");
        assert!(
            sb.run_env(&["rm", "--safe", &format!("f{i}")], &limit)
                .status
                .success()
        );
    }
    let ids: Vec<_> = sb.history().iter().map(|e| e["id"].clone()).collect();
    assert_eq!(ids, [3, 4, 5]);
    assert_eq!(sb.trash_len(), 3, "payloads of dropped entries are deleted");
}

#[test]
fn backup_limit_discards_oldest_backups_but_never_loses_current_files() {
    let sb = Sandbox::new();
    let limit = [("DOGFISH_BACKUP_LIMIT", "10")];
    sb.write("dst", "original-1");
    sb.write("v2", "version-2");
    sb.write("v3", "version-3");
    assert!(sb.run_env(&["cp", "v2", "dst"], &limit).status.success());
    // The newest backup is always kept, even alone over the limit.
    assert_eq!(sb.trash_len(), 1);
    assert!(sb.run_env(&["cp", "v3", "dst"], &limit).status.success());
    assert_eq!(
        sb.trash_len(),
        1,
        "older backup discarded to honour the limit"
    );

    let history = sb.history();
    // `history --json` lists oldest first.
    assert_eq!(history[0]["backup_purged"], true);
    assert!(history[1].get("backup_purged").is_none());

    // Undo #2 restores version-2 from its backup; undo #1 has no backup left, so it
    // must leave the file alone rather than delete it.
    assert!(sb.run(&["undo"]).status.success());
    assert_eq!(sb.read("dst"), "version-2");
    let out = sb.run(&["undo"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("cannot be restored"));
    assert_eq!(sb.read("dst"), "version-2");
}

#[test]
fn rm_safe_files_are_exempt_from_the_backup_limit() {
    let sb = Sandbox::new();
    let limit = [("DOGFISH_BACKUP_LIMIT", "1")];
    sb.write("keep", "a fairly large trashed file");
    assert!(
        sb.run_env(&["rm", "--safe", "keep"], &limit)
            .status
            .success()
    );
    sb.write("dst", "old");
    sb.write("new", "new");
    assert!(sb.run_env(&["cp", "new", "dst"], &limit).status.success());
    sb.write("new2", "newer");
    assert!(sb.run_env(&["cp", "new2", "dst"], &limit).status.success());
    assert!(sb.run(&["undo", "-n", "3"]).status.success());
    assert_eq!(sb.read("keep"), "a fairly large trashed file");
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
    assert_eq!(sb.history().len(), 16);
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

#[test]
fn undo_recording_can_be_turned_off_but_rm_safe_still_trashes() {
    let sb = Sandbox::new();
    let off = [("DOGFISH_UNDO", "off")];
    sb.write("src", "new");
    sb.write("dst", "old");
    assert!(sb.run_env(&["cp", "src", "dst"], &off).status.success());
    assert!(sb.run_env(&["mv", "src", "moved"], &off).status.success());
    assert_eq!(sb.read("dst"), "new");
    assert!(!sb.exists(".dogfish/journal.ndjson"), "nothing recorded");

    sb.write("f", "keep me");
    assert!(sb.run_env(&["rm", "--safe", "f"], &off).status.success());
    assert!(sb.run(&["undo"]).status.success());
    assert_eq!(sb.read("f"), "keep me");
}

// ---------------------------------------------------------------------------
// Counterexamples found by the model-based tests (tests/undo_model.rs), each
// reduced to its smallest form.

/// A directory on another file system than the sandbox, when there is one.
fn other_fs_dir() -> Option<tempfile::TempDir> {
    use std::os::unix::fs::MetadataExt;
    let here = fs::metadata(std::env::temp_dir()).ok()?.dev();
    tempfile::tempdir_in("/dev/shm")
        .ok()
        .filter(|d| fs::metadata(d.path()).is_ok_and(|m| m.dev() != here))
}

fn same_inode(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (a, b) = (fs::metadata(a).unwrap(), fs::metadata(b).unwrap());
    (a.dev(), a.ino()) == (b.dev(), b.ino())
}

/// Removes the trash directories this sandbox's journal made on other file
/// systems (listed in `$DOGFISH_HOME/trash-dirs`).
fn remove_other_fs_trash(sb: &Sandbox) {
    if let Ok(list) = fs::read_to_string(sb.path(".dogfish/trash-dirs")) {
        for dir in list.lines() {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

#[test]
fn failed_mv_records_nothing() {
    // `mv missing b` used to record a move, and undo then renamed b to `missing`.
    let sb = Sandbox::new();
    sb.write("b", "keep");
    assert!(!sb.run(&["mv", "missing", "b"]).status.success());
    assert!(sb.history().is_empty());
    assert!(sb.run(&["undo"]).status.success());
    assert_eq!(sb.read("b"), "keep");
    assert!(!sb.exists("missing"));
}

#[test]
fn mv_that_leaves_its_source_alone_records_nothing() {
    let sb = Sandbox::new();
    sb.write("a", "a");
    sb.write("b", "b");
    sb.run(&["mv", "-n", "a", "a"]);
    sb.run(&["mv", "-n", "a/", "b"]);
    assert!(sb.history().is_empty());
}

#[test]
fn undo_of_cp_into_a_hard_linked_file_restores_every_link() {
    // cp writes into the existing inode, so every link sees the new contents;
    // undo used to rename the backup over `a`, leaving `l` with them.
    let sb = Sandbox::new();
    sb.write("a", "old");
    fs::hard_link(sb.path("a"), sb.path("l")).unwrap();
    sb.write("b", "new");
    assert!(sb.run(&["cp", "b", "a"]).status.success());
    assert_eq!(sb.read("l"), "new");
    assert!(sb.run(&["undo"]).status.success());
    assert_eq!(sb.read("a"), "old");
    assert_eq!(sb.read("l"), "old");
    assert!(same_inode(&sb.path("a"), &sb.path("l")));
}

#[test]
fn undo_of_cp_merging_into_a_directory_keeps_its_inodes() {
    let sb = Sandbox::new();
    sb.write("src/d/f", "new");
    sb.write("dst/d/f", "old");
    fs::hard_link(sb.path("dst/d/f"), sb.path("outside")).unwrap();
    assert!(sb.run(&["cp", "-r", "src/d", "dst"]).status.success());
    assert_eq!(sb.read("outside"), "new");
    assert!(sb.run(&["undo"]).status.success());
    assert_eq!(sb.read("dst/d/f"), "old");
    assert_eq!(sb.read("outside"), "old");
    assert!(same_inode(&sb.path("dst/d/f"), &sb.path("outside")));
}

#[test]
fn undo_restores_a_fifo_that_remove_destination_replaced() {
    use std::os::unix::fs::FileTypeExt;
    let sb = Sandbox::new();
    let c = std::ffi::CString::new(sb.path("f").to_str().unwrap()).unwrap();
    // SAFETY: `c` is a valid NUL-terminated path for the duration of the call.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o640) }, 0);
    sb.write("a", "data");
    let out = sb.run(&["cp", "--remove-destination", "a", "f"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(sb.run(&["undo"]).status.success());
    let meta = fs::symlink_metadata(sb.path("f")).unwrap();
    assert!(meta.file_type().is_fifo());
}

#[test]
fn undo_of_mv_into_a_symlinked_directory_moves_it_back() {
    // `mv b x b` with `b` a symlink to `d` renames the link itself to d/b
    // (as GNU does), after which `b/b` names nothing: the journal must record
    // where it really went.
    let sb = Sandbox::new();
    fs::create_dir(sb.path("d")).unwrap();
    symlink("d", sb.path("b")).unwrap();
    let out = sb.run(&["mv", "b", "missing", "b"]);
    assert!(!out.status.success());
    assert!(
        !stderr(&out).contains("cannot stat 'b/b'"),
        "{}",
        stderr(&out)
    );
    assert!(fs::symlink_metadata(sb.path("d/b")).is_ok());
    assert!(sb.run(&["undo"]).status.success());
    assert_eq!(fs::read_link(sb.path("b")).unwrap(), Path::new("d"));
    assert!(fs::symlink_metadata(sb.path("d/b")).is_err());
}

#[test]
fn undo_of_cp_backup_undoes_the_copy_before_the_backup_rename() {
    let sb = Sandbox::new();
    sb.write("a", "old");
    sb.write("b", "new");
    assert!(sb.run(&["cp", "-b", "b", "a"]).status.success());
    assert_eq!(sb.read("a~"), "old");
    assert!(sb.run(&["undo", "-n", "2"]).status.success());
    assert_eq!(sb.read("a"), "old");
    assert!(!sb.exists("a~"));
}

#[test]
fn rm_safe_on_another_file_system_keeps_hard_links() {
    // The home trash is elsewhere: moving there copied, and undo brought back
    // a new inode. Each file system now gets a trash of its own.
    let Some(other) = other_fs_dir() else {
        eprintln!("note: no second file system at /dev/shm");
        return;
    };
    let sb = Sandbox::new();
    let (a, b) = (other.path().join("a"), other.path().join("b"));
    fs::write(&a, "data").unwrap();
    fs::hard_link(&a, &b).unwrap();
    let out = sb.run(&["rm", "--safe", a.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(sb.run(&["undo"]).status.success());
    assert!(same_inode(&a, &b));
    remove_other_fs_trash(&sb);
}

#[test]
fn failed_cp_link_with_backup_puts_the_backup_back_and_records_nothing() {
    let Some(other) = other_fs_dir() else {
        eprintln!("note: no second file system at /dev/shm");
        return;
    };
    let sb = Sandbox::new();
    sb.write("b", "src");
    let c = other.path().join("c");
    fs::write(&c, "keep").unwrap();
    // A hard link across file systems fails after the backup rename.
    let out = sb.run(&["cp", "--backup=numbered", "-l", "b", c.to_str().unwrap()]);
    assert!(!out.status.success());
    assert_eq!(fs::read_to_string(&c).unwrap(), "keep");
    assert!(sb.history().is_empty(), "{:?}", sb.history());
    remove_other_fs_trash(&sb);
}

/// `a/` holding `c` and `a/a`, a symlink to `../a` (so `a/a/a` is `a/a`).
fn dir_with_symlink_to_itself(sb: &Sandbox) {
    sb.write("a/c", "");
    symlink("../a", sb.path("a/a")).unwrap();
}

#[test]
fn cp_backup_onto_itself_through_a_directory_symlink_changes_nothing() {
    // `a/a/a` is the entry `a/a`. The same-file check compared the parent
    // directories without following `a/a`, so cp renamed `a/a` to `a/a~`
    // and then failed, leaving it renamed.
    let sb = Sandbox::new();
    dir_with_symlink_to_itself(&sb);
    let out = sb.run(&["cp", "--backup=simple", "-r", "a/a", "a/a"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stderr(&out), "cp: 'a/a' and 'a/a/a' are the same file\n");
    assert_eq!(fs::read_link(sb.path("a/a")).unwrap(), Path::new("../a"));
    assert!(!sb.exists("a/a~"));
    assert!(sb.history().is_empty(), "{:?}", sb.history());
}

#[test]
fn undo_puts_back_a_backup_made_through_a_symlink_it_renamed() {
    // The backup renames the symlink the destination's path goes through;
    // the journal recorded the path as spelled (`a/a/a`), which then named
    // nothing, so undo left the symlink renamed.
    let sb = Sandbox::new();
    dir_with_symlink_to_itself(&sb);
    let out = sb.run(&["mv", "--backup=numbered", "a", "a/a"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        "mv: cannot move 'a' to a subdirectory of itself, 'a/a/a'\n"
    );
    assert!(sb.exists("a/a.~1~"));
    assert!(sb.run(&["undo"]).status.success());
    assert_eq!(fs::read_link(sb.path("a/a")).unwrap(), Path::new("../a"));
    assert!(!sb.exists("a/a.~1~"));

    // As GNU, cp can't put the backup back by the path as spelled; undo can.
    symlink(".", sb.path("l")).unwrap();
    let out = sb.run(&["cp", "-Pb", "-T", "a/a", "l/l"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("cannot un-backup 'l/l'"),
        "{}",
        stderr(&out)
    );
    assert!(sb.run(&["undo"]).status.success());
    assert_eq!(fs::read_link(sb.path("l")).unwrap(), Path::new("."));
    assert!(!sb.exists("l~"));
    assert!(sb.history().is_empty(), "{:?}", sb.history());
}

#[test]
fn undo_of_mv_across_file_systems_keeps_hard_links() {
    // The move copies `a` and unlinks it; its inode lives on as `b`. Undo
    // used to copy the file back as a new inode, so `a` and `b` were no
    // longer one file (and undoing an earlier in-place `cp` into it then
    // reached `a` only).
    let Some(other) = other_fs_dir() else {
        eprintln!("note: no second file system at /dev/shm");
        return;
    };
    let sb = Sandbox::new();
    let (a, b) = (other.path().join("a"), other.path().join("b"));
    fs::write(&a, "").unwrap();
    fs::hard_link(&a, &b).unwrap();
    sb.write("a", "\0");
    fs::hard_link(sb.path("a"), sb.path("b")).unwrap();
    let a_str = a.to_str().unwrap();
    for args in [["cp", "a", a_str], ["mv", a_str, "a"]] {
        let out = sb.run(&args);
        assert!(out.status.success(), "{args:?}: {}", stderr(&out));
    }
    assert_eq!(fs::read_to_string(&b).unwrap(), "\0");
    for _ in 0..2 {
        let out = sb.run(&["undo"]);
        assert!(out.status.success(), "{}", stderr(&out));
    }
    assert!(same_inode(&a, &b));
    assert_eq!(fs::read_to_string(&a).unwrap(), "");
    assert!(same_inode(&sb.path("a"), &sb.path("b")));
    assert_eq!(sb.read("a"), "\0");
    assert!(sb.history().is_empty());
    remove_other_fs_trash(&sb);
}

#[test]
fn undo_of_mv_across_file_systems_after_an_edit_keeps_both_versions() {
    // Once the moved copy or the original (through another link) has
    // changed, the kept inode can't stand in for the copy: undo copies the
    // copy back as a new file, the other link keeps what it holds, and the
    // kept inode stays in the trash.
    let Some(other) = other_fs_dir() else {
        eprintln!("note: no second file system at /dev/shm");
        return;
    };
    for edit_copy in [true, false] {
        let sb = Sandbox::new();
        let (a, b) = (other.path().join("a"), other.path().join("b"));
        fs::write(&a, "old").unwrap();
        fs::hard_link(&a, &b).unwrap();
        let out = sb.run(&["mv", a.to_str().unwrap(), "c"]);
        assert!(out.status.success(), "{}", stderr(&out));
        let (moved, linked) = if edit_copy {
            sb.write("c", "edited");
            ("edited", "old")
        } else {
            fs::write(&b, "edited").unwrap();
            ("old", "edited")
        };
        let out = sb.run(&["undo"]);
        assert!(out.status.success(), "{}", stderr(&out));
        assert!(!sb.exists("c"));
        assert_eq!(fs::read_to_string(&a).unwrap(), moved);
        assert_eq!(fs::read_to_string(&b).unwrap(), linked);
        assert!(!same_inode(&a, &b));
        remove_other_fs_trash(&sb);
        fs::remove_file(&a).unwrap();
        fs::remove_file(&b).unwrap();
    }
}
