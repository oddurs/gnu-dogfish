//! Differential GNU-compatibility suite for the file tools: `touch`, `mkdir`,
//! `realpath`, `basename`, `dirname`, `mktemp`, `ln`, `chmod`, `chown`, `cp`,
//! `mv` and `rm`.
//!
//! Read-only cases use the shared `gnu_cases!` table. Every mutating case
//! builds two identical scratch trees, runs the same command (and stdin) in
//! each — `dogfish <tool>` in one, the system GNU tool in the other — and
//! compares stdout, stderr, the exit status and the resulting trees: names,
//! file types, contents, permission bits, ownership, symlink targets, hard-link
//! structure and (where the command sets them deterministically) timestamps.
//! Dogfish's undo journal lives outside the tree (`DOGFISH_HOME`).
//!
//! When the system tool is missing or is not GNU coreutils, a case prints a
//! note and passes.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{dogfish_bin, exec, gnu_tool};

// ------------------------------------------------------------------ fixtures

/// Read-only fixture for `realpath` and friends (paths are printed absolute,
/// identically for both implementations since they share this directory).
fn fixtures() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = common::fresh_fixture_dir();
        common::write_files(&dir, &[("d/f", b"F\n"), ("d/sub/g", b"G\n"), ("a", b"A\n")]);
        for (link, target) in [
            ("ld", "d"),
            ("la", "a"),
            ("d/up", "../a"),
            ("d/sub/lf", "../f"),
            ("dangling", "nowhere"),
            ("loop1", "loop2"),
            ("loop2", "loop1"),
            ("chain", "ld/sub"),
        ] {
            symlink(target, dir.join(link)).unwrap();
        }
        dir
    })
}

/// Timestamp given to every entry of a scratch tree (2001-09-09).
const T0: i64 = 1_000_000_000;

/// Builds the scratch tree every mutating case starts from. Both runs of a case
/// build it the same way, in the same order, so directory order matches too.
fn build_tree(root: &Path) {
    let files: &[(&str, &[u8], u32)] = &[
        ("a", b"A\n", 0o644),
        ("b", b"B\n", 0o644),
        ("c.txt", b"hello\n", 0o644),
        ("c.txt~", b"old backup\n", 0o644),
        ("empty", b"", 0o644),
        ("exe", b"#!/bin/sh\n", 0o755),
        ("ro", b"read-only\n", 0o444),
        ("wide", b"w\n", 0o666),
        ("old", b"old\n", 0o644),
        ("new", b"new\n", 0o644),
        ("num", b"N\n", 0o644),
        ("num.~1~", b"N1\n", 0o644),
        ("spa ce", b"S\n", 0o644),
        ("h1", b"linked\n", 0o644),
        ("d/f", b"F\n", 0o644),
        ("d/.hidden", b"H\n", 0o600),
        ("d/sub/g", b"G\n", 0o640),
        ("p2/q/z", b"Z\n", 0o644),
    ];
    for dir in ["d/sub", "e", "p/q", "p2/q", "q", "sg"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    for (name, data, mode) in files {
        let path = root.join(name);
        fs::write(&path, data).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(*mode)).unwrap();
    }
    fs::hard_link(root.join("h1"), root.join("h2")).unwrap();
    for (link, target) in [
        ("la", "a"),
        ("dl", "nowhere"),
        ("ld", "d"),
        ("lla", "la"),
        ("lsub", "d/sub/g"),
        ("lh1", "h1"),
        ("d/up", "../a"),
    ] {
        symlink(target, root.join(link)).unwrap();
    }
    fs::set_permissions(root.join("sg"), fs::Permissions::from_mode(0o2755)).unwrap();
    // Fixed times, deepest entries first so setting a child never touches a parent.
    let mut entries = Vec::new();
    collect(root, root, &mut entries);
    entries.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for rel in entries {
        let t = match rel.to_str() {
            Some("old") => T0 - 100_000,
            Some("new") => T0 + 100_000,
            _ => T0,
        };
        let ft = filetime::FileTime::from_unix_time(t, 0);
        filetime::set_symlink_file_times(root.join(&rel), ft, ft).unwrap();
    }
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        out.push(path.strip_prefix(root).unwrap().to_path_buf());
        if path.symlink_metadata().unwrap().is_dir() {
            collect(root, &path, out);
        }
    }
}

// ------------------------------------------------------------- tree compare

/// What a case compares about the resulting tree, besides names, types,
/// contents, modes, ownership, link targets and hard links.
#[derive(Clone, Copy, Default)]
struct Compare {
    /// Modification times of non-directories.
    mtime: bool,
    /// Access times of non-directories too.
    atime: bool,
    /// Modification times of directories too.
    dir_mtime: bool,
    /// Skip the tree comparison entirely (a documented deviation).
    no_tree: bool,
    /// Skip comparing stderr.
    no_stderr: bool,
    /// Run Dogfish with `DOGFISH_UNDO=off`: the undo trash keeps an extra hard
    /// link to a replaced file, which shows in its link count (ROADMAP.md).
    no_undo: bool,
    /// Compare stdout as a set of lines: GNU `cp -r` copies a directory's
    /// entries in inode order, which differs between the two scratch trees.
    unordered: bool,
    /// Give the case a second directory on another file system (cross-device
    /// `mv`): `@OTHER@` in the arguments names it, `$OTHER` in the setup. The
    /// case is skipped when there is no such file system.
    other_fs: bool,
    /// Run both tools with `POSIXLY_CORRECT` set.
    posixly_correct: bool,
}

fn snapshot(root: &Path, cmp: Compare) -> BTreeMap<String, String> {
    let mut entries = Vec::new();
    collect(root, root, &mut entries);
    let mut by_inode: HashMap<u64, Vec<String>> = HashMap::new();
    let mut metas = Vec::new();
    for rel in entries {
        let path = root.join(&rel);
        let meta = path.symlink_metadata().unwrap();
        // Names that are not UTF-8 are compared escaped, never merged.
        let name = rel
            .to_str()
            .map_or_else(|| format!("{rel:?}"), str::to_string);
        if !meta.is_dir() && meta.nlink() > 1 {
            by_inode.entry(meta.ino()).or_default().push(name.clone());
        }
        metas.push((name, path, meta));
    }
    let mut snap = BTreeMap::new();
    for (name, path, meta) in metas {
        let ft = meta.file_type();
        let mut desc = if ft.is_symlink() {
            format!("symlink -> {}", fs::read_link(&path).unwrap().display())
        } else if ft.is_dir() {
            format!("dir {:04o}", meta.mode() & 0o7777)
        } else if ft.is_file() {
            format!(
                "file {:04o} {:?}",
                meta.mode() & 0o7777,
                // Unreadable without privileges (mode 000): compare the mode only.
                fs::read(&path).map_or_else(
                    |_| "<unreadable>".to_string(),
                    |data| String::from_utf8_lossy(&data).into_owned()
                )
            )
        } else {
            format!("special {:o}", meta.mode())
        };
        desc.push_str(&format!(" owner={}:{}", meta.uid(), meta.gid()));
        if let Some(group) = by_inode.get_mut(&meta.ino()) {
            group.sort();
            desc.push_str(&format!(" links={group:?}"));
        }
        if (cmp.mtime && !ft.is_dir()) || (cmp.dir_mtime && ft.is_dir()) {
            desc.push_str(&format!(" mtime={}.{:09}", meta.mtime(), meta.mtime_nsec()));
        }
        if cmp.atime && !ft.is_dir() {
            desc.push_str(&format!(" atime={}.{:09}", meta.atime(), meta.atime_nsec()));
        }
        snap.insert(name, desc);
    }
    snap
}

/// Removes `path` even when a case left directories without write or search
/// permission (`chmod -R u-w`): unprivileged, `remove_dir_all` cannot empty
/// them, and the next run (the release build reuses these paths) would find
/// the old tree.
fn force_remove(path: &Path) {
    fn open_up(p: &Path) {
        if let Ok(m) = p.symlink_metadata()
            && m.is_dir()
        {
            let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o700));
            if let Ok(entries) = fs::read_dir(p) {
                for e in entries.flatten() {
                    open_up(&e.path());
                }
            }
        }
    }
    if fs::remove_dir_all(path).is_err() {
        open_up(path);
        let _ = fs::remove_dir_all(path);
    }
}

fn scratch_root() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("gnu_compat_fileops-trees");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    })
}

/// A time zone with daylight saving time that needs no tz database.
const TZ_NEW_YORK: &str = "EST5EDT,M3.2.0,M11.1.0";

struct TreeCase<'a> {
    name: &'a str,
    tool: &'a str,
    args: &'a [&'a str],
    stdin: Option<&'a [u8]>,
    /// A shell command run (with the system's tools) in both trees first.
    setup: Option<&'a str>,
    tz: &'a str,
    /// `_POSIX2_VERSION` for both runs (enables obsolete syntax).
    posix2_version: Option<&'a str>,
    cmp: Compare,
}

/// A writable directory on a different file system than the scratch trees
/// (as GNU's `other_partition_tmpdir`), if there is one.
fn other_fs_root() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let here = fs::metadata(scratch_root()).ok()?.dev();
        let mut candidates: Vec<PathBuf> = ["/dev/shm", "/tmp", "/var/tmp", "/run/user"]
            .iter()
            .map(PathBuf::from)
            .collect();
        if let Some(t) = std::env::var_os("TMPDIR") {
            candidates.insert(0, PathBuf::from(t));
        }
        candidates.into_iter().find_map(|c| {
            let m = fs::metadata(&c).ok()?;
            if !m.is_dir() || m.dev() == here {
                return None;
            }
            // Writable? Probe with a directory of our own.
            let probe = c.join(format!("dogfish-fileops-probe-{}", std::process::id()));
            fs::create_dir(&probe).ok()?;
            fs::remove_dir(&probe).ok()?;
            Some(c)
        })
    })
    .as_deref()
}

