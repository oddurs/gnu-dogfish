//! du — Parallel disk usage with sparklines on a TTY, GNU `du` otherwise

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use dogfish_core::{
    DogfishContext, emit_objects_or_table, format_size, gnu_io_error, sparkline_bar, stdout,
};
use serde::Serialize;

use crate::common::dirfd::{At, DirFd, EntryType, MAX_OPEN_DIRS, Names, fts_root_name};
use crate::common::files0::{Files0From, extra_operand_error};
use crate::common::fnmatch::fnmatch;
use crate::common::getopt::{
    Getopt, HasArg, OptSpec, argmatch, getopt_error, lossy, print_help, print_version, usage_error,
};
use crate::common::human::{
    HUMAN_AUTOSCALE, HUMAN_BASE_1024, HUMAN_SI, env_block_size, human_readable, parse_block_size,
    parse_size_suffixed,
};
use crate::common::quote::{Quoting, qa, qf, quote_bytes, quote_locale};
use crate::common::strftime::{Timespec, format_local_time};

const DU_OPTIONS: &[OptSpec<&str>] = &[
    OptSpec::both(b'a', "all", HasArg::No, "all"),
    OptSpec::long("apparent-size", HasArg::No, "apparent-size"),
    OptSpec::both(b'B', "block-size", HasArg::Required, "block-size"),
    OptSpec::both(b'b', "bytes", HasArg::No, "bytes"),
    OptSpec::both(b'l', "count-links", HasArg::No, "count-links"),
    OptSpec::both(b'L', "dereference", HasArg::No, "dereference"),
    OptSpec::both(b'D', "dereference-args", HasArg::No, "dereference-args"),
    OptSpec::long("exclude", HasArg::Required, "exclude"),
    OptSpec::both(b'X', "exclude-from", HasArg::Required, "exclude-from"),
    OptSpec::long("files0-from", HasArg::Required, "files0-from"),
    OptSpec::both(b'h', "human-readable", HasArg::No, "human-readable"),
    OptSpec::long("inodes", HasArg::No, "inodes"),
    OptSpec::long("si", HasArg::No, "si"),
    OptSpec::both(b'd', "max-depth", HasArg::Required, "max-depth"),
    OptSpec::both(b'0', "null", HasArg::No, "null"),
    OptSpec::both(b'P', "no-dereference", HasArg::No, "no-dereference"),
    OptSpec::both(b'x', "one-file-system", HasArg::No, "one-file-system"),
    OptSpec::both(b'S', "separate-dirs", HasArg::No, "separate-dirs"),
    OptSpec::both(b's', "summarize", HasArg::No, "summarize"),
    OptSpec::both(b'c', "total", HasArg::No, "total"),
    OptSpec::both(b't', "threshold", HasArg::Required, "threshold"),
    OptSpec::long("time", HasArg::Optional, "time"),
    OptSpec::long("time-style", HasArg::Required, "time-style"),
    OptSpec::long("help", HasArg::No, "help"),
    OptSpec::long("version", HasArg::No, "version"),
    // Dogfish extensions.
    OptSpec::both(b'J', "json", HasArg::No, "json").exact(),
    // Short-only options.
    OptSpec::short(b'H', HasArg::No, "dereference-args"),
    OptSpec::short(b'k', HasArg::No, "kilobytes"),
    OptSpec::short(b'm', HasArg::No, "megabytes"),
];

const DU_HELP: &str = "\
Usage: du [OPTION]... [FILE]...
  or:  du [OPTION]... --files0-from=F
Summarize device usage of the set of FILEs, recursively for directories.

  -0, --null            end each output line with NUL, not newline
  -a, --all             write counts for all files, not just directories
      --apparent-size   print apparent sizes rather than device usage
  -B, --block-size=SIZE  scale sizes by SIZE before printing them
  -b, --bytes           equivalent to '--apparent-size --block-size=1'
  -c, --total           produce a grand total
  -D, --dereference-args  dereference only symlinks that are listed on the
                          command line (also -H)
  -d, --max-depth=N     print the total for a directory only if it is N or
                          fewer levels below the command line argument
      --files0-from=F   summarize device usage of the
                          NUL-terminated file names specified in file F;
                          if F is -, then read names from standard input
  -h, --human-readable  print sizes in human readable format (e.g., 1K 234M 2G)
      --inodes          list inode usage information instead of block usage
  -k                    like --block-size=1K
  -L, --dereference     dereference all symbolic links
  -l, --count-links     count sizes many times if hard linked
  -m                    like --block-size=1M
  -P, --no-dereference  don't follow any symbolic links (this is the default)
  -S, --separate-dirs   for directories do not include size of subdirectories
      --si              like -h, but use powers of 1000 not 1024
  -s, --summarize       display only a total for each argument
  -t, --threshold=SIZE  exclude entries smaller than SIZE if positive,
                          or entries greater than SIZE if negative
      --time            show time of the last modification of any file in the
                          directory, or any of its subdirectories
      --time=WORD       show time as WORD instead of modification time:
                          atime, access, use, ctime or status
      --time-style=STYLE  show times using STYLE, which can be:
                            full-iso, long-iso, iso, or +FORMAT;
                            FORMAT is interpreted like in 'date'
  -X, --exclude-from=FILE  exclude files that match any pattern in FILE
      --exclude=PATTERN    exclude files that match PATTERN
  -x, --one-file-system    skip directories on different file systems
  -J, --json            emit NDJSON records (Dogfish extension)
      --help            display this help and exit
      --version         output version information and exit

