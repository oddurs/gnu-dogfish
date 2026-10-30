//! Model-based tests of the undo contract: random file trees, random
//! sequences of `cp`, `mv` and `rm --safe` run through the real binary, then
//! as many `dogfish undo` calls as the operations recorded.
//!
//! # Invariants
//!
//! 1. **Undo restores.** After any sequence of `cp` / `mv` / `rm --safe`
//!    (with `-f`, `-b`, `-r`, `-a`, `-n`, `-T`, `--remove-destination`, `-l`,
//!    `-s`, existing or missing destinations, same-file cases and moves across
//!    file systems), undoing every operation the journal recorded restores the
//!    tree byte for byte: the same paths, file types (including FIFOs and
//!    dangling symlinks), permission bits, file contents, symlink targets, and
//!    the same hard-link structure.
//! 2. **No operation loses data.** After every single operation, every file
//!    content that existed before it still exists, either in the tree or in
//!    the trash (`$DOGFISH_HOME/trash`).
//! 3. **Undo never clobbers and never fails on its own journal.** Every undo
//!    of a recorded operation succeeds; the journal stays readable.
//! 4. **Crashes lose nothing.** Killed at any step (`DOGFISH_CRASH_AT=<n>`,
//!    debug builds only), an operation leaves every original either at its
//!    path or in the trash, referenced by a journal entry: the journal still
//!    parses, and undoing everything it holds brings back every file content
//!    of the original tree, with no undo failing.
//!
//! The case count is `DOGFISH_PROPTEST_CASES` (default: a few dozen, so the
//! suite stays fast); set it to thousands for a long run.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};

// ------------------------------------------------------------------ trees

const NAMES: [&str; 4] = ["a", "b", "c", "d"];

