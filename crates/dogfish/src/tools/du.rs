//! du — Parallel disk usage with sparklines on a TTY, GNU `du` otherwise

use std::ffi::{CStr, CString};
use std::fs;
use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use dogfish_core::{
    DogfishContext, emit_objects_or_table, format_size, gnu_io_error, io::ignore_broken_pipe,
    sparkline_bar, stdout,
};
use serde::Serialize;

use crate::common::dirfd::{At, DirFd, EntryType, MAX_OPEN_DIRS, Names};
use crate::common::fnmatch::fnmatch;
use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy_all, print_help, print_version, unsupported,
    usage_error,
};
use crate::common::human::{
    HUMAN_AUTOSCALE, HUMAN_BASE_1024, HUMAN_SI, SizeError, env_block_size, human_readable,
    parse_block_size, parse_size_suffixed,
};
use crate::common::quote::{Quoting, quote_af, quote_bytes, quote_f};

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
    OptSpec::long("files0-from", HasArg::Required, "unsupported"),
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
    OptSpec::long("time", HasArg::Optional, "unsupported"),
    OptSpec::long("time-style", HasArg::Required, "unsupported"),
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
}

/// What `du` needs of a `stat`.
#[derive(Clone, Copy)]
struct DuStat {
    dev: u64,
    ino: u64,
    nlink: u64,
    mode: u32,
    size: u64,
    blocks: u64,
}