Directory trees are read in parallel; output order matches GNU du. On a
terminal (without -a/-d/-s/-S) Dogfish shows the top level sorted by size
with usage bars.
";

#[derive(Serialize, Clone)]
struct DuRecord {
    path: String,
    size: u64,
    size_human: String,
    files: u64,
}

/// Which timestamp `--time` shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DuTime {
    Mtime,
    Atime,
    Ctime,
}

/// What `du` measures and how it walks (shared by the parallel collectors).
struct DuConfig {
    all: bool,
    apparent: bool,
    inodes: bool,
    count_links: bool,
    /// Directories are de-duplicated too (several operands, or `-L`).
    hash_all: bool,
    follow_all: bool,
    one_file_system: bool,
    excludes: Vec<Vec<u8>>,
    /// `--time`: the timestamp to keep the latest of.
    time: Option<DuTime>,
}

/// Earlier than any timestamp (GNU's `TYPE_MINIMUM (time_t), -1`).
const NO_TIME: Timespec = Timespec {
    sec: i64::MIN,
    nsec: -1,
};

/// What `du` needs of a `stat`.
#[derive(Clone, Copy)]
struct DuStat {
    dev: u64,
    ino: u64,
    nlink: u64,
    mode: u32,
    size: u64,
    blocks: u64,
    /// The `--time` timestamp (unset without `--time`).
    time: Timespec,
}

impl DuStat {
    #[allow(clippy::unnecessary_cast)] // the field types vary by platform
    fn from_stat(st: &libc::stat, time: Option<DuTime>) -> Self {
        let (sec, nsec) = match time {
            None => (0, 0),
            Some(DuTime::Mtime) => (st.st_mtime as i64, st.st_mtime_nsec as i64),
            Some(DuTime::Atime) => (st.st_atime as i64, st.st_atime_nsec as i64),
            Some(DuTime::Ctime) => (st.st_ctime as i64, st.st_ctime_nsec as i64),
        };
        Self {
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
            nlink: st.st_nlink as u64,
            mode: st.st_mode as u32,
            size: st.st_size as u64,
            blocks: st.st_blocks as u64,
            time: Timespec { sec, nsec },
        }
    }

    fn kind(&self) -> EntryType {
        EntryType::from_mode(self.mode)
    }

    fn is_dir(&self) -> bool {
        self.kind() == EntryType::Dir
    }
}

impl DuConfig {
    #[allow(clippy::unnecessary_cast)] // `mode_t` is `u16` on macOS
    fn size_of(&self, st: &DuStat) -> u64 {
        if self.inodes {
            1
        } else if self.apparent {
            let regular = st.mode & libc::S_IFMT as u32 == libc::S_IFREG as u32;
            if regular || st.kind() == EntryType::Symlink {
                st.size
            } else {
                0
            }
        } else {
            st.blocks * 512
        }
    }

    /// The (dev, ino) key under which a hard-linked entry is counted once.
    fn dedup_key(&self, st: &DuStat) -> Option<(u64, u64)> {
        let linked = if st.is_dir() {
            self.hash_all
        } else {
            self.hash_all || st.nlink > 1
        };
        (linked && !self.count_links).then_some((st.dev, st.ino))
    }

    /// GNU `exclude` matching without anchoring: the pattern may match the
    /// whole path or any part of it that starts after a `/`.
    fn excluded(&self, path: &[u8]) -> bool {
        self.excludes.iter().any(|pat| {
            fnmatch(pat, path, false)
                || path.iter().enumerate().any(|(i, c)| {
                    *c == b'/'
                        && path.get(i + 1) != Some(&b'/')
                        && fnmatch(pat, &path[i + 1..], false)
                })
        })
    }
}

/// A directory read during the collection phase.
struct DuDir {
    meta: DuStat,
    /// Combined size, count and latest time of entries that need neither
    /// printing nor de-duplication (single-link files without `-a`).
    plain_size: u64,
    plain_files: u64,
    plain_time: Timespec,
    /// The names `entries` refer to.
    names: Names,
    entries: Vec<DuEntry>,
    read_error: Option<io::Error>,
    /// The walk could not climb back into this directory after reading a
    /// deep subdirectory (it was moved meanwhile): GNU's `fts_read` fails.
    lost: Option<io::Error>,
}

impl DuDir {
    fn empty(meta: DuStat) -> Self {
        Self {
            meta,
            plain_size: 0,
            plain_files: 0,
            plain_time: NO_TIME,
            names: Names::default(),
            entries: Vec::new(),
            read_error: None,
            lost: None,
        }
    }

    fn unreadable(meta: DuStat, error: io::Error) -> Self {
        let mut dir = Self::empty(meta);
        dir.read_error = Some(error);
        dir
    }

    fn name(&self, index: usize) -> &[u8] {
        self.names.get(index).0.to_bytes()
    }
}