/// One node of a generated tree. Children only matter for directories.
#[derive(Clone, Debug)]
enum Node {
    File {
        data: Vec<u8>,
        mode: u32,
    },
    Dir {
        mode: u32,
        children: Vec<(usize, Node)>,
    },
    Symlink(&'static str),
    Fifo,
    /// A hard link to the n-th regular file created before it (mod count).
    HardLink(usize),
}

const LINK_TARGETS: [&str; 7] = ["a", "b", "../a", "c/a", "missing", "d", "/nonexistent"];

fn file_mode() -> impl Strategy<Value = u32> {
    prop::sample::select(vec![0o644, 0o600, 0o755, 0o444, 0o640])
}

fn leaf() -> impl Strategy<Value = Node> {
    prop_oneof![
        4 => (prop::collection::vec(any::<u8>(), 0..24), file_mode())
            .prop_map(|(data, mode)| Node::File { data, mode }),
        1 => prop::sample::select(LINK_TARGETS.to_vec()).prop_map(Node::Symlink),
        1 => Just(Node::Fifo),
        1 => (0usize..4).prop_map(Node::HardLink),
    ]
}

fn children(inner: impl Strategy<Value = Node>) -> impl Strategy<Value = Vec<(usize, Node)>> {
    prop::collection::vec((0usize..NAMES.len(), inner), 0..4)
}

fn node() -> impl Strategy<Value = Node> {
    let dir_mode = prop::sample::select(vec![0o755, 0o700, 0o750]);
    prop_oneof![
        3 => leaf(),
        1 => (dir_mode, children(leaf())).prop_map(|(mode, children)| Node::Dir { mode, children }),
    ]
}

fn tree() -> impl Strategy<Value = Vec<(usize, Node)>> {
    prop::collection::vec((0usize..NAMES.len(), node()), 2..7)
}

fn mkfifo(p: &Path) {
    let c = CString::new(p.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c` is a valid NUL-terminated path for the duration of the call.
    let r = unsafe { libc::mkfifo(c.as_ptr(), 0o644) };
    assert_eq!(r, 0, "mkfifo {p:?}");
}

/// Creates `nodes` under `dir`; names already taken are skipped.
fn build(dir: &Path, nodes: &[(usize, Node)], files: &mut Vec<PathBuf>) {
    let mut dirs = Vec::new();
    for (name, node) in nodes {
        let p = dir.join(NAMES[*name]);
        if p.symlink_metadata().is_ok() {
            continue;
        }
        match node {
            Node::File { data, mode } => {
                fs::write(&p, data).unwrap();
                fs::set_permissions(&p, fs::Permissions::from_mode(*mode)).unwrap();
                files.push(p);
            }
            Node::Dir { mode, children } => {
                fs::create_dir(&p).unwrap();
                build(&p, children, files);
                dirs.push((p, *mode));
            }
            Node::Symlink(t) => symlink(t, &p).unwrap(),
            Node::Fifo => mkfifo(&p),
            Node::HardLink(n) => {
                if !files.is_empty() {
                    fs::hard_link(&files[n % files.len()], &p).unwrap();
                }
            }
        }
    }
    // Modes last, so a restrictive mode doesn't stop the children being made.
    for (p, mode) in dirs {
        fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
    }
}

// ------------------------------------------------------------- snapshots

#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    File(Vec<u8>),
    Dir,
    Symlink(PathBuf),
    Fifo,
    Other,
}

/// Everything the undo contract promises to restore.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    entries: BTreeMap<PathBuf, (Kind, u32)>,
    /// Groups of paths sharing one inode (only groups of two or more).
    links: BTreeSet<BTreeSet<PathBuf>>,
}

fn walk(root: &Path, rel: &Path, out: &mut Vec<(PathBuf, fs::Metadata)>) {
    let Ok(rd) = fs::read_dir(root.join(rel)) else {
        return;
    };
    let mut names: Vec<_> = rd.flatten().map(|e| e.file_name()).collect();
    names.sort();
    for name in names {
        let r = rel.join(&name);
        let Ok(m) = fs::symlink_metadata(root.join(&r)) else {
            continue;
        };
        let is_dir = m.is_dir();
        out.push((r.clone(), m));
        if is_dir {
            walk(root, &r, out);
        }
    }
}

/// Snapshot of the trees under `roots`, keyed by `<root index>/<relative path>`.
fn snapshot(roots: &[&Path]) -> Snapshot {
    let mut entries = BTreeMap::new();
    let mut inodes: HashMap<(u64, u64), BTreeSet<PathBuf>> = HashMap::new();
    for (i, root) in roots.iter().enumerate() {
        let mut found = Vec::new();
        walk(root, Path::new(""), &mut found);
        for (rel, m) in found {
            let key = Path::new(&i.to_string()).join(&rel);
            let abs = root.join(&rel);
            let ft = m.file_type();
            let kind = if ft.is_symlink() {
                Kind::Symlink(fs::read_link(&abs).unwrap())
            } else if ft.is_dir() {
                Kind::Dir
            } else if ft.is_file() {
                Kind::File(fs::read(&abs).unwrap_or_else(|e| panic!("read {abs:?}: {e}")))
            } else if ft.is_fifo() {
                Kind::Fifo
            } else {
                Kind::Other
            };
            if !ft.is_dir() && m.nlink() > 1 {
                inodes
                    .entry((m.dev(), m.ino()))
                    .or_default()
                    .insert(key.clone());
            }
            entries.insert(key, (kind, m.mode() & 0o7777));
        }
    }
    let links = inodes.into_values().filter(|g| g.len() > 1).collect();
    Snapshot { entries, links }
}

/// Every regular file content anywhere under `dir` (not following symlinks).
fn contents_under(dir: &Path, out: &mut BTreeSet<Vec<u8>>) {
    let mut found = Vec::new();
    walk(dir, Path::new(""), &mut found);
    for (rel, m) in found {
        if m.is_file()
            && let Ok(data) = fs::read(dir.join(rel))
        {
            out.insert(data);
        }
    }
}

fn file_contents(s: &Snapshot) -> BTreeSet<Vec<u8>> {
    s.entries
        .values()
        .filter_map(|(k, _)| match k {
            Kind::File(d) => Some(d.clone()),
            _ => None,
        })
        .collect()
}

/// Human-readable difference between two snapshots, for failure messages.
fn diff(want: &Snapshot, got: &Snapshot) -> String {
    let mut out = String::new();
    for (p, w) in &want.entries {
        match got.entries.get(p) {
            None => out += &format!("  missing  {p:?}: {w:?}\n"),
            Some(g) if g != w => out += &format!("  changed  {p:?}: {w:?} -> {g:?}\n"),
            _ => {}
        }
    }
    for (p, g) in &got.entries {
        if !want.entries.contains_key(p) {
            out += &format!("  extra    {p:?}: {g:?}\n");
        }
    }
    if want.links != got.links {
        out += &format!("  links    {:?} -> {:?}\n", want.links, got.links);
    }
    out
}

// ------------------------------------------------------------ operations

/// A path the operations draw from: with `pick`, one of the paths that exist
/// when the operation runs; else `NAMES[a]` or `NAMES[a]/NAMES[b]` in the work
/// tree, or a name on the other file system (most of which do not exist).
#[derive(Clone, Copy, Debug)]
struct P {
    pick: Option<usize>,
    first: usize,
    second: Option<usize>,
    other_fs: bool,
    slash: bool,
}

fn path_strategy(existing: f64) -> impl Strategy<Value = P> {
    (
        prop::option::weighted(existing, 0usize..64),
        0usize..NAMES.len(),
        prop::option::weighted(0.25, 0usize..NAMES.len()),
        prop::bool::weighted(0.15),
        prop::bool::weighted(0.08),
    )
        .prop_map(|(pick, first, second, other_fs, slash)| P {
            pick,
            first,
            second,
            other_fs,
            slash,
        })
}

#[derive(Clone, Debug)]
enum Tool {
    Cp,
    Mv,
    Rm,
}

#[derive(Clone, Debug)]
struct Op {
    tool: Tool,
    flags: Vec<&'static str>,
    paths: Vec<P>,
}

const CP_FLAGS: &[&str] = &[
    "-f",
    "-b",
    "--backup=numbered",
    "--backup=simple",
    "-r",
    "-a",
    "--remove-destination",
    "-l",
    "-s",
    "-T",
    "-n",
    "-L",
    "-P",
    "-p",
];
const MV_FLAGS: &[&str] = &["-f", "-b", "--backup=numbered", "-T", "-n"];
const RM_FLAGS: &[&str] = &["-r", "-f"];

fn flags(set: &'static [&'static str]) -> impl Strategy<Value = Vec<&'static str>> {
    prop::sample::subsequence(set.to_vec(), 0..=set.len().min(3))
}

/// One or two operands (mostly existing), plus a destination (existing half
/// the time) when `dest`.
fn operands(dest: bool) -> impl Strategy<Value = Vec<P>> {
    let with_dest = |srcs: usize| {
        (
            prop::collection::vec(path_strategy(0.85), srcs),
            prop::collection::vec(path_strategy(0.5), usize::from(dest)),
        )
            .prop_map(|(mut s, d)| {
                s.extend(d);
                s
            })
    };
    prop_oneof![4 => with_dest(1), 1 => with_dest(2)]
}

/// `-r` half the time: most trees hold directories.
fn recursive(mut flags: Vec<&'static str>, r: bool) -> Vec<&'static str> {
    if r && !flags.contains(&"-r") {
        flags.push("-r");
    }
    flags
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (flags(CP_FLAGS), any::<bool>(), operands(true))
            .prop_map(|(f, r, paths)| Op { tool: Tool::Cp, flags: recursive(f, r), paths }),
        3 => (flags(MV_FLAGS), operands(true))
            .prop_map(|(flags, paths)| Op { tool: Tool::Mv, flags, paths }),
        2 => (flags(RM_FLAGS), any::<bool>(), operands(false))
            .prop_map(|(f, r, paths)| Op { tool: Tool::Rm, flags: recursive(f, r), paths }),
    ]
}

// --------------------------------------------------------------- sandbox

struct Sandbox {
    _tmp: tempfile::TempDir,
    _other: Option<tempfile::TempDir>,
    work: PathBuf,
    home: PathBuf,
    /// A directory on a different file system, when there is one.
    other: Option<PathBuf>,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("w");
        let home = tmp.path().join("home");
        fs::create_dir(&work).unwrap();
        let dev = fs::metadata(tmp.path()).unwrap().dev();
        let other = tempfile::tempdir_in("/dev/shm")
            .ok()
            .filter(|d| fs::metadata(d.path()).is_ok_and(|m| m.dev() != dev));
        Self {
            work,
            home,
            other: other.as_ref().map(|d| d.path().to_path_buf()),
            _other: other,
            _tmp: tmp,
        }
    }

    fn roots(&self) -> Vec<&Path> {
        let mut r = vec![self.work.as_path()];
        r.extend(self.other.as_deref());
        r
    }

    /// Every path in the trees, as operations name them.
    fn existing(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (i, root) in self.roots().into_iter().enumerate() {
            let mut found = Vec::new();
            walk(root, Path::new(""), &mut found);
            for (rel, _) in found {
                out.push(if i == 0 {
                    rel.display().to_string()
                } else {
                    root.join(rel).display().to_string()
                });
            }
        }
        out
    }

    fn render(&self, p: &P) -> String {
        if let Some(k) = p.pick {
            let all = self.existing();
            if !all.is_empty() {
                let mut s = all[k % all.len()].clone();
                if p.slash {
                    s.push('/');
                }
                return s;
            }
        }
        let mut s = match (&self.other, p.other_fs) {
            (Some(o), true) => format!("{}/{}", o.display(), NAMES[p.first]),
            _ => NAMES[p.first].to_string(),
        };
        if let Some(b) = p.second {
            s = format!("{s}/{}", NAMES[b]);
        }
        if p.slash {
            s.push('/');
        }
        s
    }

    fn command(&self, args: &[String], env: &[(&str, String)]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_dogfish"));
        cmd.current_dir(&self.work)
            .env("DOGFISH_HOME", &self.home)
            .env("DOGFISH_BACKUP_LIMIT", "1G")
            .env_remove("DOGFISH_UNDO")
            .env_remove("DOGFISH_CRASH_AT")
            .env_remove("VERSION_CONTROL")
            .env_remove("SIMPLE_BACKUP_SUFFIX")
            .env("LC_ALL", "C")
            .envs(env.iter().map(|(k, v)| (*k, v)))
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    /// Runs dogfish, failing the case if it hangs (a FIFO opened by mistake).
    fn run(&self, args: &[String], env: &[(&str, String)]) -> Result<Output, TestCaseError> {
        let mut child = self.command(args, env).spawn().unwrap();
        let start = Instant::now();
        loop {
            if child.try_wait().unwrap().is_some() {
                return Ok(child.wait_with_output().unwrap());
            }
            if start.elapsed() > Duration::from_secs(5) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(TestCaseError::fail(format!("dogfish {args:?} hung")));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn op_args(&self, op: &Op) -> Vec<String> {
        let mut args: Vec<String> = vec![
            match op.tool {
                Tool::Cp => "cp",
                Tool::Mv => "mv",
                Tool::Rm => "rm",
            }
            .into(),
        ];
        if matches!(op.tool, Tool::Rm) {
            args.push("--safe".into());
        }
        args.extend(op.flags.iter().map(|f| f.to_string()));
        args.extend(op.paths.iter().map(|p| self.render(p)));
        args
    }

    /// Whether `op` is a `mv` between the two file systems.
    fn moves_across(&self, op: &Op) -> bool {
        let Some(other) = &self.other else {
            return false;
        };
        let on_other = |p: &P| self.render(p).starts_with(&*other.to_string_lossy());
        matches!(op.tool, Tool::Mv)
            && op.paths.iter().any(on_other)
            && !op.paths.iter().all(on_other)
    }

    /// Whether `op` could open a FIFO for reading or writing and block (as
    /// GNU's would): such operations are skipped.
    fn may_block(&self, op: &Op) -> bool {
        if !matches!(op.tool, Tool::Cp) {
            return false;
        }
        let has = |f: &str| op.flags.contains(&f);
        if has("-l") || has("-s") {
            return false;
        }
        let replaces = has("--remove-destination");
        let recursive = has("-r") || has("-a");
        let (srcs, dst) = op.paths.split_at(op.paths.len() - 1);
        let dst = self.work.join(self.render(&dst[0]));
        let dst_dir = !has("-T") && fs::metadata(&dst).is_ok_and(|m| m.is_dir());
        let is_fifo = |p: &Path| fs::metadata(p).is_ok_and(|m| m.file_type().is_fifo());
        for s in srcs {
            let src = self.work.join(self.render(s));
            let target = if dst_dir {
                dst.join(src.file_name().unwrap_or_default())
            } else {
                dst.clone()
            };
            if !recursive && is_fifo(&src) {
                return true;
            }
            // Every file the copy might write, against what is there now.
            let mut found = vec![(PathBuf::new(), fs::symlink_metadata(&src).ok())];
            if fs::metadata(&src).is_ok_and(|m| m.is_dir()) {
                let mut inner = Vec::new();
                walk(&src, Path::new(""), &mut inner);
                found.extend(inner.into_iter().map(|(r, m)| (r, Some(m))));
            }
            for (rel, _) in found {
                let t = if rel.as_os_str().is_empty() {
                    target.clone()
                } else {
                    target.join(&rel)
                };
                if (is_fifo(&t) && !replaces) || (has("-L") && is_fifo(&src.join(&rel))) {
                    return true;
                }
            }
        }
        false
    }

    fn journal_len(&self) -> Result<usize, TestCaseError> {
        let out = self.run(&["history".into(), "--json".into()], &[])?;
        prop_assert!(
            out.status.success(),
            "history failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // One NDJSON record per journal entry.
        let mut n = 0;
        for line in out.stdout.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            serde_json::from_slice::<serde_json::Value>(line)
                .map_err(|e| TestCaseError::fail(format!("history --json: {e}")))?;
            n += 1;
        }
        Ok(n)
    }

    /// The home trash and the trash directories on other file systems.
    fn trash_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = vec![self.home.join("trash")];
        if let Ok(list) = fs::read(self.home.join("trash-dirs")) {
            dirs.extend(
                list.split(|&b| b == b'\n')
                    .filter(|l| !l.is_empty())
                    .map(|l| PathBuf::from(std::ffi::OsStr::from_bytes(l))),
            );
        }
        dirs
    }

    fn trash_contents(&self) -> BTreeSet<Vec<u8>> {
        let mut out = BTreeSet::new();
        for dir in self.trash_dirs() {
            contents_under(&dir, &mut out);
        }
        out
    }

    /// Undoes everything in the journal; each undo must succeed.
    fn undo_all(&self, log: &str) -> Result<(), TestCaseError> {
        let n = self.journal_len()?;
        for i in 0..n {
            let out = self.run(&["undo".into()], &[])?;
            prop_assert!(
                out.status.success(),
                "undo {} of {n} failed: {}{}\n{log}",
                i + 1,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        prop_assert_eq!(
            self.journal_len()?,
            0,
            "journal not empty after undo\n{}",
            log
        );
        Ok(())
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Trash directories on other file systems (named after this home).
        for dir in self.trash_dirs().into_iter().skip(1) {
            let mut found = Vec::new();
            walk(&dir, Path::new(""), &mut found);
            for (rel, m) in found {
                if m.is_dir() {
                    let _ = fs::set_permissions(dir.join(rel), fs::Permissions::from_mode(0o700));
                }
            }
            let _ = fs::remove_dir_all(&dir);
        }
        // Make everything removable for TempDir (restrictive modes, as non-root).
        for root in [Some(self._tmp.path()), self.other.as_deref()]
            .into_iter()
            .flatten()
        {
            let mut found = Vec::new();
            walk(root, Path::new(""), &mut found);
            for (rel, m) in found {
                if m.is_dir() {
                    let _ = fs::set_permissions(root.join(rel), fs::Permissions::from_mode(0o700));
                }
            }
        }
    }
}

fn cases(default: u32) -> u32 {
    std::env::var("DOGFISH_PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn setup(sb: &Sandbox, work: &[(usize, Node)], other: &[(usize, Node)]) {
    build(&sb.work, work, &mut Vec::new());
    if let Some(o) = &sb.other {
        build(o, other, &mut Vec::new());
    }
}

/// Runs `ops`, checking that no operation loses data, then undoes them all
/// and compares with the tree before.
fn check_sequence(
    work: &[(usize, Node)],
    other: &[(usize, Node)],
    ops: &[Op],
) -> Result<(), TestCaseError> {
    let sb = Sandbox::new();
    setup(&sb, work, other);
    let before = snapshot(&sb.roots());
    let mut log = String::new();
    let mut moved_across = false;
    for op in ops {
        if sb.may_block(op) {
            continue;
        }
        moved_across |= sb.moves_across(op);
        let pre = snapshot(&sb.roots());
        let args = sb.op_args(op);
        let out = sb.run(&args, &[])?;
        STATS[usize::from(out.status.success())].fetch_add(1, Ordering::Relaxed);
        if std::env::var_os("DOGFISH_MODEL_DEBUG").is_some() {
            eprint!("ERR {}", String::from_utf8_lossy(&out.stderr));
        }
        log += &format!(
            "$ dogfish {}  # exit {}\n{}",
            args.join(" "),
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr)
        );
        // Invariant 2: nothing that existed before is gone from both the tree
        // and the trash.
        let post = snapshot(&sb.roots());
        let mut have = file_contents(&post);
        have.extend(sb.trash_contents());
        for data in file_contents(&pre) {
            prop_assert!(
                have.contains(&data),
                "content {:?} lost by the last operation\n{}\n{}",
                String::from_utf8_lossy(&data),
                log,
                diff(&pre, &post)
            );
        }
    }
    STATS[2].fetch_add(sb.journal_len()? as u64, Ordering::Relaxed);
    sb.undo_all(&log)?;
    let mut after = snapshot(&sb.roots());
    if moved_across {
        // A move across file systems copies: like GNU's, it cannot keep hard
        // links to files outside what it moves, and neither can moving back.
        after.links = before.links.clone();
    }
    prop_assert!(
        before == after,
        "undo did not restore the tree:\n{}{}",
        diff(&before, &after),
        log
    );
    Ok(())
}

/// Operations that failed, that succeeded, and journal entries undone.
static STATS: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

fn runner(default_cases: u32) -> TestRunner {
    TestRunner::new(Config {
        cases: cases(default_cases),
        failure_persistence: None,
        max_shrink_iters: 2048,
        ..Config::default()
    })
}

#[test]
fn undo_restores_any_sequence_of_cp_mv_rm() {
    let strategy = (
        tree(),
        prop::collection::vec((0usize..NAMES.len(), leaf()), 0..3),
        prop::collection::vec(op(), 1..6),
    );
    let default = if cfg!(debug_assertions) { 192 } else { 384 };
    runner(default)
        .run(&strategy, |(work, other, ops)| {
            check_sequence(&work, &other, &ops)
        })
        .unwrap();
    eprintln!(
        "operations: {} failed, {} succeeded; {} journal entries undone",
        STATS[0].load(Ordering::Relaxed),
        STATS[1].load(Ordering::Relaxed),
        STATS[2].load(Ordering::Relaxed)
    );
}

// -------------------------------------------------------- crash injection

/// Runs `op` killed at its `n`-th step, then checks invariant 4. `Ok(false)`
/// once the operation runs to completion (there is no `n`-th step).
fn check_crash(
    work: &[(usize, Node)],
    other: &[(usize, Node)],
    prefix: &[Op],
    op: &Op,
    n: usize,
) -> Result<bool, TestCaseError> {
    let sb = Sandbox::new();
    setup(&sb, work, other);
    let before = snapshot(&sb.roots());
    let mut log = String::new();
    let mut moved_across = false;
    for p in prefix {
        if !sb.may_block(p) {
            moved_across |= sb.moves_across(p);
            let args = sb.op_args(p);
            sb.run(&args, &[])?;
            log += &format!("$ dogfish {}\n", args.join(" "));
        }
    }
    if sb.may_block(op) {
        return Ok(false);
    }
    moved_across |= sb.moves_across(op);
    let args = sb.op_args(op);
    let out = sb.run(&args, &[("DOGFISH_CRASH_AT", n.to_string())])?;
    use std::os::unix::process::ExitStatusExt;
    let crashed = out.status.signal() == Some(libc::SIGKILL);
    log += &format!(
        "$ DOGFISH_CRASH_AT={n} dogfish {}  # {}\n{}",
        args.join(" "),
        if crashed {
            "killed"
        } else {
            "ran to completion"
        },
        String::from_utf8_lossy(&out.stderr)
    );
    // The journal must still parse, and every undo must succeed.
    sb.undo_all(&log)?;
    // Nothing of the original tree may be gone from both the tree and the trash.
    let mut have = file_contents(&snapshot(&sb.roots()));
    have.extend(sb.trash_contents());
    for data in file_contents(&before) {
        prop_assert!(
            have.contains(&data),
            "content {:?} lost after a crash\n{}",
            String::from_utf8_lossy(&data),
            log
        );
    }
    if !crashed {
        // Ran to completion: the ordinary contract holds.
        let mut after = snapshot(&sb.roots());
        if moved_across {
            after.links = before.links.clone();
        }
        prop_assert!(
            before == after,
            "undo did not restore the tree:\n{}{}",
            diff(&before, &after),
            log
        );
    }
    Ok(crashed)
}

#[test]
fn crash_at_any_step_loses_nothing() {
    if !cfg!(debug_assertions) {
        // The hook is compiled into debug builds only; see
        // `crash_hook_is_inert_in_release_builds`.
        return;
    }
    let strategy = (
        tree(),
        prop::collection::vec((0usize..NAMES.len(), leaf()), 0..3),
        prop::collection::vec(op(), 0..3),
        op(),
    );
    runner(64)
        .run(&strategy, |(work, other, prefix, op)| {
            for n in 1..200 {
                if !check_crash(&work, &other, &prefix, &op, n)? {
                    break;
                }
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn crash_hook_is_inert_in_release_builds() {
    let sb = Sandbox::new();
    fs::write(sb.work.join("a"), "data").unwrap();
    let out = sb
        .run(
            &["rm".into(), "--safe".into(), "a".into()],
            &[("DOGFISH_CRASH_AT", "1".into())],
        )
        .unwrap();
    if cfg!(debug_assertions) {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(out.status.signal(), Some(libc::SIGKILL));
    } else {
        assert!(out.status.success(), "{out:?}");
        assert!(sb.work.join("a").symlink_metadata().is_err());
    }
}

/// Operations whose every step is in the journal before it happens: killed at
/// any step, undoing what the journal holds brings back the exact tree.
#[test]
fn crash_then_undo_restores_exactly() {
    if !cfg!(debug_assertions) {
        return;
    }
    let file = |data: &[u8]| Node::File {
        data: data.to_vec(),
        mode: 0o644,
    };
    let dir = |children: Vec<(usize, Node)>| Node::Dir {
        mode: 0o755,
        children,
    };
    let at = |first: usize, other_fs: bool| P {
        pick: None,
        first,
        second: None,
        other_fs,
        slash: false,
    };
    let op = |tool: Tool, flags: &[&'static str], paths: Vec<P>| Op {
        tool,
        flags: flags.to_vec(),
        paths,
    };
    // a: a file hard-linked as c; b: a directory; d: a file.
    let work = vec![
        (0, file(b"a")),
        (2, Node::HardLink(0)),
        (
            1,
            dir(vec![(0, file(b"inner")), (1, Node::Symlink("../a"))]),
        ),
        (3, file(b"d")),
    ];
    let other = vec![(0, file(b"other a")), (1, Node::HardLink(0))];
    let cases = [
        op(Tool::Rm, &["-r"], vec![at(1, false)]),
        op(Tool::Rm, &[], vec![at(0, true)]),
        op(Tool::Mv, &[], vec![at(3, false), at(0, false)]),
        op(Tool::Mv, &["-b"], vec![at(3, false), at(0, false)]),
        op(Tool::Mv, &[], vec![at(1, false), at(2, true)]),
        op(Tool::Mv, &[], vec![at(3, false), at(0, true)]),
        op(Tool::Cp, &[], vec![at(3, false), at(0, false)]),
        op(
            Tool::Cp,
            &["--remove-destination"],
            vec![at(3, false), at(0, false)],
        ),
        op(Tool::Cp, &["-b"], vec![at(3, false), at(0, false)]),
        op(Tool::Cp, &[], vec![at(3, false), at(0, true)]),
    ];
    for op in &cases {
        for n in 1..200 {
            let sb = Sandbox::new();
            setup(&sb, &work, &other);
            let before = snapshot(&sb.roots());
            let args = sb.op_args(op);
            let out = sb
                .run(&args, &[("DOGFISH_CRASH_AT", n.to_string())])
                .unwrap();
            use std::os::unix::process::ExitStatusExt;
            let crashed = out.status.signal() == Some(libc::SIGKILL);
            let log = format!("$ DOGFISH_CRASH_AT={n} dogfish {}", args.join(" "));
            sb.undo_all(&log).unwrap();
            let mut after = snapshot(&sb.roots());
            if sb.moves_across(op) {
                after.links = before.links.clone();
            }
            assert!(
                before == after,
                "{log}: undo did not restore the tree:\n{}",
                diff(&before, &after)
            );
            if !crashed {
                break;
            }
        }
    }
}