fn replace_bytes(data: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

impl TreeCase<'_> {
    fn flag(&mut self, flag: &str) {
        match flag {
            "mtime" => self.cmp.mtime = true,
            "atime" => {
                self.cmp.mtime = true;
                self.cmp.atime = true;
            }
            "dir_mtime" => {
                self.cmp.mtime = true;
                self.cmp.dir_mtime = true;
            }
            "no_tree" => self.cmp.no_tree = true,
            "nostderr" => self.cmp.no_stderr = true,
            "no_undo" => self.cmp.no_undo = true,
            "unordered" => self.cmp.unordered = true,
            "other_fs" => self.cmp.other_fs = true,
            "posixly_correct" => self.cmp.posixly_correct = true,
            "tz_new_york" => self.tz = TZ_NEW_YORK,
            "posix_1992" => self.posix2_version = Some("199209"),
            other => panic!("unknown flag {other}"),
        }
    }

    fn run(&self) {
        let Some(gnu) = gnu_tool(self.tool) else {
            eprintln!(
                "note: skipping — system `{}` is missing or not GNU coreutils",
                self.tool
            );
            return;
        };
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let base = scratch_root().join(format!(
            "{}-{}",
            self.name,
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        force_remove(&base);
        let (ours_dir, gnu_dir) = (base.join("dogfish"), base.join("gnu"));
        // The second file system's directory for each run (empty unless asked).
        let (ours_other, gnu_other) = if self.cmp.other_fs {
            let Some(root) = other_fs_root() else {
                eprintln!("note: skipping — no writable directory on another file system");
                return;
            };
            let tag = format!(
                "dogfish-fileops-{}-{}",
                std::process::id(),
                base.file_name().unwrap().to_string_lossy()
            );
            (
                root.join(format!("{tag}-dogfish")),
                root.join(format!("{tag}-gnu")),
            )
        } else {
            (PathBuf::new(), PathBuf::new())
        };
        for (dir, other) in [(&ours_dir, &ours_other), (&gnu_dir, &gnu_other)] {
            fs::create_dir_all(dir).unwrap();
            build_tree(dir);
            if self.cmp.other_fs {
                let _ = fs::remove_dir_all(other);
                fs::create_dir_all(other).unwrap();
            }
            if let Some(setup) = self.setup {
                let status = Command::new("sh")
                    .args(["-c", setup])
                    .current_dir(dir)
                    .env("OTHER", other)
                    .status()
                    .unwrap();
                assert!(status.success(), "setup failed: {setup}");
            }
        }
        // `@xff@` in an argument is the byte 0xff: a name that is not UTF-8.
        let args = |other: &Path| -> Vec<OsString> {
            self.args
                .iter()
                .map(|a| {
                    let a = a.replace("@OTHER@", &other.to_string_lossy());
                    OsString::from_vec(replace_bytes(a.as_bytes(), b"@xff@", b"\xff"))
                })
                .collect()
        };

        // POSIXLY_CORRECT goes through a shell, as `exec` clears it.
        let posix = |program: &Path, args: Vec<OsString>| {
            let mut c = Command::new("sh");
            c.arg("-c")
                .arg("POSIXLY_CORRECT=1 exec \"$0\" \"$@\"")
                .arg(program)
                .args(args);
            c
        };
        let mut ours = if self.cmp.posixly_correct {
            let mut v = vec![OsString::from(self.tool)];
            v.extend(args(&ours_other));
            posix(Path::new(dogfish_bin()), v)
        } else {
            let mut c = Command::new(dogfish_bin());
            c.arg(self.tool).args(args(&ours_other));
            c
        };
        ours.env("DOGFISH_HOME", base.join("dogfish-home"))
            .env("TZ", self.tz);
        if self.cmp.no_undo {
            ours.env("DOGFISH_UNDO", "off");
        }
        let mut theirs = if self.cmp.posixly_correct {
            // GNU names itself after argv[0], the full path here.
            posix(&gnu, args(&gnu_other))
        } else {
            let mut c = Command::new(&gnu);
            c.arg0(self.tool).args(args(&gnu_other));
            c
        };
        theirs.env("TZ", self.tz);
        if let Some(v) = self.posix2_version {
            ours.env("_POSIX2_VERSION", v);
            theirs.env("_POSIX2_VERSION", v);
        }
        let mut a = exec(ours, &ours_dir, self.stdin, "C");
        let mut b = exec(theirs, &gnu_dir, self.stdin, "C");
        if self.cmp.posixly_correct {
            let full = format!("{}: ", gnu.display());
            let short = format!("{}: ", self.tool);
            b.stderr = replace_bytes(&b.stderr, full.as_bytes(), short.as_bytes());
        }
        if self.cmp.other_fs {
            for (o, other) in [(&mut a, &ours_other), (&mut b, &gnu_other)] {
                let from = other.to_string_lossy().into_owned();
                o.stdout = replace_bytes(&o.stdout, from.as_bytes(), b"@OTHER@");
                o.stderr = replace_bytes(&o.stderr, from.as_bytes(), b"@OTHER@");
            }
        }

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
            self.tool,
            self.args,
            self.stdin.map(String::from_utf8_lossy),
            show(&a),
            show(&b)
        );
        let lines = |out: &[u8]| {
            let mut l: Vec<Vec<u8>> = out.split(|&c| c == b'\n').map(<[u8]>::to_vec).collect();
            if self.cmp.unordered {
                l.sort();
            }
            l
        };
        assert_eq!(lines(&a.stdout), lines(&b.stdout), "stdout differs{ctx}");
        assert_eq!(a.status.code(), b.status.code(), "exit status differs{ctx}");
        if !self.cmp.no_stderr {
            assert_eq!(a.stderr, b.stderr, "stderr differs{ctx}");
        }
        if !self.cmp.no_tree {
            let snap = |dir: &Path, other: &Path| {
                let mut s = snapshot(dir, self.cmp);
                if self.cmp.other_fs {
                    for (k, v) in snapshot(other, self.cmp) {
                        s.insert(format!("@OTHER@/{k}"), v);
                    }
                }
                s
            };
            let (sa, sb) = (snap(&ours_dir, &ours_other), snap(&gnu_dir, &gnu_other));
            if sa != sb {
                let mut diff = String::new();
                for key in sa
                    .keys()
                    .chain(sb.keys())
                    .collect::<std::collections::BTreeSet<_>>()
                {
                    let (x, y) = (sa.get(key), sb.get(key));
                    if x != y {
                        diff.push_str(&format!(
                            "  {key}:\n    dogfish: {x:?}\n    GNU:     {y:?}\n"
                        ));
                    }
                }
                panic!("resulting trees differ:\n{diff}{ctx}");
            }
        }
        // The undo trash Dogfish made on the other file system, if any.
        if let Ok(list) = fs::read_to_string(base.join("dogfish-home/trash-dirs")) {
            for dir in list.lines() {
                force_remove(Path::new(dir));
            }
        }
        force_remove(&base);
        if self.cmp.other_fs {
            let _ = fs::remove_dir_all(&ours_other);
            let _ = fs::remove_dir_all(&gnu_other);
        }
    }
}

/// `name: tool [args...] (setup("sh command"))? (stdin b"...")? (=> flag, ...)?;`
/// (`@xff@` in an argument is the byte 0xff)
/// — runs the command in two fresh scratch trees and compares everything (see
/// the module docs). Flags: `mtime`, `atime`, `dir_mtime`, `no_tree`,
/// `nostderr`, `unordered`, `tz_new_york`, `other_fs`, `posixly_correct`,
/// `posix_1992` (`_POSIX2_VERSION=199209`).
macro_rules! tree_cases {
    ($(
        $(#[$meta:meta])*
        $name:ident : $tool:literal [$($arg:expr),* $(,)?]
            $(setup($setup:expr))?
            $(stdin $input:expr)?
            $(=> $($flag:ident),+)?
        ;
    )*) => {
        $(
            $(#[$meta])*
            #[test]
            fn $name() {
                #[allow(unused_mut)]
                let mut case = TreeCase {
                    name: stringify!($name),
                    tool: $tool,
                    args: &[$($arg),*],
                    stdin: None,
                    setup: None,
                    tz: "UTC0",
                    posix2_version: None,
                    cmp: Compare::default(),
                };
                $( case.stdin = Some($input); )?
                $( case.setup = Some($setup); )?
                $( $( case.flag(stringify!($flag)); )+ )?
                case.run();
            }
        )*
    };
}

// --------------------------------------------------------- read-only cases

gnu_cases! {
    // ----------------------------------------------------------- realpath
    realpath_file: "realpath" ["a", "d/f"];
    realpath_dir_link: "realpath" ["ld", "ld/sub", "chain"];
    realpath_nested_link: "realpath" ["d/sub/lf", "d/up"];
    realpath_missing_last: "realpath" ["nope"];
    realpath_missing_middle: "realpath" ["nope/x", "a"];
    realpath_e: "realpath" ["-e", "d/f", "nope"];
    realpath_e_dangling: "realpath" ["-e", "dangling"];
    realpath_dangling: "realpath" ["dangling"];
    realpath_m: "realpath" ["-m", "/a/../b", "nope/x/..", "a/../../b"];
    realpath_m_through_file: "realpath" ["-m", "d/f/x"];
    realpath_loop: "realpath" ["loop1"];
    realpath_m_loop: "realpath" ["-m", "loop1"];
    realpath_s: "realpath" ["-s", "ld/sub/lf", "ld/../a"];
    realpath_sm: "realpath" ["-sm", "/x/./y//z/", "/.."];
    realpath_s_missing: "realpath" ["-s", "nope/x"];
    realpath_logical: "realpath" ["-L", "ld/..", "chain/.."];
    realpath_physical: "realpath" ["-P", "ld/..", "chain/.."];
    realpath_trailing_slash_file: "realpath" ["d/f/"];
    realpath_e_trailing_slash: "realpath" ["-e", "d/f/", "d/"];
    realpath_relative_to: "realpath" ["--relative-to=d", "ld/sub/lf", "a", "d"];
    realpath_relative_to_sub: "realpath" ["--relative-to=d/sub", "d/f", "/"];
    realpath_relative_base: "realpath" ["--relative-base=d", "d/f", "a", "/etc"];
    realpath_relative_both: "realpath" ["--relative-to=d/sub", "--relative-base=d", "d/f", "a"];
    realpath_relative_root: "realpath" ["--relative-to=/usr/bin", "/usr/lib", "/usr/bin", "/"];
    realpath_relative_missing: "realpath" ["--relative-to=nope/x", "a"];
    realpath_relative_m: "realpath" ["-m", "--relative-to=nope", "d"];
    realpath_zero: "realpath" ["-z", "d", "a"];
    realpath_quiet: "realpath" ["-q", "nope/x", "a"];
    realpath_empty: "realpath" [""];
    realpath_no_operand: "realpath" [];
    realpath_quoting: "realpath" ["-e", "it's/x", "a b/x", "tab\tx", "q$x", "#x"];
    realpath_double_slash: "realpath" ["//", "-s", "//a"];
    realpath_bad_option: "realpath" ["--bogus", "a"];
    realpath_missing_arg: "realpath" ["--relative-to"];

    // ----------------------------------------------------------- basename
    basename_plain: "basename" ["/usr/lib/libfoo.so"];
    basename_suffix_operand: "basename" ["/a/b/file.txt", ".txt"];
    basename_suffix_whole: "basename" ["b", "b"];
    basename_trailing_slashes: "basename" ["a///"];
    basename_slashes: "basename" ["//"];
    basename_empty: "basename" [""];
    basename_suffix_on_dir: "basename" ["/a/b/", "b"];
    basename_multiple: "basename" ["-a", "/x/y", "z/w", "/"];
    basename_s: "basename" ["-s", ".c", "x.c", "y.c", ".c"];
    basename_long_suffix: "basename" ["--suffix=.c", "dir/x.c"];
    basename_zero: "basename" ["-z", "a/b", "-a", "c/d"];
    basename_dashdash: "basename" ["--", "-a"];
    basename_partial_suffix: "basename" ["a.c", "c"];
    basename_no_operand: "basename" [];
    basename_extra_operand: "basename" ["a", "b", "c"];
    basename_bad_option: "basename" ["-x", "a"];

    // ------------------------------------------------------------ dirname
    dirname_plain: "dirname" ["/usr/lib/libfoo.so"];
    dirname_multi: "dirname" ["a", "/a", "a/b/", ".", ".."];
    dirname_slashes: "dirname" ["//", "///a//b//", "a//b", "//a"];
    dirname_empty: "dirname" [""];
    dirname_dot_component: "dirname" ["a/.", "./a"];
    dirname_zero: "dirname" ["-z", "a/b", "c"];
    dirname_no_operand: "dirname" [];
    dirname_bad_option: "dirname" ["--bogus"];

    // ------------------------------------------------------------- mktemp
    mktemp_too_few_x: "mktemp" ["foo.XX"];
    mktemp_too_few_x_tmpdir: "mktemp" ["-p", ".", "foo.XX"];
    mktemp_suffix_needs_x: "mktemp" ["--suffix=.x", "fooX"];
    mktemp_suffix_slash: "mktemp" ["--suffix=a/b", "fooXXX"];
    mktemp_implicit_suffix_slash: "mktemp" ["fooXXXX/bar"];
    mktemp_t_with_slash: "mktemp" ["-t", "a/XXX"];
    mktemp_tmpdir_absolute: "mktemp" ["-p", ".", "/abs/XXX"];
    mktemp_two_templates: "mktemp" ["a", "b"];
    mktemp_no_dir: "mktemp" ["nodir/XXX"];
    mktemp_d_no_dir: "mktemp" ["-d", "nodir/XXX"];
    mktemp_quiet: "mktemp" ["-q", "nodir/XXX"];
    mktemp_bad_option: "mktemp" ["--bogus"];
}

/// `mktemp` names are random: compare the outputs with the randomized
/// characters masked, plus what got created.
#[test]
fn mktemp_names_and_modes_match_gnu() {
    let Some(gnu) = gnu_tool("mktemp") else {
        eprintln!("note: skipping — system `mktemp` is missing or not GNU coreutils");
        return;
    };
    let cases: &[&[&str]] = &[
        &["-u"],
        &["-u", "foo.XXXXXX"],
        &["-u", "aXXXb"],
        &["-u", "--suffix=.txt", "x.XXX"],
        &["-u", "-p", "sub", "t.XXXX"],
        &["-u", "--tmpdir=sub", "t.XXXX"],
        &["-u", "-t", "t.XXXX"],
        &["-u", "nodir/XXX"],
        &[],
        &["-d"],
        &["foo.XXXXXX"],
        &["-d", "-p", "sub", "dir.XXXX"],
        &["--suffix=.log", "log.XXXXX"],
    ];
    for args in cases {
        let root = scratch_root().join(format!("mktemp-{}", args.join("_").replace('/', "%")));
        let mut outputs = Vec::new();
        for (who, dir) in [("dogfish", root.join("dogfish")), ("gnu", root.join("gnu"))] {
            fs::create_dir_all(dir.join("sub")).unwrap();
            let tmp = dir.join("tmp");
            fs::create_dir_all(&tmp).unwrap();
            let mut cmd = if who == "dogfish" {
                let mut c = Command::new(dogfish_bin());
                c.arg("mktemp");
                c
            } else {
                let mut c = Command::new(&gnu);
                c.arg0("mktemp");
                c
            };
            cmd.args(*args).env("TMPDIR", &tmp);
            let out = exec(cmd, &dir, None, "C");
            let stdout =
                String::from_utf8_lossy(&out.stdout).replace(&*dir.to_string_lossy(), "<root>");
            let created = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
            let kind = fs::symlink_metadata(dir.join(&created))
                .map(|m| {
                    format!(
                        "{} {:o}",
                        if m.is_dir() { "dir" } else { "file" },
                        m.mode() & 0o777
                    )
                })
                .unwrap_or_else(|_| "absent".into());
            outputs.push((
                stdout,
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
                kind,
            ));
        }
        let (ours, theirs) = (&outputs[0], &outputs[1]);
        // Mask the characters that differ, which must be the random ones.
        let mask = |s: &str, other: &str| -> String {
            s.chars()
                .zip(other.chars())
                .map(|(a, b)| {
                    if a != b && a.is_ascii_alphanumeric() && b.is_ascii_alphanumeric() {
                        '#'
                    } else {
                        a
                    }
                })
                .collect()
        };
        assert_eq!(
            ours.0.len(),
            theirs.0.len(),
            "mktemp {args:?}: {ours:?} vs {theirs:?}"
        );
        assert_eq!(
            mask(&ours.0, &theirs.0),
            mask(&theirs.0, &ours.0),
            "mktemp {args:?}"
        );
        assert_eq!(
            (ours.1, &ours.2, &ours.3),
            (theirs.1, &theirs.2, &theirs.3),
            "mktemp {args:?}"
        );
    }
}

// ----------------------------------------------------------- mutating cases

tree_cases! {
    // -------------------------------------------------------------- mkdir
    mkdir_verbose: "mkdir" ["-v", "n1", "n2"];
    mkdir_parents_verbose: "mkdir" ["-pv", "x/y/z"];
    mkdir_parents_partial: "mkdir" ["-pv", "d/sub/new/deeper"];
    mkdir_parents_dot_dot: "mkdir" ["-pv", "./g/../h/"];
    mkdir_exists: "mkdir" ["d", "n"];
    mkdir_parents_exists: "mkdir" ["-p", "d", "e"];
    mkdir_parents_onto_file: "mkdir" ["-p", "a"];
    mkdir_parents_through_file: "mkdir" ["-p", "a/x"];
    mkdir_parents_through_link: "mkdir" ["-p", "la/x"];
    mkdir_missing_parent: "mkdir" ["-v", "x/y", "z"];
    mkdir_mode_octal: "mkdir" ["-m", "700", "m"];
    mkdir_mode_symbolic: "mkdir" ["-m", "u=rwx,go=", "m"];
    mkdir_mode_sticky: "mkdir" ["-m", "+t", "m"];
    mkdir_mode_minus_w: "mkdir" ["-m", "-w", "m"];
    mkdir_mode_other_write: "mkdir" ["--mode=o+w", "m"];
    mkdir_mode_setgid: "mkdir" ["-m", "g+s", "m"];
    mkdir_mode_sticky_octal: "mkdir" ["-m", "1777", "m"];
    mkdir_mode_invalid: "mkdir" ["-m", "zz", "m"];
    mkdir_parents_mode: "mkdir" ["-p", "-m", "700", "q1/r/s"];
    mkdir_quoting: "mkdir" ["-v", "a b", "it's", "nl\nx", "tab\tt"];
    mkdir_empty_name: "mkdir" [""];
    mkdir_no_operand: "mkdir" [];
    mkdir_bad_option: "mkdir" ["--bogus", "x"];
    mkdir_mode_missing_arg: "mkdir" ["-m"];

    // -------------------------------------------------------------- touch
    touch_create: "touch" ["n1", "n2", "a"];
    touch_epoch: "touch" ["-d", "@0", "a", "n"] => atime;
    touch_epoch_fraction: "touch" ["-d", "@1.5", "n"] => atime;
    touch_epoch_negative: "touch" ["--date=@-1", "n"] => atime;
    touch_iso_date: "touch" ["-d", "2020-01-01", "n"] => atime;
    touch_iso_datetime: "touch" ["-d", "2020-01-01 12:34:56.789", "n"] => atime;
    touch_iso_t_zulu: "touch" ["-d", "2020-01-01T12:34:56Z", "n"] => atime;
    touch_numeric_offset: "touch" ["-d", "2020-01-01 12:00 +0200", "n"] => atime;
    touch_offset_with_colon: "touch" ["-d", "2020-01-01 12:00:00.5+01:00", "n"] => atime;
    touch_utc_word: "touch" ["-d", "2020-01-01 12:00:00 UTC", "n"] => atime;
    touch_zone_name: "touch" ["-d", "2020-01-01 12:00 PST", "n"] => atime;
    touch_zone_correction: "touch" ["-d", "2020-01-01 12:00 UTC+2", "n"] => atime;
    touch_us_date: "touch" ["-d", "1/2/20", "n"] => atime;
    touch_month_name: "touch" ["-d", "Jan 5 2020", "n"] => atime;
    touch_day_month_year_time: "touch" ["-d", "5 Jan 2020 10:00", "n"] => atime;
    touch_compact_date: "touch" ["-d", "20200105", "n"] => atime;
    touch_relative_days: "touch" ["-d", "2020-01-01 +2 days", "n"] => atime;
    touch_relative_month_overflow: "touch" ["-d", "2021-01-31 +1 month", "n"] => atime;
    touch_relative_ago: "touch" ["-d", "2020-06-01 1 hour ago", "n"] => atime;
    touch_pm: "touch" ["-d", "2020-06-01 10:30pm", "n"] => atime;
    touch_midnight_am: "touch" ["-d", "2020-06-01 12am", "n"] => atime;
    touch_time_before_date: "touch" ["-d", "10am 2020-06-01", "n"] => atime;
    touch_local_dst: "touch" ["-d", "2020-07-01 12:00", "n"] => atime, tz_new_york;
    touch_local_winter: "touch" ["-d", "2020-01-01", "n"] => atime, tz_new_york;
    touch_invalid_date: "touch" ["-d", "garbage", "n"];
    touch_invalid_day: "touch" ["-d", "2020-02-30", "n"];
    touch_invalid_leap_day: "touch" ["-d", "Feb 29 2021", "n"];
    touch_invalid_hour: "touch" ["-d", "2020-01-01 25:00", "n"];
    touch_stamp: "touch" ["-t", "202001011200", "n"] => atime;
    touch_stamp_seconds: "touch" ["-t", "2001011200.30", "n"] => atime;
    touch_stamp_two_digit_year: "touch" ["-t", "9901011200", "n"] => atime;
    touch_stamp_leap_second: "touch" ["-t", "202001011200.60", "n"] => atime;
    touch_stamp_local: "touch" ["-t", "202007011200", "n"] => atime, tz_new_york;
    touch_stamp_invalid: "touch" ["-t", "2020", "n"];
    touch_stamp_bad_seconds: "touch" ["-t", "202001011200.61", "n"];
    touch_access_only: "touch" ["-a", "-d", "@100", "a"] => atime;
    touch_modify_only: "touch" ["-m", "-d", "@200", "a"] => atime;
    touch_time_word: "touch" ["--time=atime", "-d", "@5", "a"] => atime;
    touch_time_word_modify: "touch" ["--time=mtime", "-d", "@5", "a"] => atime;
    touch_time_word_bad: "touch" ["--time=bogus", "a"];
    touch_reference: "touch" ["-r", "old", "a", "n"] => atime;
    touch_reference_with_date: "touch" ["-r", "old", "-d", "+1 day", "a"] => atime;
    touch_reference_missing: "touch" ["-r", "nope", "a"];
    touch_two_sources: "touch" ["-r", "old", "-t", "202001011200", "a"];
    touch_no_create: "touch" ["-c", "nofile", "a"];
    touch_missing_dir: "touch" ["-d", "@0", "nodir/z"];
    touch_symlink_itself: "touch" ["-h", "-d", "@7", "dl", "la"] => atime;
    touch_symlink_missing: "touch" ["-h", "nodl"];
    touch_follows_symlink: "touch" ["-d", "@9", "la"] => mtime;
    touch_directory: "touch" ["-d", "@0", "e"] => atime, dir_mtime;
    touch_no_operand: "touch" [];
    touch_stdout: "touch" ["-"];
    touch_stdout_no_create: "touch" ["-c", "-", "n"];
    touch_stdout_no_dereference: "touch" ["-h", "-"];
    touch_now_is_no_date: "touch" ["-d", "now", "n"];
    touch_relative_now: "touch" ["-d", "+0 sec", "n"];
    touch_obsolete_stamp: "touch" ["01010000", "n"] => atime, posix_1992;
    touch_obsolete_stamp_year: "touch" ["0101000099", "n"] => atime, posix_1992;
    touch_obsolete_stamp_dash_dash: "touch" ["--", "1231235969", "n"] => atime, posix_1992;
    touch_obsolete_pre_2000_is_a_file: "touch" ["0101000000", "n"] => posix_1992;
    touch_obsolete_century_is_a_file: "touch" ["200001010000", "n"] => posix_1992;
    touch_obsolete_invalid_is_a_file: "touch" ["13010000", "n"] => posix_1992;
    touch_obsolete_one_operand_is_a_file: "touch" ["01010000"] => posix_1992;
    touch_obsolete_needs_old_posix: "touch" ["01010000", "n"];
    touch_obsolete_not_after_date: "touch" ["-d", "@0", "01010000", "n"] => atime, posix_1992;
    touch_tz_in_date_string: "touch" ["-d", "TZ=\"Asia/Tokyo\" 2020-01-01 09:00", "n"] => atime;
    touch_military_zone: "touch" ["-d", "2020-01-01 09:00 Q", "n"] => atime;
    touch_ordinal_weekday: "touch" ["-d", "2020-01-01 third fri", "n"] => atime;
    touch_two_digit_year_rel: "touch" ["-d", "Sun, 90-12-11 + 3 days - 90 minutes", "n"] => atime;
    touch_stamp_trailing_dot: "touch" ["-t", "202001011200.", "n"];
    touch_stamp_odd_length: "touch" ["-t", "2020010112001", "n"];
    touch_stamp_century_short: "touch" ["-t", "6901011200", "n"] => atime;
    touch_stamp_feb_30: "touch" ["-t", "202002301200", "n"];

    // ----------------------------------------------------------------- ln
    ln_hard_verbose: "ln" ["-v", "a", "hl"];
    ln_symbolic_verbose: "ln" ["-sv", "a", "s"];
    ln_exists: "ln" ["a", "b"];
    ln_symbolic_exists: "ln" ["-s", "a", "b"];
    ln_force: "ln" ["-fv", "a", "b"];
    ln_symbolic_force: "ln" ["-sfv", "a", "b"];
    ln_single_operand: "ln" ["-sv", "d/sub/g"];
    ln_single_operand_exists: "ln" ["a"];
    ln_into_directory: "ln" ["-v", "a", "b", "d"];
    ln_target_not_directory: "ln" ["-s", "a", "b", "nope"];
    ln_through_dir_symlink: "ln" ["-sv", "a", "ld"];
    ln_no_dereference: "ln" ["-snfv", "b", "ld"];
    ln_no_target_directory: "ln" ["-sT", "a", "e"];
    ln_no_target_directory_force: "ln" ["-sTf", "a", "e"];
    ln_target_directory: "ln" ["-t", "e", "-sv", "../a", "../b"];
    ln_relative: "ln" ["-srv", "a", "d/ra"];
    ln_relative_through_link: "ln" ["-srv", "d/up", "e/rb"];
    ln_relative_needs_symbolic: "ln" ["-r", "a", "x"];
    ln_backup: "ln" ["-bv", "a", "b"];
    ln_backup_suffix: "ln" ["-S", ".old", "-bv", "a", "b"];
    ln_backup_numbered: "ln" ["--backup=numbered", "-sfv", "a", "num"];
    ln_hard_link_directory: "ln" ["d", "hd"];
    ln_missing_source: "ln" ["nope", "x"];
    ln_missing_directory: "ln" ["-s", "a", "nodir/x"];
    ln_no_operand: "ln" [];
    ln_extra_operand: "ln" ["-T", "a", "b", "c"];
    ln_interactive_no: "ln" ["-iv", "a", "b"] stdin b"n\n";
    ln_interactive_yes: "ln" ["-iv", "a", "b"] stdin b"y\n";
    ln_same_file: "ln" ["-f", "a", "a"];
    ln_symbolic_same_file: "ln" ["-sf", "a", "a"];
    ln_replace_hard_link: "ln" ["-fv", "h1", "h2"];
    ln_logical: "ln" ["-L", "la", "hl"];
    ln_physical: "ln" ["la", "hp"];
    ln_empty_target: "ln" ["-s", "", "empt"];

    // -------------------------------------------------------------- chmod
    chmod_octal: "chmod" ["-v", "755", "a"];
    chmod_retained: "chmod" ["-v", "644", "a"];
    chmod_changes: "chmod" ["-c", "755", "a", "b", "exe"];
    chmod_symbolic: "chmod" ["-v", "u+x,g-w,o=", "a"];
    chmod_capital_x: "chmod" ["-v", "a+X", "a", "exe", "e"];
    chmod_equals: "chmod" ["-v", "=rwx", "a"];
    chmod_minus_w: "chmod" ["-v", "-w", "a"];
    chmod_minus_w_umask_surprise: "chmod" ["-v", "-w", "wide"];
    chmod_dash_rwx: "chmod" ["-v", "-rwx", "a"];
    chmod_special_bits: "chmod" ["-v", "u+s,g+s,+t", "exe"];
    chmod_all_bits: "chmod" ["-v", "7777", "a"];
    chmod_zero: "chmod" ["-v", "0", "a"];
    chmod_copy_bits: "chmod" ["-v", "go=u", "exe"];
    chmod_copy_group: "chmod" ["-v", "u=g", "a"];
    chmod_noop_clause: "chmod" ["-v", "u+", "a"];
    chmod_x_if_any: "chmod" ["-v", "g-x,+X", "exe"];
    chmod_recursive: "chmod" ["-vR", "750", "d"];
    chmod_recursive_changes: "chmod" ["-cR", "o-r", "d"];
    // Deeper (130 levels) than the recursive tools keep directories open.
    chmod_recursive_deep: "chmod" ["-vR", "u-w", "deep"] setup("mkdir -p deep/$(printf 'k/%.0s' $(seq 130)) && echo bottom > deep/$(printf 'k/%.0s' $(seq 130))f");
    chmod_recursive_operands: "chmod" ["-R", "g+w", "d", "ld", "p", "deep"] setup("mkdir -p deep/$(printf 'k/%.0s' $(seq 130)) && echo bottom > deep/$(printf 'k/%.0s' $(seq 130))f");
    chmod_dir_keeps_setgid: "chmod" ["-v", "755", "sg"];
    chmod_dir_clears_setgid: "chmod" ["-v", "00755", "sg"];
    chmod_symlink_operand: "chmod" ["-v", "700", "la"];
    chmod_dangling: "chmod" ["-v", "755", "dl"];
    chmod_missing_file: "chmod" ["-v", "755", "nope", "a"];
    chmod_quiet_missing: "chmod" ["-f", "755", "nope"];
    chmod_reference: "chmod" ["-v", "--reference=exe", "a", "b"];
    chmod_reference_missing: "chmod" ["--reference=nope", "a"];
    chmod_invalid: "chmod" ["zz", "a"];
    chmod_invalid_octal: "chmod" ["8", "a"];
    chmod_invalid_perm: "chmod" ["u+z", "a"];
    chmod_invalid_comma: "chmod" [",", "a"];
    chmod_missing_after_mode: "chmod" ["755"];
    chmod_no_operand: "chmod" [];
    chmod_bad_option: "chmod" ["-Q", "755", "a"];
    // fts keeps one of several trailing slashes, in messages too.
    chmod_recursive_collapsed_slashes: "chmod" ["-vR", "750", "d///", "nope//"];

    // -------------------------------------------------------------- chown
    chown_user: "chown" ["-v", "nobody", "a"];
    chown_changes: "chown" ["-c", "nobody", "a", "b"];
    chown_user_group: "chown" ["-v", "nobody:0", "a"];
    chown_numeric: "chown" ["-v", "1234:5678", "a"];
    chown_group_only: "chown" ["-v", ":1", "a"];
    chown_dot_separator: "chown" ["-v", ".1", "a"];
    chown_login_group: "chown" ["-v", "root:", "a"];
    chown_numeric_login_group: "chown" ["-v", "0.", "a"];
    chown_invalid_user: "chown" ["nosuchuser", "a"];
    chown_invalid_group: "chown" [":nosuchgrp", "a"];
    chown_invalid_spec: "chown" ["a:b:c", "a"];
    chown_reserved_id: "chown" ["4294967295", "a"];
    chown_empty_spec: "chown" ["-v", "", "a"];
    chown_colon_only: "chown" ["-v", ":", "a"];
    chown_no_dereference: "chown" ["-v", "-h", "99", "la"];
    chown_dereference: "chown" ["-v", "99", "la"];
    chown_recursive: "chown" ["-vR", "99", "d"];
    chown_recursive_cmdline_link: "chown" ["-RHv", "99", "ld"];
    chown_from: "chown" ["-v", "--from=0", "99", "a", "b"];
    chown_reference: "chown" ["-v", "--reference=a", "b"];
    chown_missing_file: "chown" ["-v", "99", "nope"];
    chown_missing_after_spec: "chown" ["root"];
    chown_no_operand: "chown" [];
    chown_recursive_collapsed_slashes: "chown" ["-vR", "99", "d///", "nope//"];

    // ----------------------------------------------------------------- rm
    rm_verbose: "rm" ["-v", "a", "b"];
    rm_missing: "rm" ["nope", "a"];
    rm_force_missing: "rm" ["-fv", "nope", "a"];
    rm_directory: "rm" ["d"];
    rm_dir_flag_nonempty: "rm" ["-d", "d"];
    rm_dir_flag_empty: "rm" ["-dv", "e"];
    rm_recursive: "rm" ["-rv", "d"];
    rm_recursive_nested: "rm" ["-rv", "p"];
    // Deeper (130 levels) than rm keeps directories open.
    rm_recursive_deep: "rm" ["-rv", "deep"] setup("mkdir -p deep/$(printf 'k/%.0s' $(seq 130)) && echo bottom > deep/$(printf 'k/%.0s' $(seq 130))f");
    rm_recursive_quiet: "rm" ["-r", "d", "p", "p2", "deep"] setup("mkdir -p deep/$(printf 'k/%.0s' $(seq 130)) && echo bottom > deep/$(printf 'k/%.0s' $(seq 130))f");
    rm_trailing_slash_file: "rm" ["a/"];
    rm_force_trailing_slash_file: "rm" ["-f", "a/"];
    rm_recursive_trailing_slash: "rm" ["-rfv", "d/"];
    rm_no_operand: "rm" [];
    rm_force_no_operand: "rm" ["-f"];
    rm_interactive: "rm" ["-iv", "a", "b"] stdin b"y\nn\n";
    rm_interactive_eof: "rm" ["-i", "a"];
    rm_interactive_types: "rm" ["-i", "empty", "la", "dl", "exe"] stdin b"n\nn\ny\nn\n";
    rm_once_few: "rm" ["-I", "a", "b"] stdin b"n\n";
    rm_once_many_no: "rm" ["-I", "a", "b", "c.txt", "empty"] stdin b"n\n";
    rm_once_many_yes: "rm" ["-Iv", "a", "b", "c.txt", "empty"] stdin b"y\n";
    rm_once_recursive: "rm" ["-rI", "d"] stdin b"y\n";
    rm_recursive_interactive: "rm" ["-ri", "d"] stdin b"y\nn\ny\ny\ny\ny\ny\n";
    rm_recursive_interactive_all: "rm" ["-riv", "p"] stdin b"y\ny\ny\n";
    rm_recursive_interactive_empty: "rm" ["-ri", "e"] stdin b"y\n";
    rm_interactive_dir_without_r: "rm" ["-i", "e"];
    rm_interactive_dir_flag: "rm" ["-di", "e"] stdin b"y\n";
    rm_interactive_never: "rm" ["--interactive=never", "-v", "a"];
    rm_interactive_bad: "rm" ["--interactive=bogus", "a"];
    rm_force_then_interactive: "rm" ["-fi", "a"] stdin b"n\n";
    rm_dot: "rm" ["-r", "."];
    rm_dotdot: "rm" ["-r", "d/.."];
    rm_dot_slash: "rm" ["-rf", "./"];
    rm_dot_without_r: "rm" ["."];
    rm_dot_dir_flag: "rm" ["-d", "."];
    rm_symlink_to_dir: "rm" ["-rv", "ld"];
    rm_dashdash: "rm" ["-v", "--", "a", "./b"];
    rm_one_file_system: "rm" ["-rv", "--one-file-system", "d"];
    rm_quoting: "rm" ["-v", "spa ce"];
    rm_bad_option: "rm" ["-X", "a"];
    // fts keeps one of several trailing slashes, in messages too.
    rm_recursive_collapsed_slashes: "rm" ["-rv", "d///"];
    rm_dir_flag_collapsed_slashes: "rm" ["-dv", "e//"];
    rm_directory_collapsed_slashes: "rm" ["d///"];
    rm_missing_collapsed_slashes: "rm" ["nope///", "a//"];
    // Deeper than PATH_MAX (140 levels of 40-byte names) and than the
    // directories rm keeps open.
    // (Perl: a shell's `cd` may build the full path, which is too long.)
    rm_recursive_deeper_than_path_max: "rm" ["-rv", "long"] setup("perl -e 'my $n = q(n) x 40; mkdir q(long) and chdir q(long) or die $!; for my $i (1..140) { mkdir $n and chdir $n or die $!; if ($i % 30 == 0) { open(my $f, q(>), qq(f$i)) or die $!; mkdir qq(s$i) or die $! } } open(my $f, q(>), q(f)) or die $!'");
    rm_recursive_deeper_than_path_max_quiet: "rm" ["-r", "long", "a"] setup("perl -e 'my $n = q(n) x 40; mkdir q(long) and chdir q(long) or die $!; for my $i (1..140) { mkdir $n and chdir $n or die $! } symlink(q(/), q(root)) or die $!; symlink(q(..), q(up)) or die $!'");

    // ----------------------------------------------------------------- cp
    cp_verbose: "cp" ["-v", "a", "c"];
    cp_into_directory: "cp" ["-v", "a", "b", "c.txt", "e"];
    cp_recursive: "cp" ["-rv", "d", "e"] => unordered;
    cp_overwrite_keeps_mode: "cp" ["-v", "exe", "b"];
    cp_new_file_mode: "cp" ["exe", "x"];
    cp_new_file_umask: "cp" ["wide", "x"];
    cp_missing_file_operand: "cp" [];
    cp_missing_destination: "cp" ["a"];
    cp_target_not_directory: "cp" ["a", "b", "c.txt"];
    cp_target_missing: "cp" ["a", "b", "zz"];
    cp_missing_source: "cp" ["nope", "a", "e"];
    cp_directory_without_r: "cp" ["d", "x"];
    cp_into_itself: "cp" ["-r", "d", "d/sub"] => no_tree;
    cp_same_file: "cp" ["a", "a"];
    cp_same_file_dot: "cp" ["a", "./a"];
    cp_same_file_link: "cp" ["a", "la"];
    cp_same_symlink: "cp" ["la", "la"];
    cp_interactive_no: "cp" ["-i", "a", "b"] stdin b"n\n";
    cp_interactive_yes: "cp" ["-iv", "a", "b"] stdin b"y\n";
    cp_interactive_new_file: "cp" ["-iv", "a", "x"];
    cp_force_then_interactive: "cp" ["-fi", "a", "b"] stdin b"n\n";
    cp_backup: "cp" ["-bv", "a", "b"];
    cp_backup_replaces_old_backup: "cp" ["-bv", "a", "c.txt"];
    cp_backup_numbered: "cp" ["--backup=numbered", "-v", "a", "b"];
    cp_backup_existing_numbered: "cp" ["--backup=existing", "-v", "a", "num"];
    cp_backup_existing_simple: "cp" ["--backup=existing", "-v", "a", "b"];
    cp_backup_suffix: "cp" ["-S", ".bak", "-bv", "a", "b"];
    cp_backup_none: "cp" ["--backup=none", "-v", "a", "b"];
    cp_backup_bad: "cp" ["--backup=bogus", "a", "b"];
    cp_update_skips_newer: "cp" ["-uv", "old", "new"];
    cp_update_copies_older: "cp" ["-uv", "new", "old"];
    cp_update_none: "cp" ["--update=none", "-v", "a", "b"];
    cp_update_all: "cp" ["--update=all", "-v", "old", "new"];
    cp_update_bad: "cp" ["--update=bad", "a", "b"];
    cp_follows_symlink: "cp" ["-v", "la", "x"];
    cp_no_dereference: "cp" ["-Pv", "la", "x"];
    cp_d_copies_link: "cp" ["-dv", "la", "x"];
    cp_dangling_source: "cp" ["-v", "dl", "x"];
    cp_recursive_link_operand: "cp" ["-rv", "ld", "x"] => unordered;
    cp_recursive_follow_cmdline: "cp" ["-rHv", "ld", "x"] => unordered;
    cp_recursive_dereference: "cp" ["-rLv", "d", "x"] => unordered;
    cp_target_directory: "cp" ["-t", "e", "-v", "a", "b"];
    cp_target_directory_missing: "cp" ["-t", "nodir", "a"];
    cp_target_directory_file: "cp" ["-t", "a", "b"];
    cp_no_target_directory_onto_dir: "cp" ["-Tv", "a", "e"];
    cp_no_target_directory_tree: "cp" ["-Trv", "d", "x"] => unordered;
    cp_no_target_directory_merge: "cp" ["-rTv", "d", "e"] => unordered;
    cp_no_target_directory_extra: "cp" ["-T", "a", "b", "c"];
    cp_target_and_no_target: "cp" ["-t", "e", "-T", "a"];
    cp_hard_link: "cp" ["-lv", "a", "hl"];
    cp_symbolic_link: "cp" ["-sv", "a", "sl"];
    cp_symbolic_link_elsewhere: "cp" ["-sv", "a", "d/sl"];
    cp_hard_and_symbolic: "cp" ["-ls", "a", "x"];
    cp_missing_directory: "cp" ["-v", "a", "nodir/x"];
    cp_recursive_missing_directory: "cp" ["-rv", "d", "nodir/x"];
    cp_directory_slash: "cp" ["-rv", "d/", "e/"] => unordered;
    cp_file_slash_destination: "cp" ["-v", "a", "b/"];
    cp_archive: "cp" ["-av", "d", "x"] => mtime, unordered;
    cp_preserve_default: "cp" ["-p", "old", "x"] => mtime;
    cp_preserve_mode: "cp" ["--preserve=mode", "exe", "b"];
    cp_preserve_timestamps: "cp" ["--preserve=timestamps", "old", "x"] => mtime;
    cp_preserve_bad: "cp" ["--preserve=bogus", "a", "x"];
    cp_no_preserve_mode: "cp" ["-p", "--no-preserve=mode", "exe", "x"] => mtime;
    cp_writes_through_symlink: "cp" ["-v", "a", "lsub"];
    cp_dangling_destination: "cp" ["-v", "a", "dl"];
    cp_remove_destination: "cp" ["--remove-destination", "-v", "a", "la"];
    cp_force: "cp" ["-fv", "a", "ro"];
    cp_read_only_destination: "cp" ["a", "ro"];
    cp_parents: "cp" ["--parents", "-v", "d/sub/g", "e"];
    cp_parents_flat: "cp" ["--parents", "-v", "a", "e"];
    cp_duplicate_source: "cp" ["-v", "b", "b", "b", "e"];
    cp_mixed_sources: "cp" ["-v", "a", "d/sub", "d"];
    cp_directory_onto_file: "cp" ["-rv", "e", "a"];
    cp_preserves_hard_links: "cp" ["-a", "h1", "h2", "e"];
    cp_attributes_only: "cp" ["--attributes-only", "a", "x"];
    // A symlink copied onto what it resolves to (GNU's same_file_ok).
    cp_no_dereference_symlink_onto_referent: "cp" ["-d", "la", "a"];
    cp_archive_symlink_onto_referent: "cp" ["-a", "la", "a"];
    cp_remove_destination_symlink_onto_referent: "cp" ["--remove-destination", "-d", "la", "a"];
    cp_symlink_onto_referent: "cp" ["la", "a"];
    cp_backup_onto_symlink_to_source: "cp" ["-bv", "a", "la"];
    cp_backup_no_dereference_onto_symlink_to_source: "cp" ["-bdv", "a", "la"];
    cp_hard_link_onto_symlink_to_source: "cp" ["-l", "a", "la"];
    cp_symbolic_link_onto_symlink_to_source: "cp" ["-s", "a", "la"];
    cp_force_symbolic_link_onto_symlink_to_source: "cp" ["-sf", "a", "la"];
    cp_backup_hard_links: "cp" ["-bv", "h1", "h2"] => no_undo;
    cp_attributes_only_keeps_linked_contents: "cp" ["-a", "--attributes-only", "a", "h1"] => mtime;
    cp_strip_trailing_slashes: "cp" ["-rv", "--strip-trailing-slashes", "d/", "x"] => unordered;
    cp_quoting: "cp" ["-v", "spa ce", "it's"];
    cp_bad_option: "cp" ["--bogus", "a", "b"];
    cp_bad_short_option: "cp" ["-q", "a", "b"];
    cp_missing_option_argument: "cp" ["-S"];

    // ----------------------------------------------------------------- mv
    mv_verbose: "mv" ["-v", "a", "z"];
    mv_missing_source: "mv" ["nope", "x"];
    mv_missing_file_operand: "mv" [];
    mv_missing_destination: "mv" ["a"];
    mv_same_file: "mv" ["a", "a"];
    mv_same_file_dot: "mv" ["a", "./a"];
    // A symlink moved onto what it resolves to would destroy the file.
    mv_symlink_onto_referent: "mv" ["la", "a"];
    mv_symlink_chain_onto_referent: "mv" ["lla", "a"];
    mv_symlink_onto_referent_by_link: "mv" ["lh1", "h1"];
    mv_symlink_onto_other_hard_link: "mv" ["-v", "lh1", "h2"] => no_undo;
    mv_backup_symlink_onto_referent: "mv" ["-bv", "la", "a"];
    mv_hard_links: "mv" ["h1", "h2"];
    mv_backup_hard_links: "mv" ["-bv", "h1", "h2"];
    mv_into_itself: "mv" ["d", "d/sub"];
    mv_into_directory: "mv" ["-v", "a", "b", "e"];
    mv_into_directory_partial: "mv" ["-v", "a", "nope", "b", "e"];
    mv_target_not_directory: "mv" ["a", "b", "c.txt"];
    mv_no_clobber: "mv" ["-n", "a", "b"];
    mv_no_clobber_verbose: "mv" ["-nv", "a", "b"];
    mv_interactive_no: "mv" ["-i", "a", "b"] stdin b"n\n";
    mv_interactive_yes: "mv" ["-iv", "a", "b"] stdin b"y\n";
    mv_force_then_interactive: "mv" ["-fi", "a", "b"] stdin b"n\n";
    mv_interactive_then_force: "mv" ["-if", "a", "b"] stdin b"n\n";
    mv_backup: "mv" ["-bv", "a", "b"];
    mv_backup_numbered: "mv" ["--backup=numbered", "-v", "a", "num"];
    mv_backup_suffix: "mv" ["-S", ".bak", "-bv", "a", "b"];
    mv_backup_replaces_old_backup: "mv" ["-bv", "a", "c.txt"];
    mv_update_skips_newer: "mv" ["-uv", "old", "new"];
    mv_update_moves_older: "mv" ["-uv", "new", "old"];
    mv_update_none: "mv" ["--update=none", "-v", "a", "b"];
    mv_no_target_directory_onto_dir: "mv" ["-Tv", "a", "e"];
    mv_no_target_directory_nonempty: "mv" ["-Tv", "e", "d"];
    mv_no_target_directory_empty: "mv" ["-Tv", "d", "e"];
    mv_no_target_directory_dir_onto_file: "mv" ["-T", "e", "a"];
    mv_target_directory: "mv" ["-t", "e", "-v", "a", "b"];
    mv_dir_onto_nonempty: "mv" ["q", "p2"];
    mv_dir_onto_empty: "mv" ["-v", "q", "p"];
    mv_trailing_slash_file: "mv" ["-v", "d/f", "d/f/"];
    mv_trailing_slash_destination: "mv" ["-v", "d/sub", "e2/"];
    mv_strip_trailing_slashes: "mv" ["--strip-trailing-slashes", "-v", "e/", "e2"];
    mv_symlink: "mv" ["-v", "la", "la2"];
    mv_dangling_symlink: "mv" ["-v", "dl", "dl2"];
    mv_hard_link_pair: "mv" ["-v", "h1", "e"];
    mv_keeps_times: "mv" ["old", "x"] => mtime;
    mv_quoting: "mv" ["-v", "spa ce", "it's"];
    mv_bad_option: "mv" ["--bogus", "a", "b"];
}

// ------------------------------------- cp, mv and ln: GNU's copy.c and ln.c

tree_cases! {
    // Same-file and backup rules.
    cp_backup_would_destroy_source: "cp" ["--backup=simple", "c.txt~", "c.txt"];
    mv_backup_would_destroy_source: "mv" ["--backup=simple", "c.txt~", "c.txt"];
    cp_force_backup_onto_itself: "cp" ["--force", "--backup=simple", "--suffix=.b", "a", "a"];
    cp_force_backup_onto_itself_t: "cp" ["-T", "--force", "--backup=simple", "--suffix=.b", "a", "a"];
    cp_backup_suffix_orig: "cp" ["--backup=simple", "--suffix=.orig", "-v", "a", "b"];
    mv_backup_suffix_orig: "mv" ["--backup=simple", "--suffix=.orig", "-v", "a", "b"];
    ln_backup_suffix_orig: "ln" ["--backup=simple", "--suffix=.orig", "-v", "a", "b"];
    ln_force_backup_onto_own_backup: "ln" ["-fv", "--backup=simple", "a", "b"] setup("ln b b~");
    ln_symbolic_force_backup_onto_own_backup: "ln" ["-sfv", "--backup=simple", "a", "b"] setup("ln b b~");
    cp_backup_symlink_onto_referent: "cp" ["-b", "la", "a"];
    cp_backup_force_symlink_onto_referent: "cp" ["-bf", "la", "a"];
    cp_force_hard_link_onto_symlink: "cp" ["-fl", "a", "la"];
    cp_backup_hard_link_onto_symlink: "cp" ["-bl", "a", "la"];
    cp_hard_link_symlink_onto_symlink: "cp" ["-dl", "la", "lla"];
    cp_force_symbolic_onto_hard_link: "cp" ["-sf", "h1", "h2"];
    cp_no_dereference_hard_linked_symlinks: "cp" ["-d", "ll1", "lh1"] setup("ln lh1 ll1");

    // Option parsing (glibc getopt_long and the order GNU checks things in).
    cp_keep_directory_symlink_is_9_5: "cp" ["--keep-directory-symlink", "a", "n"];
    cp_abbreviated_long_options: "cp" ["--verb", "--back=numbered", "a", "b"];
    cp_ambiguous_option: "cp" ["--re", "a", "n"];
    cp_multiple_target_directories: "cp" ["-t", "d", "-t", "e", "a"];
    mv_multiple_target_directories: "mv" ["-t", "d", "--target-directory=e", "a"];
    ln_multiple_target_directories: "ln" ["-t", "d", "-t", "e", "a"];
    ln_target_directory_checked_while_parsing: "ln" ["-t", "nodir", "--bogus", "a"];
    ln_target_directory_not_a_directory: "ln" ["-t", "a", "-Q", "b"];
    ln_bad_option_before_target_directory: "ln" ["--bogus", "-t", "nodir", "a"];
    mv_last_update_wins: "mv" ["--update=none", "--update=all", "-v", "a", "b"];
    cp_last_update_wins: "cp" ["--update=all", "--update=none", "-v", "a", "b"];
    mv_debug_is_verbose: "mv" ["--debug", "a", "n"];
    cp_permuted_options: "cp" ["a", "n", "-v"];
    cp_posixly_correct_stops_at_operand: "cp" ["a", "n", "-v"] => posixly_correct;

    // Destinations created earlier in the same run are never overwritten.
    cp_will_not_overwrite_just_created: "cp" ["a", "m/a", "e"] setup("mkdir m && echo MA > m/a");
    cp_numbered_backup_just_created: "cp" ["--backup=numbered", "-v", "a", "m/a", "e"]
        setup("mkdir m && echo MA > m/a");
    mv_will_not_overwrite_just_created: "mv" ["a", "m/a", "e"] setup("mkdir m && echo MA > m/a");
    mv_hard_link_pair_and_other: "mv" ["h1", "m/h2", "h2", "e"] setup("mkdir m && echo M > m/h2");
    ln_will_not_overwrite_just_created: "ln" ["-f", "a", "m/a", "e"] setup("mkdir m && echo MA > m/a");
    cp_through_just_created_symlink: "cp" ["-dR", "la", "m/la", "e"] setup("mkdir m && echo M > m/la");
    cp_through_just_created_symlink_existing: "cp" ["-dR", "la", "m/la", "e"]
        setup("mkdir m && echo M > m/la && echo EA > e/a");
    cp_source_file_twice: "cp" ["a", "./a", "e"];
    cp_source_dir_twice: "cp" ["-a", "./d", "d", "e"] => mtime;
    cp_source_dir_twice_into_itself: "cp" ["-a", "./e", "e", "e/"] => no_tree;
    cp_backup_source_twice: "cp" ["--backup", "a", "a", "e"];
    cp_numbered_backup_source_twice: "cp" ["--backup=numbered", "a", "a", "e"];
    mv_source_twice: "mv" ["a", "a", "e"];
    mv_source_twice_dot: "mv" ["./a", "a", "e"];
    mv_source_dir_twice_into_itself: "mv" ["-v", "./e", "e", "e/"];

    // Moving across file systems: a copy that keeps hard links, special files
    // and attributes, then removal of the source.
    mv_xdev_file: "mv" ["-v", "a", "@OTHER@"] => other_fs;
    mv_xdev_replace: "mv" ["-v", "a", "@OTHER@/b"] setup("echo OB > \"$OTHER/b\"") => other_fs;
    mv_xdev_backup: "mv" ["-vb", "a", "@OTHER@/b"] setup("echo OB > \"$OTHER/b\"") => other_fs;
    mv_xdev_no_copy: "mv" ["--no-copy", "a", "d", "@OTHER@"] => other_fs;
    mv_xdev_tree: "mv" ["-v", "d", "@OTHER@"] => other_fs, unordered;
    mv_xdev_hard_links: "mv" ["-v", "h1", "h2", "@OTHER@"] => other_fs;
    mv_xdev_hard_links_in_tree: "mv" ["d", "@OTHER@"] setup("ln d/f d/sub/f2") => other_fs;
    mv_xdev_symlinks: "mv" ["-v", "la", "dl", "ld", "@OTHER@"] => other_fs;
    mv_xdev_fifo: "mv" ["-v", "fifo", "@OTHER@"] setup("mkfifo fifo") => other_fs;
    mv_xdev_onto_empty_dir: "mv" ["-Tv", "d", "@OTHER@/e"] setup("mkdir \"$OTHER/e\"")
        => other_fs, unordered;
    mv_xdev_onto_full_dir: "mv" ["-T", "d", "@OTHER@/full"]
        setup("mkdir \"$OTHER/full\" && touch \"$OTHER/full/x\"") => other_fs;
    mv_xdev_dir_onto_file: "mv" ["-T", "d", "@OTHER@/b"] setup("echo OB > \"$OTHER/b\"") => other_fs;
    mv_xdev_file_onto_dir: "mv" ["-T", "a", "@OTHER@/e"] setup("mkdir \"$OTHER/e\"") => other_fs;
    mv_xdev_keeps_attributes: "mv" ["old", "exe", "ro", "@OTHER@"] => other_fs, mtime;
    mv_xdev_update_skips_newer: "mv" ["-uv", "old", "@OTHER@/b"] setup("echo OB > \"$OTHER/b\"")
        => other_fs;
    mv_xdev_no_clobber: "mv" ["-n", "a", "@OTHER@/b"] setup("echo OB > \"$OTHER/b\"") => other_fs;
    mv_xdev_interactive_no: "mv" ["-i", "a", "@OTHER@/b"]
        setup("echo OB > \"$OTHER/b\"") stdin b"n\n" => other_fs;
    mv_xdev_interactive_yes: "mv" ["-iv", "a", "@OTHER@/b"]
        setup("echo OB > \"$OTHER/b\"") stdin b"y\n" => other_fs;

    // FIFOs and other special files.
    cp_recursive_fifo: "cp" ["-Rv", "fifo", "x"] setup("mkfifo fifo");
    cp_recursive_fifo_onto_file: "cp" ["-Rv", "fifo", "b"] setup("mkfifo fifo");
    cp_archive_fifo: "cp" ["-a", "fifo", "x"] setup("mkfifo -m 640 fifo");
    cp_recursive_tree_with_fifo: "cp" ["-R", "m", "x"] setup("mkdir m && mkfifo m/p && echo F > m/f");

    // --link, --preserve=links and dereferencing.
    cp_link_follow_command_line: "cp" ["--link", "-H", "la", "x"];
    cp_link_follow_always: "cp" ["--link", "-L", "la", "x"];
    cp_link_physical: "cp" ["--link", "-P", "la", "x"];
    cp_link_follow_dangling: "cp" ["--link", "-H", "dl", "x"];
    cp_link_recursive_dir_symlink: "cp" ["--link", "-RL", "ld", "x"] => unordered;
    cp_preserve_links_command_line_symlink: "cp" ["--preserve=links", "-R", "-H", "a", "la", "e"];
    cp_preserve_links_dereference: "cp" ["--preserve=links", "-RL", "m", "x"]
        setup("mkdir m && echo M > m/a && ln -s a m/b");
    cp_preserve_links_dereference_hard: "cp" ["--preserve=links", "-RL", "m", "x"]
        setup("mkdir m && echo M > m/a && ln m/a m/b");
    cp_no_preserve_links: "cp" ["-dR", "--no-preserve=links", "m", "x"]
        setup("mkdir m && echo M > m/a && ln m/a m/b");
    cp_archive_update_keeps_hard_links: "cp" ["-au", "s", "t"]
        setup("mkdir -p s t/s && echo S > s/f && ln s/f s/l && ln s/f s/n && touch t/s/f && touch -d '+1 hour' t/s/n");

    // --parents.
    cp_parents_preserve_existing_dirs: "cp" ["-p", "--parents", "d/sub/g", "e"]
        setup("mkdir -p e/d/sub && chmod 700 e/d e/d/sub") => mtime;
    cp_parents_new_dirs_mode: "cp" ["-p", "--parents", "d/sub/g", "e"] setup("chmod 751 d")
        => mtime;
    cp_parents_no_preserve_mode: "cp" ["--parents", "--no-preserve=mode", "d/sub/g", "e"]
        setup("chmod 700 d");
    cp_parents_through_file: "cp" ["--parents", "a/g", "e"];
    cp_parents_recursive_slash: "cp" ["-Rv", "--parents", "d/sub/", "e"];
    cp_parents_onto_file: "cp" ["--parents", "d/sub/g", "e"] setup("touch e/d");

    // --reflink and --sparse.
    cp_reflink_always: "cp" ["--reflink=always", "a", "x"];
    cp_reflink_never: "cp" ["--reflink=never", "-v", "a", "x"];
    cp_reflink_auto_sparse_always: "cp" ["--reflink=auto", "--sparse=always", "a", "x"];
    cp_reflink_sparse_conflict: "cp" ["--reflink", "--sparse=always", "a", "x"];
    cp_reflink_sparse_never_conflict: "cp" ["--reflink", "--sparse=never", "a", "x"];
    cp_reflink_bad: "cp" ["--reflink=bogus", "a", "x"];
    cp_sparse_always: "cp" ["--sparse=always", "sp", "x"] setup("truncate -s 64K sp && printf X >> sp");
    cp_sparse_never: "cp" ["--sparse=never", "sp", "x"] setup("truncate -s 64K sp && printf X >> sp");
    cp_sparse_bad: "cp" ["--sparse=bogus", "a", "x"];

    // Writing through symlinks.
    cp_dangling_destination_posixly_correct: "cp" ["a", "dl"] => posixly_correct;
    cp_force_dangling_destination: "cp" ["-f", "a", "dl"];
    cp_force_symlink_loop: "cp" ["-f", "a", "loop"] setup("ln -s loop loop");
    cp_symlink_loop: "cp" ["a", "loop"] setup("ln -s loop loop");

    // Names.
    cp_recursive_dotdot: "cp" ["-R", "d/sub/..", "e"] => unordered;
    cp_target_double_slash: "cp" ["-v", "a", "e//"];
    cp_trailing_slash_missing: "cp" ["a", "nodir/"];
    cp_interactive_force_read_only: "cp" ["-fi", "a", "ro"] stdin b"y\n";

    // ln -r resolves only the destination's directory.
    ln_relative_replace_symlink: "ln" ["-srfv", "here", "lx"] setup("ln -s dir1/dir2/f lx");
    ln_relative_symlink_chain: "ln" ["-srv", "lla", "e/x"];
    ln_relative_empty: "ln" ["-sr", "", "x"];
    ln_symbolic_force_onto_itself_relative: "ln" ["-sf", "../a", "e/a"] setup("touch e/a");

    // mv: backups of directories, the last --update.
    mv_backup_trailing_slash: "mv" ["-T", "--backup=numbered", "-v", "e", "p/"];
    mv_backup_into_directory: "mv" ["--backup=simple", "f", "d/"] setup("echo NF > f");
    mv_update_last_wins: "mv" ["--update=none", "--update=all", "new", "old"];
    mv_update_last_wins_none: "mv" ["--update=all", "--update=none", "new", "old"];
    cp_update_last_wins: "cp" ["--update=none", "--update=all", "new", "old"];
    cp_update_last_wins_none: "cp" ["--update=all", "--update=none", "new", "old"];
    mv_trailing_slash_new_name: "mv" ["-T", "e", "e3/"];
    cp_recursive_trailing_slash_new_name: "cp" ["-r", "e", "e3/"];

    // Names that are not UTF-8 (`@xff@` is the byte 0xff; `\377` in setup).
    cp_non_utf8_names: "cp" ["-v", "n@xff@", "c@xff@"] setup("printf X > \"$(printf 'n\\377')\"");
    cp_non_utf8_into_directory: "cp" ["-v", "a", "n@xff@", "d"] setup("printf X > \"$(printf 'n\\377')\"");
    cp_non_utf8_target_directory: "cp" ["-v", "-t", "d@xff@", "a", "b"] setup("mkdir \"$(printf 'd\\377')\"");
    cp_non_utf8_tree: "cp" ["-rv", "t@xff@", "u@xff@"] setup("mkdir \"$(printf 't\\377')\" && printf Y > \"$(printf 't\\377/f\\377')\"");
    cp_non_utf8_parents: "cp" ["-v", "--parents", "t@xff@/f@xff@", "e"] setup("mkdir \"$(printf 't\\377')\" && printf Y > \"$(printf 't\\377/f\\377')\"");
    cp_non_utf8_missing_source: "cp" ["n@xff@", "z"];
    cp_non_utf8_backup: "cp" ["-v", "--backup=numbered", "a", "n@xff@"] setup("printf X > \"$(printf 'n\\377')\"");
    mv_non_utf8_names: "mv" ["-v", "n@xff@", "m@xff@"] setup("printf X > \"$(printf 'n\\377')\"");
    mv_non_utf8_into_directory: "mv" ["-v", "a", "n@xff@", "d@xff@"] setup("printf X > \"$(printf 'n\\377')\" && mkdir \"$(printf 'd\\377')\"");
    mv_non_utf8_strip_slashes: "mv" ["-v", "--strip-trailing-slashes", "t@xff@/", "e"] setup("mkdir \"$(printf 't\\377')\"");
    ln_non_utf8_symbolic: "ln" ["-sv", "n@xff@", "l@xff@"];
    ln_non_utf8_hard: "ln" ["-v", "n@xff@", "h@xff@"] setup("printf X > \"$(printf 'n\\377')\"");
    ln_non_utf8_target_directory: "ln" ["-v", "-t", "d@xff@", "a"] setup("mkdir \"$(printf 'd\\377')\"");
    ln_non_utf8_target_directory_missing: "ln" ["-t", "d@xff@", "a"];
    mkdir_non_utf8_parents: "mkdir" ["-pv", "x@xff@/y@xff@"];
    mkdir_non_utf8_exists: "mkdir" ["d@xff@"] setup("mkdir \"$(printf 'd\\377')\"");
    mkdir_non_utf8_mode: "mkdir" ["-v", "-m", "700", "m@xff@"];
    rm_non_utf8: "rm" ["-v", "n@xff@", "d@xff@"] setup("printf X > \"$(printf 'n\\377')\"");
    rm_non_utf8_recursive: "rm" ["-rv", "t@xff@"] setup("mkdir \"$(printf 't\\377')\" && printf Y > \"$(printf 't\\377/f\\377')\"");
    stat_non_utf8: "stat" ["-c", "%n|%N|%s", "n@xff@", "l@xff@", "nope@xff@"] setup("printf X > \"$(printf 'n\\377')\" && ln -s \"$(printf 'n\\377')\" \"$(printf 'l\\377')\"");
    stat_non_utf8_deref_terse: "stat" ["-L", "-c", "%n %s", "l@xff@"] setup("printf X > \"$(printf 'n\\377')\" && ln -s \"$(printf 'n\\377')\" \"$(printf 'l\\377')\"");
    stat_fs_non_utf8: "stat" ["-f", "-c", "%n", "n@xff@", "nope@xff@"] setup("printf X > \"$(printf 'n\\377')\"");
    stat_fs_stdin: "stat" ["-f", "-"];
    df_non_utf8: "df" ["--output=file", "n@xff@", "nope@xff@"] setup("printf X > \"$(printf 'n\\377')\"");
    df_total_target: "df" ["--output=target", "--total", "a"];
    df_total_source_target: "df" ["--output=source,target", "--total", "a"] => nostderr;
    realpath_non_utf8: "realpath" ["--relative-to=.", "n@xff@", "l@xff@", "nope@xff@/x"] setup("printf X > \"$(printf 'n\\377')\" && ln -s \"$(printf 'n\\377')\" \"$(printf 'l\\377')\"");
    realpath_non_utf8_relative: "realpath" ["--relative-to=d@xff@", "n@xff@"] setup("printf X > \"$(printf 'n\\377')\" && mkdir \"$(printf 'd\\377')\"");
    basename_non_utf8: "basename" ["-a", "-s", "@xff@", "x/n@xff@", "d@xff@/"];
    dirname_non_utf8: "dirname" ["d@xff@/n@xff@", "n@xff@"];
    chmod_non_utf8: "chmod" ["-v", "600", "n@xff@", "nope@xff@"] setup("printf X > \"$(printf 'n\\377')\"");
    chmod_non_utf8_recursive: "chmod" ["-Rc", "700", "t@xff@"] setup("mkdir \"$(printf 't\\377')\" && printf Y > \"$(printf 't\\377/f\\377')\"");
    chmod_non_utf8_mode: "chmod" ["u@xff@", "a"];
    chown_non_utf8: "chown" ["-v", ":0", "n@xff@", "nope@xff@"] setup("printf X > \"$(printf 'n\\377')\"");
    chown_non_utf8_user: "chown" ["u@xff@", "a"];
    chown_non_utf8_spec_only: "chown" ["u@xff@"];
    touch_non_utf8: "touch" ["-d", "2001-02-03 04:05:06", "n@xff@", "new@xff@"] setup("printf X > \"$(printf 'n\\377')\"") => mtime, atime;
    touch_non_utf8_date: "touch" ["-d", "x@xff@", "a"];
    touch_non_utf8_reference: "touch" ["-r", "nope@xff@", "a"];
    mkdir_non_utf8_bad_mode: "mkdir" ["-m", "u@xff@", "m"];
    md5sum_non_utf8: "md5sum" ["n@xff@", "b\\s", "nope@xff@"] setup("printf X > \"$(printf 'n\\377')\" && printf Y > 'b\\s'");
    sha256sum_non_utf8_tag: "sha256sum" ["--tag", "n@xff@"] setup("printf X > \"$(printf 'n\\377')\"");
    b2sum_non_utf8_zero: "b2sum" ["-z", "-l", "128", "n@xff@"] setup("printf X > \"$(printf 'n\\377')\"");
    md5sum_check_non_utf8: "md5sum" ["-c", "s@xff@"] setup("printf X > \"$(printf 'n\\377')\" && printf Y > \"$(printf 'm\\377')\" && md5sum \"$(printf 'n\\377')\" \"$(printf 'm\\377')\" > \"$(printf 's\\377')\" && echo Z > \"$(printf 'm\\377')\" && printf '00000000000000000000000000000000  x\\377\\n' >> \"$(printf 's\\377')\"");
    md5sum_check_non_utf8_missing_list: "md5sum" ["-c", "nope@xff@"];
    id_non_utf8_user: "id" ["-u", "root", "u@xff@"];
    printenv_non_utf8: "printenv" ["nope@xff@"];
    env_non_utf8_verbose: "env" ["-v", "-i", "-u", "B@xff@", "A=x@xff@", "printenv", "A"];
    env_non_utf8_bad_unset: "env" ["-u", "B@xff@=", "true"];
}

/// Whether `dir`'s file system stores names that are not UTF-8. APFS (macOS)
/// refuses them with EILSEQ, for every program alike, so there is nothing to
/// test there.
fn accepts_non_utf8_names(dir: &Path) -> bool {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let probe = dir.join(OsStr::from_bytes(b".probe\xff"));
    match fs::write(&probe, b"") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(e) if e.raw_os_error() == Some(libc::EILSEQ) => {
            eprintln!("note: skipping — this file system rejects names that are not UTF-8");
            false
        }
        Err(e) => panic!("cannot create {}: {e}", probe.display()),
    }
}

/// `mktemp` names are random: check that a template that is not UTF-8 is
/// used byte for byte, as GNU does.
#[test]
fn mktemp_non_utf8_template() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let dir = scratch_root().join("mktemp-non-utf8");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    if !accepts_non_utf8_names(&dir) {
        return;
    }
    for args in [
        &[&b"-p"[..], b".", b"f\xffXXXXXX"][..],
        &[b"-d", b"-p", b".", b"d\xffXXXXXX"],
        &[b"--suffix=\xff", b"s.XXXXXX"],
    ] {
        let mut cmd = Command::new(dogfish_bin());
        cmd.arg("mktemp")
            .args(args.iter().map(|a| OsStr::from_bytes(a)));
        let out = exec(cmd, &dir, None, "C");
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        let name = out.stdout.strip_suffix(b"\n").unwrap();
        assert!(name.contains(&0xff), "{out:?}");
        assert!(dir.join(OsStr::from_bytes(name)).exists(), "{out:?}");
    }
}

/// The undo journal keeps names that are not UTF-8 losslessly: undoing an
/// overwrite and a move restores exactly the files that were there.
#[test]
fn undo_restores_non_utf8_names() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let dir = scratch_root().join("undo-non-utf8");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    if !accepts_non_utf8_names(&dir) {
        return;
    }
    let home = dir.join(".home");
    let odd = OsStr::from_bytes(b"n\xff");
    let moved = OsStr::from_bytes(b"m\xff");
    fs::write(dir.join(odd), "old\n").unwrap();
    fs::write(dir.join("a"), "new\n").unwrap();
    let run = |args: &[&OsStr]| {
        let mut cmd = Command::new(dogfish_bin());
        cmd.args(args).env("DOGFISH_HOME", &home);
        let out = exec(cmd, &dir, None, "C");
        assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
    };
    run(&["cp".as_ref(), "a".as_ref(), odd]);
    assert_eq!(fs::read(dir.join(odd)).unwrap(), b"new\n");
    run(&["mv".as_ref(), odd, moved]);
    run(&["undo".as_ref()]);
    assert!(dir.join(odd).exists() && !dir.join(moved).exists());
    run(&["undo".as_ref()]);
    assert_eq!(fs::read(dir.join(odd)).unwrap(), b"old\n");
}