/// An entry of a [`DuDir`]; `name` indexes its `names`.
enum DuEntry {
    File {
        name: usize,
        meta: DuStat,
    },
    Dir {
        name: usize,
        dir: Box<DuDir>,
    },
    /// Could not be stat'ed; `None` is a dangling symlink under `-L`, which GNU
    /// reports without an error string.
    Unreadable {
        name: usize,
        error: Option<io::Error>,
    },
}

/// GNU's message for a file that could not be `stat`ed.
fn du_access_error(path: &[u8], error: Option<&io::Error>) -> String {
    let path = OsStr::from_bytes(path);
    match error {
        Some(e) => format!("cannot access {}: {}", qa(path), gnu_io_error(e)),
        None => format!("cannot access {}", qa(path)),
    }
}

/// `stat` for `-L`: `Err(None)` marks a dangling symlink.
fn du_stat_follow(at: At<'_>) -> std::result::Result<libc::stat, Option<io::Error>> {
    at.stat(true).map_err(|e| match at.stat(false) {
        Ok(st) if EntryType::of(&st) == EntryType::Symlink => None,
        _ => Some(e),
    })
}

/// GNU fts path joining: `dir` + `/` + `name`, without doubling a trailing `/`.
fn fts_join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = dir.to_vec();
    if path.last() != Some(&b'/') {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

/// How to reach entry `name` of a directory: through its descriptor, or by
/// its full path (deep under -L, where none is kept open).
fn entry_at<'a>(fd: Option<&DirFd>, name: &'a CStr, full: Option<&'a CStr>) -> At<'a> {
    match (fd, full) {
        (Some(fd), _) => At { dir: fd.fd(), name },
        (None, Some(path)) => At::cwd(path),
        // A path with a NUL cannot be reached; EBADF says so.
        (None, None) => At { dir: -1, name },
    }
}

/// A directory still to be read: how to reach and print it, its ancestors
/// (for loop detection), its depth, and where its entry sits in the tree
/// (entry indices from the operand down).
struct DuJob {
    display: Vec<u8>,
    meta: DuStat,
    lineage: Vec<(u64, u64)>,
    depth: usize,
    slot: Vec<usize>,
}

/// What reading a directory shares across the walk.
struct DuCollect<'a> {
    cfg: &'a DuConfig,
    root_dev: u64,
    /// When set, subdirectories are left here as jobs instead of being read.
    defer: Option<Vec<DuJob>>,
}