impl DuStat {
    #[allow(clippy::unnecessary_cast)] // the field types vary by platform
    fn from_stat(st: &libc::stat) -> Self {
        Self {
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
            nlink: st.st_nlink as u64,
            mode: st.st_mode as u32,
            size: st.st_size as u64,
            blocks: st.st_blocks as u64,
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
    /// Combined size and count of entries that need neither printing nor
    /// de-duplication (single-link files without `-a`).
    plain_size: u64,
    plain_files: u64,
    /// The names `entries` refer to.
    names: Names,
    entries: Vec<DuEntry>,
    read_error: Option<io::Error>,
}

impl DuDir {
    fn empty(meta: DuStat) -> Self {
        Self {
            meta,
            plain_size: 0,
            plain_files: 0,
            names: Names::default(),
            entries: Vec::new(),
            read_error: None,
        }
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

fn du_access_error(path: &str, error: Option<&io::Error>) {
    match error {
        Some(e) => eprintln!("du: cannot access {}: {}", quote_af(path), gnu_io_error(e)),
        None => eprintln!("du: cannot access {}", quote_af(path)),
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

/// How to reach entry `name` of a directory: through its descriptor, or
/// (deep down, where none is kept open) by its full path.
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
    /// Reads `at` (whose printed name is `display`) and its subdirectories,
    /// each entry `stat`ed relative to its open directory. Entry order is
    /// kept, so the sequential pass sees the same readdir order GNU's fts does.
    fn dir(
        &mut self,
        at: At<'_>,
        display: &[u8],
        meta: DuStat,
        ancestors: &[(u64, u64)],
        depth: usize,
        slot: &[usize],
    ) -> DuDir {
        let cfg = self.cfg;
        let mut dir = DuDir::empty(meta);
        // The operand was stat'ed through any symlink; below it, only -L follows.
        let fd = match at.open_dir(depth > 0 && !cfg.follow_all) {
            Ok(fd) => fd,
            Err(e) => {
                dir.read_error = Some(e);
                return dir;
            }
        };
        let mut names = Names::default();
        if let Err(e) = fd.read_names(&mut names) {
            dir.read_error = Some(e);
        }
        let mut lineage = ancestors.to_vec();
        lineage.push((meta.dev, meta.ino));
        // Deep down, entries are reached by full path, with no directory held open.
        let fd = (depth < MAX_OPEN_DIRS).then_some(fd);
        let mut subdirs: Vec<(usize, Vec<u8>)> = Vec::new();
        for i in 0..names.len() {
            let (name, _, _) = names.get(i);
            let full = fd
                .is_none()
                .then(|| CString::new(fts_join(display, name.to_bytes())).ok())
                .flatten();
            let child = entry_at(fd.as_ref(), name, full.as_deref());
            let st = if cfg.follow_all {
                du_stat_follow(child)
            } else {
                child.stat(false).map_err(Some)
            };
            let meta = match st {
                Ok(st) => DuStat::from_stat(&st),
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
                // A directory that is its own ancestor (a symlink loop under -L)
                // stays empty; the walk then skips it as already counted.
                if !lineage.contains(&(meta.dev, meta.ino)) {
                    subdirs.push((dir.entries.len(), child_display));
                }
                dir.entries.push(DuEntry::Dir {
                    name: i,
                    dir: Box::new(DuDir::empty(meta)),
                });
            } else if !cfg.all && cfg.dedup_key(&meta).is_none() {
                dir.plain_size += cfg.size_of(&meta);
                dir.plain_files += 1;
            } else {
                dir.entries.push(DuEntry::File { name: i, meta });
            }
        }
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
            let name = names.get(*name).0;
            let full = fd
                .is_none()
                .then(|| CString::new(child_display.clone()).ok())
                .flatten();
            let child = entry_at(fd.as_ref(), name, full.as_deref());
            let meta = sub.meta;
            **sub = self.dir(
                child,
                &child_display,
                meta,
                &lineage,
                depth + 1,
                &child_slot,
            );
        }
        dir.names = names;
        dir
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
            Err(_) => {
                let mut dir = DuDir::empty(job.meta);
                dir.read_error = Some(io::Error::from_raw_os_error(libc::EINVAL));
                dir
            }
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
}

/// Where streamed `du` lines go.
type DuSink<'a> = &'a mut dyn FnMut(&DuLine) -> io::Result<()>;

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
    fn emit(&mut self, path: Vec<u8>, size: u64, files: u64) -> io::Result<()> {
        let size_i = i128::from(size);
        let keep = if self.threshold >= 0 {
            size_i >= self.threshold
        } else {
            size_i <= -self.threshold
        };
        if !keep {
            return Ok(());
        }
        let line = DuLine { path, size, files };
        match self.sink.as_mut() {
            Some(sink) => sink(&line),
            None => {
                self.lines.push(line);
                Ok(())
            }
        }
    }

    /// First visit of an entry: false if it was already counted.
    fn first_visit(&mut self, meta: &DuStat) -> bool {
        match self.cfg.dedup_key(meta) {
            Some(key) => self.seen.insert(key),
            None => true,
        }
    }

    /// Walks a collected directory; returns (total size, file count) of the subtree.
    fn walk_dir(&mut self, dir: &DuDir, display: Vec<u8>, depth: usize) -> io::Result<(u64, u64)> {
        if let Some(e) = &dir.read_error {
            eprintln!(
                "du: cannot read directory {}: {}",
                quote_af(&String::from_utf8_lossy(&display)),
                gnu_io_error(e)
            );
            self.status = 1;
        }
        let mut own = self.cfg.size_of(&dir.meta) + dir.plain_size;
        let mut files = dir.plain_files;
        let mut sub_size = 0;
        for entry in &dir.entries {
            match entry {
                DuEntry::File { name, meta } => {
                    if !self.first_visit(meta) {
                        continue;
                    }
                    let size = self.cfg.size_of(meta);
                    own += size;
                    files += 1;
                    if self.cfg.all && depth < self.max_depth {
                        self.emit(fts_join(&display, dir.name(*name)), size, 1)?;
                    }
                }
                DuEntry::Dir { name, dir: sub } => {
                    if !self.first_visit(&sub.meta) {
                        continue;
                    }
                    let (size, n) =
                        self.walk_dir(sub, fts_join(&display, dir.name(*name)), depth + 1)?;
                    sub_size += size;
                    files += n;
                }
                DuEntry::Unreadable { name, error } => {
                    let path = fts_join(&display, dir.name(*name));
                    du_access_error(&String::from_utf8_lossy(&path), error.as_ref());
                    self.status = 1;
                }
            }
        }
        let printed = if self.separate_dirs {
            own
        } else {
            own + sub_size
        };
        if depth <= self.max_depth {
            self.emit(display, printed, files)?;
        }
        Ok((own + sub_size, files))
    }
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
    };
    let mut null = false;
    let mut total = false;
    let mut deref_args = false;
    let mut max_depth: Option<usize> = None;
    let mut summarize = false;
    let mut separate_dirs = false;
    let mut threshold: i128 = 0;
    let mut json = false;
    while let Some(item) = parsed.next_text() {
        let opt = match item {
            Ok(o) => o,
            Err(e) => return Ok(getopt_error("du", &e)),
        };
        let value = opt.value.clone().unwrap_or_default();
        match opt.key {
            "null" => null = true,
            "all" => cfg.all = true,
            "apparent-size" => cfg.apparent = true,
            "block-size" => match parse_block_size(&value) {
                Ok(v) => {
                    (human_opts, block_size) = v;
                    explicit_units = true;
                }
                Err(e) => {
                    eprintln!("du: {}", e.message(&opt.spelled, &value));
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
                    Ok(0) if negative => {
                        let msg = SizeError::Invalid.message(&opt.spelled, &value);
                        eprintln!("du: {msg}");
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
                        eprintln!("du: {}", e.message(&opt.spelled, &value));
                        return Ok(1);
                    }
                }
            }
            "exclude" => cfg.excludes.push(value.into_bytes()),
            "exclude-from" => {
                let data = if value == "-" {
                    let mut buf = Vec::new();
                    io::Read::read_to_end(&mut io::stdin(), &mut buf).map(|_| buf)
                } else {
                    fs::read(&value)
                };
                match data {
                    Ok(data) => cfg.excludes.extend(
                        data.split(|b| *b == b'\n')
                            .filter(|l| !l.is_empty())
                            .map(<[u8]>::to_vec),
                    ),
                    Err(e) => {
                        eprintln!("du: {}: {}", quote_f(&value), gnu_io_error(&e));
                        return Ok(1);
                    }
                }
            }
            "one-file-system" => cfg.one_file_system = true,
            "json" => json = true,
            "unsupported" => return Ok(unsupported("du", &opt.spelled)),
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
    let operands = lossy_all(parsed.operands());
    let targets = if operands.is_empty() {
        vec![".".to_string()]
    } else {
        operands
    };
    cfg.hash_all = targets.len() > 1 || cfg.follow_all;

    // On a terminal, the default view is the Dogfish top-level breakdown.
    let rich = ctx.stdout_is_tty && !json;
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
    let mut write_line = |line: &DuLine| -> io::Result<()> {
        out.write_all(format_size_gnu(line.size).as_bytes())?;
        out.write_all(b"\t")?;
        out.write_all(&line.path)?;
        out.write_all(&[eol])
    };
    let streaming = !rich && !json;
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

    let mut grand_total = 0;
    let mut grand_files = 0;
    let mut rich_groups: Vec<usize> = Vec::new();
    for target in &targets {
        // fts keeps at most one of several trailing slashes.
        let mut display = target.as_bytes().to_vec();
        while display.len() > 2 && display.ends_with(b"//") {
            display.pop();
        }
        let Ok(c_target) = CString::new(target.as_bytes()) else {
            continue;
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
            Ok(st) => DuStat::from_stat(&st),
            Err(e) => {
                du_access_error(target, e.as_ref());
                walker.status = 1;
                continue;
            }
        };
        if !cfg.excludes.is_empty() && cfg.excluded(&display) {
            continue;
        }
        if !walker.first_visit(&meta) {
            continue;
        }
        let res = if meta.is_dir() {
            let tree = du_collect(at, &display, meta, &cfg, meta.dev);
            walker.walk_dir(&tree, display, 0)
        } else {
            let size = cfg.size_of(&meta);
            walker.emit(display, size, 1).map(|_| (size, 1))
        };
        match res {
            Ok((size, files)) => {
                grand_total += size;
                grand_files += files;
            }
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(0),
            Err(e) => return Err(e.into()),
        }
        rich_groups.push(walker.lines.len());
    }
    if total {
        let res = walker.emit(b"total".to_vec(), grand_total, grand_files);
        ignore_broken_pipe(res)?;
    }
    let status = walker.status;
    let mut lines = std::mem::take(&mut walker.lines);
    drop(walker);
    ctx.telemetry.add_items(grand_files);

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
            ignore_broken_pipe(out.write_all(row.as_bytes()))?;
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(status)
}