/// Runs Dogfish alone in a fresh scratch tree: for GNU 9.4 behaviour the
/// system's tool does not have.
fn dogfish_only(name: &str, args: &[&str], stdin: Option<&[u8]>) -> (Output, PathBuf) {
    let dir = scratch_root().join(format!("{name}-only"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    build_tree(&dir);
    let mut cmd = Command::new(dogfish_bin());
    cmd.arg(args[0])
        .args(&args[1..])
        .env("DOGFISH_HOME", dir.join(".dogfish-home"));
    (exec(cmd, &dir, stdin, "C"), dir)
}

// Ubuntu's coreutils 9.4 carries 9.5's `cp -n` (a warning, then skipping with
// status 0); upstream 9.4, whose tests/cp/cp-i.sh Dogfish passes, refuses
// with a diagnostic and status 1, and rejects `-b -n`. These pin upstream.

#[test]
fn cp_no_clobber_upstream() {
    let (out, dir) = dogfish_only("cp_no_clobber_upstream", &["cp", "-nv", "a", "b"], None);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, b"");
    assert_eq!(out.stderr, b"cp: not replacing 'b'\n");
    assert_eq!(fs::read(dir.join("b")).unwrap(), b"B\n");
}

#[test]
fn cp_interactive_then_no_clobber_upstream() {
    let (out, dir) = dogfish_only(
        "cp_interactive_then_no_clobber_upstream",
        &["cp", "-vin", "a", "b"],
        Some(b"y\n"),
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stderr, b"cp: not replacing 'b'\n");
    assert_eq!(fs::read(dir.join("b")).unwrap(), b"B\n");
}

#[test]
fn cp_no_clobber_then_interactive_upstream() {
    let (out, dir) = dogfish_only(
        "cp_no_clobber_then_interactive_upstream",
        &["cp", "-vni", "a", "b"],
        Some(b"y\n"),
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"'a' -> 'b'\n");
    assert_eq!(out.stderr, b"cp: overwrite 'b'? ");
    assert_eq!(fs::read(dir.join("b")).unwrap(), b"A\n");
}

#[test]
fn cp_backup_and_no_clobber_upstream() {
    let (out, dir) = dogfish_only(
        "cp_backup_and_no_clobber_upstream",
        &["cp", "-bn", "a", "b"],
        None,
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        out.stderr,
        b"cp: options --backup and --no-clobber are mutually exclusive\n\
          Try 'cp --help' for more information.\n"
    );
    assert_eq!(fs::read(dir.join("b")).unwrap(), b"B\n");
}

/// `chmod -R` and `chown -R` (to the current user and group) on a tree deeper
/// than `PATH_MAX`: 300 levels of 200-byte names, with a file and a symlink to
/// `..` every 50 levels. Each command runs on its own copy of the tree, built
/// the same way, against GNU's; stdout, stderr, the exit status and every
/// entry's mode and owner (listed by `find`, which walks by descriptor too)
/// must match. The trees are too deep for the tree cases' snapshot.
#[test]
fn recursive_mode_and_owner_deeper_than_path_max() {
    let (Some(gnu_chmod), Some(gnu_chown)) = (gnu_tool("chmod"), gnu_tool("chown")) else {
        eprintln!("note: skipping — system chmod/chown missing or not GNU coreutils");
        return;
    };
    let base = scratch_root().join("deeper-than-path-max");
    force_remove(&base);
    fs::create_dir_all(&base).unwrap();
    let me = fs::metadata(&base).unwrap();
    let (user, group) = (me.uid().to_string(), me.gid().to_string());
    let both = format!("{user}:{group}");
    let colon_group = format!(":{group}");
    // (Perl: a shell's `cd` may build the full path, which is too long.)
    let build = "my $n = q(n) x 200; mkdir q(deep) and chdir q(deep) or die $!; \
                 for my $i (1..300) { mkdir $n and chdir $n or die $!; \
                 if ($i % 50 == 0) { open(my $f, q(>), qq(f$i)) or die $!; \
                 symlink(q(..), qq(l$i)) or die $! } } \
                 open(my $f, q(>), q(f)) or die $!";
    let cases: &[(&str, &Path, &[&str])] = &[
        ("chmod", &gnu_chmod, &["-cR", "g+w,o-rx", "deep"]),
        ("chmod", &gnu_chmod, &["-vR", "u+w", "deep", "nope"]),
        ("chmod", &gnu_chmod, &["-R", "go=", "deep"]),
        ("chown", &gnu_chown, &["-cR", &both, "deep"]),
        ("chown", &gnu_chown, &["-vhR", &colon_group, "deep"]),
        ("chown", &gnu_chown, &["-HR", &user, "deep"]),
        // -L walks by full path, as GNU's does: both fail past PATH_MAX.
        ("chown", &gnu_chown, &["-LvR", &user, "deep"]),
    ];
    let listing = |dir: &Path| {
        let mut find = Command::new("find");
        find.args(["deep", "-printf", "%m %U:%G %y %d %f\\n"]);
        let out = exec(find, dir, None, "C");
        assert!(out.status.success(), "find failed in {}", dir.display());
        out.stdout
    };
    for (i, (tool, gnu, args)) in cases.iter().enumerate() {
        let ours_dir = base.join(format!("{i}-dogfish"));
        let gnu_dir = base.join(format!("{i}-gnu"));
        for dir in [&ours_dir, &gnu_dir] {
            fs::create_dir_all(dir).unwrap();
            let status = Command::new("perl")
                .args(["-e", build])
                .current_dir(dir)
                .status()
                .unwrap();
            assert!(status.success(), "building the deep tree failed");
        }
        let mut ours = Command::new(dogfish_bin());
        ours.arg(tool)
            .args(*args)
            .env("DOGFISH_HOME", base.join("dogfish-home"));
        let mut theirs = Command::new(gnu);
        theirs.arg0(tool).args(*args);
        let a = exec(ours, &ours_dir, None, "C");
        let b = exec(theirs, &gnu_dir, None, "C");
        // The outputs run to megabytes: report only where they part.
        let first_difference = |x: &[u8], y: &[u8]| {
            let at = x.iter().zip(y).take_while(|(p, q)| p == q).count();
            let line = |s: &[u8]| {
                let start = s[..at.min(s.len())]
                    .iter()
                    .rposition(|&c| c == b'\n')
                    .map_or(0, |p| p + 1);
                let rest = &s[start..];
                let line = &rest[..rest.iter().position(|&c| c == b'\n').unwrap_or(rest.len())];
                String::from_utf8_lossy(&line[line.len().saturating_sub(160)..]).into_owned()
            };
            format!(
                "{tool} {args:?}: {} vs {} bytes, first difference at byte {at}:\n  \
                 dogfish: ...{}\n  GNU:     ...{}",
                x.len(),
                y.len(),
                line(x),
                line(y)
            )
        };
        assert!(
            a.stdout == b.stdout,
            "stdout differs: {}",
            first_difference(&a.stdout, &b.stdout)
        );
        assert!(
            a.stderr == b.stderr,
            "stderr differs: {}",
            first_difference(&a.stderr, &b.stderr)
        );
        assert_eq!(
            a.status.code(),
            b.status.code(),
            "exit status differs: {tool} {args:?}"
        );
        let (la, lb) = (listing(&ours_dir), listing(&gnu_dir));
        assert!(
            la == lb,
            "resulting trees differ: {}",
            first_difference(&la, &lb)
        );
        assert!(
            la.split(|&c| c == b'\n').filter(|l| !l.is_empty()).count() > 300,
            "the tree is not as deep as meant"
        );
        // GNU's rm, which walks by descriptor at any depth.
        if let Some(rm) = gnu_tool("rm") {
            let _ = Command::new(rm)
                .arg("-rf")
                .arg(&ours_dir)
                .arg(&gnu_dir)
                .status();
        }
    }
    force_remove(&base);
}