impl DuCollect<'_> {
    /// Opens and reads the directory `at` (whose printed name is `display`)
    /// and its subdirectories.
    fn dir(
        &mut self,
        at: At<'_>,
        display: &[u8],
        meta: DuStat,
        ancestors: &[(u64, u64)],
        depth: usize,
        slot: &[usize],
    ) -> DuDir {
        // The operand was stat'ed through any symlink; below it, only -L follows.
        match at.open_dir(depth > 0 && !self.cfg.follow_all) {
            Ok(fd) => self.dir_fd(fd, display, meta, ancestors, depth, slot).0,
            Err(e) => DuDir::unreadable(meta, e),
        }
    }

    /// Reads the open directory `fd` and its subdirectories, each entry
    /// `stat`ed relative to its directory. Entry order is kept, so the
    /// sequential pass sees the same readdir order GNU's fts does.
    ///
    /// Gives the descriptor back for the caller to climb back up from, or
    /// `None` if this directory was lost (see [`DuDir::lost`]).
    fn dir_fd(
        &mut self,
        fd: DirFd,
        display: &[u8],
        meta: DuStat,
        ancestors: &[(u64, u64)],
        depth: usize,
        slot: &[usize],
    ) -> (DuDir, Option<DirFd>) {
        let cfg = self.cfg;
        let mut dir = DuDir::empty(meta);
        let mut names = Names::default();
        if let Err(e) = fd.read_names(&mut names) {
            dir.read_error = Some(e);
        }
        let mut lineage = ancestors.to_vec();
        lineage.push((meta.dev, meta.ino));
        // Under -L, `..` is not where a followed link came from, so the walk
        // cannot climb back up through it. Deep down, entries are then
        // reached by full path instead, with no directory held open, as
        // GNU's logical walk (FTS_NOCHDIR) reaches every entry; such a path
        // can also fail as GNU's does (ELOOP, ENAMETOOLONG).
        let by_path = cfg.follow_all && depth >= MAX_OPEN_DIRS;
        let fd = (!by_path).then_some(fd);
        let mut subdirs: Vec<(usize, Vec<u8>)> = Vec::new();
        for i in 0..names.len() {
            let (name, _, _) = names.get(i);
            let full = by_path
                .then(|| CString::new(fts_join(display, name.to_bytes())).ok())
                .flatten();
            let child = entry_at(fd.as_ref(), name, full.as_deref());
            let st = if cfg.follow_all {
                du_stat_follow(child)
            } else {
                child.stat(false).map_err(Some)
            };
            let meta = match st {
                Ok(st) => DuStat::from_stat(&st, cfg.time),
                Err(error) => {
                    dir.entries.push(DuEntry::Unreadable { name: i, error });
                    continue;
                }
            };
            if cfg.one_file_system && meta.dev != self.root_dev {
                continue;
            }
            let needs_display = !cfg.excludes.is_empty() || meta.is_dir();
            let child_display = if needs_display {
                fts_join(display, name.to_bytes())
            } else {
                Vec::new()
            };
            if !cfg.excludes.is_empty() && cfg.excluded(&child_display) {
                continue;
            }
            if meta.is_dir() {
                // A directory that is its own ancestor (a symlink loop under
                // -L, a bind mount): GNU's fts reports FTS_DC, which du skips
                // without counting it.
                if lineage.contains(&(meta.dev, meta.ino)) {
                    continue;
                }
                subdirs.push((dir.entries.len(), child_display));
                dir.entries.push(DuEntry::Dir {
                    name: i,
                    dir: Box::new(DuDir::empty(meta)),
                });
            } else if !cfg.all && cfg.dedup_key(&meta).is_none() {
                dir.plain_size += cfg.size_of(&meta);
                dir.plain_files += 1;
                dir.plain_time = dir.plain_time.max(meta.time);
            } else {
                dir.entries.push(DuEntry::File { name: i, meta });
            }
        }
        // Deeper than this (and not under -L), a subdirectory is read with
        // this directory let go of, and this one is got back through the
        // subdirectory's `..`.
        let deep = depth + 1 >= MAX_OPEN_DIRS && !cfg.follow_all;
        let mut fd = fd;
        let mut lost = None;
        for (index, child_display) in subdirs {
            let DuEntry::Dir { name, dir: sub } = &mut dir.entries[index] else {
                continue;
            };
            let mut child_slot = slot.to_vec();
            child_slot.push(index);
            if let Some(jobs) = &mut self.defer {
                jobs.push(DuJob {
                    display: child_display,
                    meta: sub.meta,
                    lineage: lineage.clone(),
                    depth: depth + 1,
                    slot: child_slot,
                });
                continue;
            }
            let sub_meta = sub.meta;
            if by_path {
                let full = CString::new(child_display.clone()).ok();
                let child = entry_at(None, names.get(*name).0, full.as_deref());
                **sub = self.dir(
                    child,
                    &child_display,
                    sub_meta,
                    &lineage,
                    depth + 1,
                    &child_slot,
                );
                continue;
            }
            let Some(this) = &fd else {
                break;
            };
            let child = At {
                dir: this.fd(),
                name: names.get(*name).0,
            };
            if !deep {
                **sub = self.dir(
                    child,
                    &child_display,
                    sub_meta,
                    &lineage,
                    depth + 1,
                    &child_slot,
                );
                continue;
            }
            let child_fd = match child.open_dir(true) {
                Ok(child_fd) => child_fd,
                Err(e) => {
                    **sub = DuDir::unreadable(sub_meta, e);
                    continue;
                }
            };
            fd = None;
            let (result, child_fd) = self.dir_fd(
                child_fd,
                &child_display,
                sub_meta,
                &lineage,
                depth + 1,
                &child_slot,
            );
            **sub = result;
            // A lost subdirectory has reported it; this one is lost with it.
            if let Some(child_fd) = child_fd {
                match child_fd.open_parent((meta.dev, meta.ino)) {
                    Ok(parent) => fd = Some(parent),
                    Err(e) => lost = Some(e),
                }
            }
        }
        dir.lost = lost;
        dir.names = names;
        (dir, fd)
    }

    /// Reads a job's directory, reached by its full path.
    fn job(&mut self, job: &DuJob) -> DuDir {
        match CString::new(job.display.clone()) {
            Ok(path) => self.dir(
                At::cwd(&path),
                &job.display,
                job.meta,
                &job.lineage,
                job.depth,
                &job.slot,
            ),
            Err(_) => DuDir::unreadable(job.meta, io::Error::from_raw_os_error(libc::EINVAL)),
        }
    }
}

/// Puts a job's result into the tree, at the entry its slot names.
fn du_place(root: &mut DuDir, slot: &[usize], result: DuDir) {
    let mut dir = root;
    for &index in slot {
        match dir.entries.get_mut(index) {
            Some(DuEntry::Dir { dir: sub, .. }) => dir = sub,
            _ => return,
        }
    }
    *dir = result;
}

/// Subtrees read in parallel once a walk has this many; fewer, and the tree
/// is read on one thread, as starting workers would cost more than they save.
const PARALLEL_MIN_JOBS: usize = 16;

/// Threads reading a large tree. More cut the wall time further but cost CPU
/// time: parallel `stat`s of one tree contend in the kernel.
const MAX_WORKERS: usize = 2;

/// Reads the operand directory `at` and everything below it. The top levels
/// are read on this thread until enough subdirectories are known; those
/// subtrees then go to a few worker threads, each taking the next as it
/// finishes. A small tree stays on one thread.
fn du_collect(at: At<'_>, display: &[u8], meta: DuStat, cfg: &DuConfig, root_dev: u64) -> DuDir {
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get().min(MAX_WORKERS));
    let mut c = DuCollect {
        cfg,
        root_dev,
        defer: (workers > 1).then(Vec::new),
    };
    let mut root = c.dir(at, display, meta, &[], 0, &[]);
    let Some(mut jobs) = c.defer.take() else {
        return root;
    };
    // Breadth first, one level at a time, while there are too few subtrees
    // to share.
    let mut levels = 1;
    while !jobs.is_empty() && jobs.len() < PARALLEL_MIN_JOBS && levels < 4 {
        c.defer = Some(Vec::new());
        for job in &jobs {
            let result = c.job(job);
            du_place(&mut root, &job.slot, result);
        }
        jobs = c.defer.take().unwrap_or_default();
        levels += 1;
    }
    if jobs.len() < PARALLEL_MIN_JOBS {
        for job in &jobs {
            let result = c.job(job);
            du_place(&mut root, &job.slot, result);
        }
        return root;
    }
    // Each worker (this thread among them) takes the next job until none is left.
    let next = AtomicUsize::new(0);
    let work = || {
        let mut c = DuCollect {
            cfg,
            root_dev,
            defer: None,
        };
        let mut done = Vec::new();
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            let Some(job) = jobs.get(i) else { break };
            done.push((i, c.job(job)));
        }
        done
    };
    let results: Vec<Vec<(usize, DuDir)>> = std::thread::scope(|s| {
        let helpers: Vec<_> = (1..workers).map(|_| s.spawn(work)).collect();
        let mut results = vec![work()];
        for h in helpers {
            results.push(h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)));
        }
        results
    });
    for (i, result) in results.into_iter().flatten() {
        du_place(&mut root, &jobs[i].slot, result);
    }
    root
}

/// One line of output.
struct DuLine {
    path: Vec<u8>,
    size: u64,
    files: u64,
    time: Timespec,
}

/// Where streamed `du` lines go.
/// Where streamed `du` lines go; `None` asks it to flush what it holds.
type DuSink<'a> = &'a mut dyn FnMut(Option<&DuLine>) -> io::Result<()>;

/// What a walk adds up: size, file count and the latest `--time`.
#[derive(Clone, Copy)]
struct DuSum {
    size: u64,
    files: u64,
    time: Timespec,
}

impl DuSum {
    const ZERO: Self = Self {
        size: 0,
        files: 0,
        time: NO_TIME,
    };

    fn add(&mut self, other: Self) {
        self.size += other.size;
        self.files += other.files;
        self.time = self.time.max(other.time);
    }
}

/// The sequential, GNU-ordered pass: de-duplication, depth and threshold
/// filtering, and output.
struct DuWalker<'a> {
    cfg: &'a DuConfig,
    seen: std::collections::HashSet<(u64, u64)>,
    max_depth: usize,
    separate_dirs: bool,
    threshold: i128,
    lines: Vec<DuLine>,
    sink: Option<DuSink<'a>>,
    status: i32,
}

impl DuWalker<'_> {
    fn emit(&mut self, path: Vec<u8>, sum: DuSum) -> io::Result<()> {
        let size_i = i128::from(sum.size);
        let keep = if self.threshold >= 0 {
            size_i >= self.threshold
        } else {
            size_i <= -self.threshold
        };
        if !keep {
            return Ok(());
        }
        let line = DuLine {
            path,
            size: sum.size,
            files: sum.files,
            time: sum.time,
        };
        match self.sink.as_mut() {
            Some(sink) => sink(Some(&line)),
            None => {
                self.lines.push(line);
                Ok(())
            }
        }
    }

    /// Reports a failure. Like gnulib's `error` (and GNU du, which flushes
    /// each line), what was printed so far goes out first, so the two
    /// streams interleave as GNU's do.
    fn error(&mut self, msg: String) {
        if let Some(sink) = self.sink.as_mut() {
            // A failed flush shows up again on the next write.
            let _ = sink(None);
        }
        eprintln!("du: {msg}");
        self.status = 1;
    }

    /// First visit of an entry: false if it was already counted.
    fn first_visit(&mut self, meta: &DuStat) -> bool {
        match self.cfg.dedup_key(meta) {
            Some(key) => self.seen.insert(key),
            None => true,
        }
    }

    /// What one entry counts for by itself.
    fn own(&self, meta: &DuStat) -> DuSum {
        DuSum {
            size: self.cfg.size_of(meta),
            files: 1,
            time: meta.time,
        }
    }

    /// Walks a collected directory; returns what its subtree adds up to.
    fn walk_dir(&mut self, dir: &DuDir, display: Vec<u8>, depth: usize) -> io::Result<DuSum> {
        if let Some(e) = &dir.read_error {
            self.error(format!(
                "cannot read directory {}: {}",
                qa(OsStr::from_bytes(&display)),
                gnu_io_error(e)
            ));
        }
        let mut own = DuSum {
            size: self.cfg.size_of(&dir.meta) + dir.plain_size,
            files: dir.plain_files,
            time: dir.meta.time.max(dir.plain_time),
        };
        let mut sub = DuSum::ZERO;
        for entry in &dir.entries {
            match entry {
                DuEntry::File { name, meta } => {
                    if !self.first_visit(meta) {
                        continue;
                    }
                    let file = self.own(meta);
                    own.add(file);
                    if self.cfg.all && depth < self.max_depth {
                        self.emit(fts_join(&display, dir.name(*name)), file)?;
                    }
                }
                DuEntry::Dir { name, dir: subdir } => {
                    if !self.first_visit(&subdir.meta) {
                        continue;
                    }
                    let total =
                        self.walk_dir(subdir, fts_join(&display, dir.name(*name)), depth + 1)?;
                    sub.add(DuSum { files: 0, ..total });
                    own.files += total.files;
                }
                DuEntry::Unreadable { name, error } => {
                    let path = fts_join(&display, dir.name(*name));
                    self.error(du_access_error(&path, error.as_ref()));
                }
            }
        }
        if let Some(e) = &dir.lost {
            self.error(format!(
                "fts_read failed: {}: {}",
                qf(OsStr::from_bytes(&display)),
                gnu_io_error(e)
            ));
        }
        let mut printed = own;
        if !self.separate_dirs {
            printed.add(sub);
        }
        if depth <= self.max_depth {
            self.emit(display, printed)?;
        }
        let mut total = own;
        total.add(sub);
        Ok(total)
    }

    /// Counts one operand (GNU's `du_files` for one name); returns what it
    /// adds to the grand total.
    fn operand(
        &mut self,
        operand: &[u8],
        deref_args: bool,
        root: &mut Vec<usize>,
    ) -> io::Result<DuSum> {
        let cfg = self.cfg;
        // fts keeps at most one of several trailing slashes.
        let display = fts_root_name(operand).to_vec();
        let Ok(c_target) = CString::new(display.clone()) else {
            return Ok(DuSum::ZERO);
        };
        let at = At::cwd(&c_target);
        let meta = if cfg.follow_all {
            du_stat_follow(at)
        } else if deref_args {
            at.stat(true).or_else(|_| at.stat(false)).map_err(Some)
        } else {
            at.stat(false).map_err(Some)
        };
        let meta = match meta {
            Ok(st) => DuStat::from_stat(&st, cfg.time),
            Err(e) => {
                self.error(du_access_error(&display, e.as_ref()));
                return Ok(DuSum::ZERO);
            }
        };
        if !cfg.excludes.is_empty() && cfg.excluded(&display) {
            return Ok(DuSum::ZERO);
        }
        if !self.first_visit(&meta) {
            return Ok(DuSum::ZERO);
        }
        let sum = if meta.is_dir() {
            let tree = du_collect(at, &display, meta, cfg, meta.dev);
            self.walk_dir(&tree, display, 0)?
        } else {
            let sum = self.own(&meta);
            self.emit(display, sum)?;
            sum
        };
        root.push(self.lines.len());
        Ok(sum)
    }
}

/// GNU du's `--time-style` (or TIME_STYLE) as a strftime format.
fn du_time_format(style: Option<OsString>) -> std::result::Result<String, i32> {
    let style = match style.map(|s| s.to_string_lossy().into_owned()) {
        Some(s) => s,
        None => match std::env::var_os("TIME_STYLE") {
            // ls's "locale" means nothing here.
            None => "long-iso".to_string(),
            Some(s) if s == "locale" => "long-iso".to_string(),
            Some(s) => {
                let s = s.to_string_lossy().into_owned();
                if let Some(format) = s.strip_prefix('+') {
                    // Only the first line, as ls would use for old files.
                    format!("+{}", format.split('\n').next().unwrap_or_default())
                } else {
                    let mut rest = s.as_str();
                    while let Some(r) = rest.strip_prefix("posix-") {
                        rest = r;
                    }
                    rest.to_string()
                }
            }
        },
    };
    if let Some(format) = style.strip_prefix('+') {
        return Ok(format.to_string());
    }
    let styles = [
        ("full-iso", "%Y-%m-%d %H:%M:%S.%N %z"),
        ("long-iso", "%Y-%m-%d %H:%M"),
        ("iso", "%Y-%m-%d"),
    ];
    argmatch("du", "time style", &style, &styles).map(str::to_string)
}

/// A `--time` timestamp as GNU's `show_date` prints it.
fn du_show_time(format: &str, t: Timespec) -> String {
    format_local_time(format, t).unwrap_or_else(|| {
        let secs = t.sec.to_string();
        eprintln!("du: time {} is out of range", quote_locale(&secs));
        secs
    })
}

pub fn run_du(args: Vec<std::ffi::OsString>, ctx: &DogfishContext) -> Result<i32> {
    let mut parsed = Getopt::new(args.into_iter().skip(1).collect(), DU_OPTIONS);
    let (mut human_opts, mut block_size) = env_block_size("DU_BLOCK_SIZE");
    let mut explicit_units = false;
    let mut cfg = DuConfig {
        all: false,
        apparent: false,
        inodes: false,
        count_links: false,
        hash_all: false,
        follow_all: false,
        one_file_system: false,
        excludes: Vec::new(),
        time: None,
    };
    let mut null = false;
    let mut total = false;
    let mut deref_args = false;
    let mut max_depth: Option<usize> = None;
    let mut summarize = false;
    let mut separate_dirs = false;
    let mut threshold: i128 = 0;
    let mut json = false;
    let mut files_from: Option<OsString> = None;
    let mut time_style: Option<OsString> = None;
    while let Some(item) = parsed.next() {
        let (key, raw) = match item {
            Ok(o) => o,
            Err(e) => return Ok(getopt_error("du", &e)),
        };
        let spelled = parsed.last_spelling();
        let value = raw.as_deref().map(lossy).unwrap_or_default();
        match key {
            "null" => null = true,
            "all" => cfg.all = true,
            "apparent-size" => cfg.apparent = true,
            "block-size" => match parse_block_size(&value) {
                Ok(v) => {
                    (human_opts, block_size) = v;
                    explicit_units = true;
                }
                Err(e) => {
                    eprintln!("du: {}", e.message(&spelled, &value));
                    return Ok(1);
                }
            },
            "bytes" => {
                cfg.apparent = true;
                (human_opts, block_size) = (0, 1);
                explicit_units = true;
            }
            "total" => total = true,
            "dereference-args" => deref_args = true,
            "max-depth" => match value.parse::<usize>() {
                Ok(d) if value.bytes().all(|b| b.is_ascii_digit()) => max_depth = Some(d),
                _ => {
                    let msg = format!(
                        "invalid maximum depth {}",
                        quote_bytes(value.as_bytes(), Quoting::Locale)
                    );
                    return Ok(usage_error("du", &msg));
                }
            },
            "human-readable" => {
                (human_opts, block_size) = (HUMAN_AUTOSCALE | HUMAN_SI | HUMAN_BASE_1024, 1);
                explicit_units = false;
            }
            "si" => {
                (human_opts, block_size) = (HUMAN_AUTOSCALE | HUMAN_SI, 1);
                explicit_units = true;
            }
            "inodes" => cfg.inodes = true,
            "kilobytes" => {
                (human_opts, block_size) = (0, 1024);
                explicit_units = true;
            }
            "megabytes" => {
                (human_opts, block_size) = (0, 1024 * 1024);
                explicit_units = true;
            }
            "dereference" => cfg.follow_all = true,
            "no-dereference" => {
                cfg.follow_all = false;
                deref_args = false;
            }
            "count-links" => cfg.count_links = true,
            "separate-dirs" => separate_dirs = true,
            "summarize" => summarize = true,
            "threshold" => {
                let (negative, digits) = match value.strip_prefix('-') {
                    Some(rest) => (true, rest),
                    None => (false, value.as_str()),
                };
                match parse_size_suffixed(digits) {
                    // GNU names the option in full here, however it was spelled.
                    Ok(0) if negative => {
                        eprintln!("du: invalid --threshold argument '-0'");
                        return Ok(1);
                    }
                    Ok(n) => {
                        threshold = if negative {
                            -i128::from(n)
                        } else {
                            i128::from(n)
                        }
                    }
                    Err(e) => {
                        eprintln!("du: {}", e.message(&spelled, &value));
                        return Ok(1);
                    }
                }
            }
            "exclude" => cfg.excludes.push(raw.unwrap_or_default().into_vec()),
            "exclude-from" => {
                let data = if value == "-" {
                    let mut buf = Vec::new();
                    io::Read::read_to_end(&mut io::stdin(), &mut buf).map(|_| buf)
                } else {
                    fs::read(raw.as_deref().unwrap_or_default())
                };
                match data {
                    Ok(data) => cfg.excludes.extend(
                        data.split(|b| *b == b'\n')
                            .filter(|l| !l.is_empty())
                            .map(<[u8]>::to_vec),
                    ),
                    Err(e) => {
                        eprintln!(
                            "du: {}: {}",
                            qf(raw.as_deref().unwrap_or_default()),
                            gnu_io_error(&e)
                        );
                        return Ok(1);
                    }
                }
            }
            "files0-from" => files_from = raw.clone(),
            "time" => {
                let words = [
                    ("atime", DuTime::Atime),
                    ("access", DuTime::Atime),
                    ("use", DuTime::Atime),
                    ("ctime", DuTime::Ctime),
                    ("status", DuTime::Ctime),
                ];
                cfg.time = Some(match &raw {
                    None => DuTime::Mtime,
                    Some(word) => match argmatch("du", "--time", word, &words) {
                        Ok(t) => t,
                        Err(status) => return Ok(status),
                    },
                });
            }
            "time-style" => time_style = raw.clone(),
            "one-file-system" => cfg.one_file_system = true,
            "json" => json = true,
            "help" => return Ok(print_help(DU_HELP)),
            "version" => return Ok(print_version("du")),
            _ => unreachable!("unhandled du option"),
        }
    }

    if cfg.all && summarize {
        return Ok(usage_error(
            "du",
            "cannot both summarize and show all entries",
        ));
    }
    if summarize && max_depth == Some(0) {
        eprintln!("du: warning: summarizing is the same as using --max-depth=0");
    }
    if let (true, Some(d)) = (summarize, max_depth)
        && d != 0
    {
        let msg = format!("warning: summarizing conflicts with --max-depth={d}");
        return Ok(usage_error("du", &msg));
    }
    if cfg.inodes {
        if cfg.apparent {
            eprintln!("du: warning: options --apparent-size and -b are ineffective with --inodes");
        }
        block_size = 1;
    }
    let time_format = match cfg.time {
        Some(_) => match du_time_format(time_style) {
            Ok(f) => Some(f),
            Err(status) => return Ok(status),
        },
        None => None,
    };
    let operands = parsed.operands();
    // The names to count: the operands, or a --files0-from list read as it goes.
    let mut list = None;
    let mut targets: Vec<OsString> = Vec::new();
    if let Some(from) = &files_from {
        if let Some(extra) = operands.first() {
            return Ok(extra_operand_error("du", extra, 1));
        }
        match Files0From::open(from) {
            Ok(l) => list = Some(l),
            Err(e) => {
                eprintln!("du: {}", Files0From::open_error(from, &e));
                return Ok(1);
            }
        }
        // GNU cannot count the names up front, so assumes several.
        cfg.hash_all = true;
    } else {
        targets = if operands.is_empty() {
            vec![OsString::from(".")]
        } else {
            operands
        };
        cfg.hash_all = targets.len() > 1 || cfg.follow_all;
    }

    // On a terminal, the default view is the Dogfish top-level breakdown.
    let rich = ctx.stdout_is_tty() && !json;
    let rich_overview = rich && !cfg.all && !summarize && max_depth.is_none() && !separate_dirs;
    let max_depth = if summarize {
        0
    } else if rich_overview {
        1
    } else {
        max_depth.unwrap_or(usize::MAX)
    };

    ctx.telemetry
        .set_strategy("rayon parallel traversal, GNU-ordered accounting");
    let mut out = io::BufWriter::new(stdout::stdout());
    let eol = if null { b'\0' } else { b'\n' };
    let format_size_gnu = |n: u64| human_readable(n, human_opts, 1, block_size);
    let mut write_line = |line: Option<&DuLine>| -> io::Result<()> {
        let Some(line) = line else {
            return out.flush();
        };
        out.write_all(format_size_gnu(line.size).as_bytes())?;
        out.write_all(b"\t")?;
        if let Some(format) = &time_format {
            if format_local_time(format, line.time).is_none() {
                // GNU's error() flushes what was printed first.
                out.flush()?;
            }
            out.write_all(du_show_time(format, line.time).as_bytes())?;
            out.write_all(b"\t")?;
        }
        out.write_all(&line.path)?;
        out.write_all(&[eol])
    };
    let streaming = !rich && !json;
    // GNU flushes standard output after every line.
    stdout::gnu_flushes_early();
    let mut walker = DuWalker {
        cfg: &cfg,
        seen: std::collections::HashSet::new(),
        max_depth,
        separate_dirs,
        threshold,
        lines: Vec::new(),
        sink: if streaming {
            Some(&mut write_line)
        } else {
            None
        },
        status: 0,
    };

    let mut grand = DuSum::ZERO;
    let mut rich_groups: Vec<usize> = Vec::new();
    let mut next_operand = targets.into_iter();
    loop {
        let name = match &mut list {
            None => match next_operand.next() {
                Some(name) => name,
                None => break,
            },
            Some(list) => match list.next_os_name() {
                Ok(Some(name)) => {
                    if let Some(msg) = list.check_name(&name, list.count(), true) {
                        walker.error(msg);
                        continue;
                    }
                    name
                }
                Ok(None) => break,
                Err(e) => {
                    walker.error(list.read_error(&e));
                    break;
                }
            },
        };
        if name.is_empty() {
            walker.error("invalid zero-length file name".to_string());
            continue;
        }
        grand.add(walker.operand(name.as_bytes(), deref_args, &mut rich_groups)?);
    }
    if total {
        walker.emit(b"total".to_vec(), grand)?;
    }
    let status = walker.status;
    let mut lines = std::mem::take(&mut walker.lines);
    drop(walker);
    ctx.telemetry.add_items(grand.files);

    if json {
        let records: Vec<DuRecord> = lines
            .iter()
            .map(|l| DuRecord {
                path: String::from_utf8_lossy(&l.path).into_owned(),
                size: l.size,
                size_human: format_size(l.size, true),
                files: l.files,
            })
            .collect();
        emit_objects_or_table(&records, ctx, true, false)?;
        return Ok(status);
    }
    if rich {
        if rich_overview {
            // Largest subdirectories first within each operand; its total last.
            let mut start = 0;
            for end in rich_groups {
                if end > start + 1 {
                    lines[start..end - 1].sort_by_key(|l| std::cmp::Reverse(l.size));
                }
                start = end;
            }
        }
        let st = ctx.style();
        let max = lines.iter().map(|l| l.size).max().unwrap_or(1).max(1);
        for l in &lines {
            let size = if explicit_units || cfg.inodes {
                format_size_gnu(l.size)
            } else {
                format_size(l.size, true)
            };
            let row = format!(
                "{}  {}  {}\n",
                st.bold_green(&format!("{size:>10}")),
                sparkline_bar(l.size as f64 / max as f64, 10, st),
                st.bold(&String::from_utf8_lossy(&l.path))
            );
            out.write_all(row.as_bytes())?;
        }
    }
    out.flush()?;
    Ok(status)
}

/// du's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("du", DU_HELP, DU_OPTIONS)
        .choices("time", ["atime", "access", "use", "ctime", "status"])
        .choices("time-style", ["full-iso", "long-iso", "iso"])
}
