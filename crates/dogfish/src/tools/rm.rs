//! rm — GNU semantics plus `--safe` (or `--trash`) undoable trash staging

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs;
use std::io::{self, IsTerminal};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::Result;
use dogfish_core::{DogfishContext, UndoJournal, gnu_io_error, outln};

use crate::common::dirfd::{At, EntryType, MAX_OPEN_DIRS, Names, WalkPath};
use crate::common::file_info::{describe_mode, file_type_description};
use crate::common::getopt::{
    Getopt, HasArg, OptSpec, argmatch, print_help, print_version, try_help, usage_error,
};
use crate::common::path::last_component;
use crate::common::preserve_root::{is_root_dir, refuse_root};
use crate::common::prompt::{Interactive, ask};
use crate::common::quote::{Quoting, qa, quote_bytes};

const RM_HELP: &str = "\
Usage: rm [OPTION]... [FILE]...
Remove (unlink) the FILE(s).

  -f, --force           ignore nonexistent files and arguments, never prompt
  -i                    prompt before every removal
  -I                    prompt once before removing more than three files, or
                          when removing recursively
      --interactive[=WHEN]  prompt according to WHEN: never, once (-I), or
                          always (-i); without WHEN, prompt always
      --one-file-system  when removing a hierarchy recursively, skip any
                          directory that is on a file system different from
                          that of the corresponding command line argument
      --no-preserve-root  do not treat '/' specially
      --preserve-root[=all]  do not remove '/' (default);
                              with 'all', reject any command line argument
                              on a separate device from its parent
  -r, -R, --recursive   remove directories and their contents recursively
  -d, --dir             remove empty directories
  -v, --verbose         explain what is being done
      --safe, --trash   move files to Dogfish's trash instead (undo with
                          `dogfish undo`)
      --help        display this help and exit
      --version     output version information and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum RmOpt {
    Dir,
    Force,
    EveryFile,
    Once,
    InteractiveWhen,
    OneFileSystem,
    NoPreserveRoot,
    PreserveRoot,
    PresumeInputTty,
    Recursive,
    Verbose,
    Safe,
    Help,
    Version,
}

/// GNU's `long_opts` (in its order: it decides ambiguity diagnostics) and
/// `"dfirvIR"`, plus the Dogfish `--safe`/`--trash`. `---presume-input-tty`
/// is GNU's hidden test option: prompt as if standard input were a terminal.
const SPECS: &[OptSpec<RmOpt>] = &[
    OptSpec::both(b'f', "force", HasArg::No, RmOpt::Force),
    OptSpec::long("interactive", HasArg::Optional, RmOpt::InteractiveWhen),
    OptSpec::long("one-file-system", HasArg::No, RmOpt::OneFileSystem),
    OptSpec::long("no-preserve-root", HasArg::No, RmOpt::NoPreserveRoot),
    OptSpec::long("preserve-root", HasArg::Optional, RmOpt::PreserveRoot),
    OptSpec::long("-presume-input-tty", HasArg::No, RmOpt::PresumeInputTty),
    OptSpec::both(b'r', "recursive", HasArg::No, RmOpt::Recursive),
    OptSpec::both(b'd', "dir", HasArg::No, RmOpt::Dir),
    OptSpec::both(b'v', "verbose", HasArg::No, RmOpt::Verbose),
    OptSpec::long("help", HasArg::No, RmOpt::Help),
    OptSpec::long("version", HasArg::No, RmOpt::Version),
    OptSpec::long("safe", HasArg::No, RmOpt::Safe).exact(),
    OptSpec::long("trash", HasArg::No, RmOpt::Safe).exact(),
    OptSpec::short(b'i', HasArg::No, RmOpt::EveryFile),
    OptSpec::short(b'I', HasArg::No, RmOpt::Once),
    OptSpec::short(b'R', HasArg::No, RmOpt::Recursive),
];

struct RmRun {
    force: bool,
    interactive: Interactive,
    recursive: bool,
    dir: bool,
    verbose: bool,
    one_file_system: bool,
    stdin_tty: bool,
}

/// Outcome of removing one file or tree.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RmStatus {
    Removed,
    Declined,
    /// Declined to descend: the directory stays, so its ancestors are left alone.
    Kept,
    Failed,
}

/// GNU's `directory_status`: whether a directory has entries, or why it
/// cannot be read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DirStatus {
    Empty,
    NonEmpty,
    /// `errno` of the failed opendir/readdir.
    Unreadable(i32),
}

fn dir_status(at: At<'_>) -> DirStatus {
    let mut names = Names::default();
    match at.open_dir(false).and_then(|d| d.read_names(&mut names)) {
        Ok(()) if names.is_empty() => DirStatus::Empty,
        Ok(()) => DirStatus::NonEmpty,
        Err(e) => DirStatus::Unreadable(e.raw_os_error().unwrap_or(libc::EIO)),
    }
}

/// [`dir_status`] of a path.
fn dir_status_of(p: &Path) -> DirStatus {
    match CString::new(p.as_os_str().as_bytes()) {
        Ok(c) => dir_status(At::cwd(&c)),
        Err(_) => DirStatus::Unreadable(libc::EINVAL),
    }
}

/// True when the last component of `f` is `.` or `..` (`d/.`, `./`, `..//`).
fn is_dot_or_dotdot(f: &OsStr) -> bool {
    let last = last_component(f.as_bytes());
    let name = &last[..last.iter().position(|&b| b == b'/').unwrap_or(last.len())];
    name == b"." || name == b".."
}

/// After a usage error, GNU's hint for removing a file whose name looks like
/// an option: `Try 'rm ./-f' to remove the file '-f'.`
fn diagnose_leading_hyphen(args: &[OsString]) {
    for arg in args {
        let b = arg.as_bytes();
        if b.len() > 1 && b[0] == b'-' && Path::new(arg).symlink_metadata().is_ok() {
            eprintln!(
                "Try 'rm ./{}' to remove the file {}.",
                quote_bytes(b, Quoting::ShellEscape),
                qa(arg)
            );
            break;
        }
    }
}

pub fn run_rm(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    use RmOpt::*;
    let args: Vec<OsString> = args.into_iter().skip(1).collect();
    let mut opts = Getopt::new(args.clone(), SPECS);
    let mut run = RmRun {
        force: false,
        interactive: Interactive::Sometimes,
        recursive: false,
        dir: false,
        verbose: false,
        one_file_system: false,
        stdin_tty: io::stdin().is_terminal(),
    };
    let (mut prompt_once, mut preserve_root, mut preserve_all, mut safe) =
        (false, true, false, false);
    while let Some(opt) = opts.next() {
        let (o, v) = match opt {
            Ok(o) => o,
            Err(e) => {
                crate::common::getopt::error_bytes("rm", e.as_bytes());
                diagnose_leading_hyphen(&args);
                return Ok(try_help("rm"));
            }
        };
        match o {
            Dir => run.dir = true,
            Force => {
                run.force = true;
                run.interactive = Interactive::Never;
                prompt_once = false;
            }
            EveryFile => {
                run.force = false;
                run.interactive = Interactive::Always;
                prompt_once = false;
            }
            Once => {
                run.force = false;
                run.interactive = Interactive::Sometimes;
                prompt_once = true;
            }
            InteractiveWhen => {
                #[derive(Clone, Copy, PartialEq)]
                enum When {
                    Never,
                    Once,
                    Always,
                }
                const WHEN: &[(&str, When)] = &[
                    ("never", When::Never),
                    ("no", When::Never),
                    ("none", When::Never),
                    ("once", When::Once),
                    ("always", When::Always),
                    ("yes", When::Always),
                ];
                let when = match v {
                    Some(w) => match argmatch("rm", "--interactive", w, WHEN) {
                        Ok(w) => w,
                        Err(code) => return Ok(code),
                    },
                    None => When::Always,
                };
                match when {
                    When::Never => {
                        run.interactive = Interactive::Never;
                        prompt_once = false;
                    }
                    When::Once => {
                        run.force = false;
                        run.interactive = Interactive::Sometimes;
                        prompt_once = true;
                    }
                    When::Always => {
                        run.force = false;
                        run.interactive = Interactive::Always;
                        prompt_once = false;
                    }
                }
            }
            OneFileSystem => run.one_file_system = true,
            NoPreserveRoot => {
                if opts.last_word() != Some(OsStr::new("--no-preserve-root")) {
                    eprintln!("rm: you may not abbreviate the --no-preserve-root option");
                    return Ok(1);
                }
                preserve_root = false;
            }
            PreserveRoot => {
                match v.as_deref().map(OsStr::as_bytes) {
                    None => {}
                    Some(b"all") => preserve_all = true,
                    Some(other) => {
                        eprintln!(
                            "rm: unrecognized --preserve-root argument: {}",
                            qa(OsStr::from_bytes(other))
                        );
                        return Ok(1);
                    }
                }
                preserve_root = true;
            }
            PresumeInputTty => run.stdin_tty = true,
            Recursive => run.recursive = true,
            Verbose => run.verbose = true,
            Safe => safe = true,
            Help => return Ok(print_help(RM_HELP)),
            Version => return Ok(print_version("rm")),
        }
    }
    let files = opts.operands();
    if files.is_empty() {
        return Ok(if run.force {
            0
        } else {
            usage_error("rm", "missing operand")
        });
    }
    if prompt_once && (files.len() > 3 || run.recursive) {
        let n = files.len();
        let question = format!(
            "remove {n} argument{}{}? ",
            if n == 1 { "" } else { "s" },
            if run.recursive { " recursively" } else { "" }
        );
        if !ask("rm", &question) {
            return Ok(0);
        }
    }

    let journal = if safe {
        Some(UndoJournal::open()?)
    } else {
        None
    };
    ctx.telemetry.set_strategy(if safe {
        "journaled trash staging (undoable via `dogfish undo`)"
    } else {
        "direct unlink"
    });

    let mut exit_code = 0;
    for f in &files {
        let p = Path::new(f);
        let meta = match p.symlink_metadata() {
            Ok(m) => m,
            Err(e) => {
                let missing = matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR));
                if !(run.force && missing) {
                    eprintln!("rm: cannot remove {}: {}", qa(f), gnu_io_error(&e));
                    exit_code = 1;
                }
                continue;
            }
        };
        // GNU checks, in this order: directory without -r, `.`/`..`, then `/`.
        // Like GNU, `-d` goes on to rmdir(2) unless the directory has entries
        // (an unreadable one may still be empty).
        if meta.is_dir() && !run.recursive && (!run.dir || dir_status_of(p) == DirStatus::NonEmpty)
        {
            let why = if run.dir {
                "Directory not empty"
            } else {
                "Is a directory"
            };
            eprintln!("rm: cannot remove {}: {why}", qa(f));
            exit_code = 1;
            continue;
        }
        if is_dot_or_dotdot(f) {
            eprintln!(
                "rm: refusing to remove '.' or '..' directory: skipping {}",
                qa(f)
            );
            exit_code = 1;
            continue;
        }
        if run.recursive && preserve_root && is_root_dir(p) {
            refuse_root("rm", f);
            exit_code = 1;
            continue;
        }
        if preserve_all
            && meta.is_dir()
            && let Ok(parent) = fs::metadata(p.join(".."))
            && parent.dev() != meta.dev()
        {
            eprintln!("rm: skipping {}, since it's on a different device", qa(f));
            eprintln!("rm: and --preserve-root=all is in effect");
            exit_code = 1;
            continue;
        }

        if let Some(j) = &journal {
            // `--safe` refuses exactly what plain `rm` refuses, then moves the whole
            // operand to the trash in one undoable step.
            if run.interactive == Interactive::Always
                && !ask(
                    "rm",
                    &format!("remove {} {}? ", file_type_description(&meta), qa(f)),
                )
            {
                continue;
            }
            match j.stage_to_trash(p) {
                Ok(entry) => {
                    ctx.telemetry.add_items(1);
                    if run.verbose {
                        let st = ctx.style();
                        outln!(
                            "{} {} (undo id #{}: run `{}` to restore)",
                            st.yellow("trashed"),
                            qa(f),
                            entry.id,
                            st.bold_cyan("dogfish undo")
                        );
                    }
                }
                Err(e) => {
                    eprintln!(
                        "rm: failed to stage {} to trash: {}",
                        qa(f),
                        gnu_io_error(&e)
                    );
                    exit_code = 1;
                }
            }
            continue;
        }

        let Ok(c_operand) = CString::new(f.as_bytes()) else {
            continue;
        };
        let at = At::cwd(&c_operand);
        let info = Info::from_metadata(&meta);
        let mut path = WalkPath::new(f.as_bytes());
        let status = if meta.is_dir() {
            if run.recursive {
                rm_tree(at, &mut path, Some(&info), 0, meta.dev(), &run, ctx)
            } else {
                rm_empty_dir(at, path.as_path(), Some(&info), None, &run, ctx)
            }
        } else {
            rm_file(at, path.as_path(), Some(&info), &run, ctx)
        };
        if status == RmStatus::Failed {
            exit_code = 1;
        }
    }
    Ok(exit_code)
}

/// What a prompt or `--one-file-system` needs to know about an entry.
#[derive(Clone, Copy)]
struct Info {
    mode: u32,
    size: u64,
    dev: u64,
}

impl Info {
    fn from_metadata(meta: &fs::Metadata) -> Self {
        Self {
            mode: meta.mode(),
            size: meta.len(),
            dev: meta.dev(),
        }
    }

    #[allow(clippy::unnecessary_cast)] // the field types vary by platform
    fn from_stat(st: &libc::stat) -> Self {
        Self {
            mode: st.st_mode as u32,
            size: st.st_size as u64,
            dev: st.st_dev as u64,
        }
    }

    fn kind(&self) -> EntryType {
        EntryType::from_mode(self.mode)
    }
}

impl RmRun {
    /// Whether removing an entry may prompt, so it must be `lstat`ed first:
    /// always with `-i`, and for write-protected files when stdin is a
    /// terminal (unless `-f`). Otherwise `readdir`'s file type suffices.
    fn may_prompt(&self) -> bool {
        match self.interactive {
            Interactive::Never => false,
            Interactive::Sometimes => self.stdin_tty,
            Interactive::Always => true,
        }
    }
}

/// Whether to ask before removing `at`: always with `-i`, and for write-protected
/// files when stdin is a terminal (unless `-f`). Returns the write-protected flag.
/// `info` is known whenever [`RmRun::may_prompt`].
fn rm_prompt_needed(at: At<'_>, info: Option<&Info>, run: &RmRun) -> Option<bool> {
    let info = info.filter(|_| run.may_prompt())?;
    let protected = info.kind() != EntryType::Symlink && at.write_protected();
    (run.interactive == Interactive::Always || protected).then_some(protected)
}

fn protected_prefix(protected: bool) -> &'static str {
    if protected { "write-protected " } else { "" }
}

fn rm_file(
    at: At<'_>,
    p: &Path,
    info: Option<&Info>,
    run: &RmRun,
    ctx: &DogfishContext,
) -> RmStatus {
    if let Some(info) = info
        && let Some(protected) = rm_prompt_needed(at, Some(info), run)
    {
        let q = format!(
            "remove {}{} {}? ",
            protected_prefix(protected),
            describe_mode(info.mode, info.size),
            qa(p)
        );
        if !ask("rm", &q) {
            return RmStatus::Declined;
        }
    }
    match at.unlink(false) {
        Ok(()) => {
            ctx.telemetry.add_items(1);
            if run.verbose {
                outln!("removed {}", qa(p));
            }
            RmStatus::Removed
        }
        Err(e) => {
            eprintln!("rm: cannot remove {}: {}", qa(p), gnu_io_error(&e));
            RmStatus::Failed
        }
    }
}

/// Removes the directory `at` (printed as `p`). `known` is its
/// [`DirStatus`] if the caller has it; otherwise it is found only when a
/// prompt or a failed `rmdir` needs it.
fn rm_empty_dir(
    at: At<'_>,
    p: &Path,
    info: Option<&Info>,
    known: Option<DirStatus>,
    run: &RmRun,
    ctx: &DogfishContext,
) -> RmStatus {
    let mut known = known;
    let mut status = || *known.get_or_insert_with(|| dir_status(at));
    if let Some(protected) = rm_prompt_needed(at, info, run) {
        let question = match status() {
            // GNU asks before trying to remove a directory it cannot read
            // (with -d, when it may be empty); any other error is final.
            DirStatus::Unreadable(libc::EACCES) if run.dir => {
                format!("attempt removal of inaccessible directory {}? ", qa(p))
            }
            DirStatus::Unreadable(errno) => {
                let e = io::Error::from_raw_os_error(errno);
                eprintln!("rm: cannot remove {}: {}", qa(p), gnu_io_error(&e));
                return RmStatus::Failed;
            }
            _ => format!(
                "remove {}directory {}? ",
                protected_prefix(protected),
                qa(p)
            ),
        };
        if !ask("rm", &question) {
            return RmStatus::Declined;
        }
    }
    match at.unlink(true) {
        Ok(()) => {
            ctx.telemetry.add_items(1);
            if run.verbose {
                outln!("removed directory {}", qa(p));
            }
            RmStatus::Removed
        }
        // `-f` ignores a directory that vanished (or a `link/` that is no directory).
        Err(e) if run.force && matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => {
            RmStatus::Removed
        }
        Err(e) => {
            // A directory that could not be read and is not empty: GNU names
            // the read error.
            let e = match e.raw_os_error() {
                Some(libc::ENOTEMPTY | libc::EEXIST) => match status() {
                    DirStatus::Unreadable(errno) => io::Error::from_raw_os_error(errno),
                    _ => e,
                },
                _ => e,
            };
            eprintln!("rm: cannot remove {}: {}", qa(p), gnu_io_error(&e));
            RmStatus::Failed
        }
    }
}

/// Removes a directory tree depth-first in `readdir` order, like GNU's `fts`
/// traversal. Symbolic links are removed, never followed. After an error inside
/// a directory, its ancestors are left alone (GNU reports only the first error).
///
/// Entries are removed relative to their open directory, and `lstat`ed only
/// when a prompt or `--one-file-system` needs more than `readdir`'s type.
/// `path` is the printed name of `at` and is restored before returning.
fn rm_tree(
    at: At<'_>,
    path: &mut WalkPath,
    info: Option<&Info>,
    depth: usize,
    top_dev: u64,
    run: &RmRun,
    ctx: &DogfishContext,
) -> RmStatus {
    let mut names = Names::default();
    let dir = at
        .open_dir(true)
        .and_then(|d| d.read_names(&mut names).map(|()| d));
    let dir = match dir {
        // An unreadable directory cannot be descended into; GNU (`FTS_DNR`)
        // goes straight to removing it, which works if it is empty.
        Err(e) => {
            let errno = e.raw_os_error().unwrap_or(libc::EIO);
            if rm_prompt_needed(at, info, run).is_some() && !(run.dir && errno == libc::EACCES) {
                eprintln!(
                    "rm: cannot remove {}: {}",
                    qa(path.as_path()),
                    gnu_io_error(&e)
                );
                return RmStatus::Failed;
            }
            let status = Some(DirStatus::Unreadable(errno));
            return rm_empty_dir(at, path.as_path(), info, status, run, ctx);
        }
        Ok(d) => d,
    };
    if !names.is_empty() {
        if let Some(protected) = rm_prompt_needed(at, info, run)
            && !ask(
                "rm",
                &format!(
                    "descend into {}directory {}? ",
                    protected_prefix(protected),
                    qa(path.as_path())
                ),
            )
        {
            return RmStatus::Kept;
        }
        // Deep down, entries are reached by full path, with no directory held open.
        let dir = (depth < MAX_OPEN_DIRS).then_some(dir);
        let need_info = run.may_prompt() || run.one_file_system;
        let (mut failed, mut kept) = (false, false);
        for i in 0..names.len() {
            let (name, kind, _) = names.get(i);
            let mark = path.push(name.to_bytes());
            let full;
            let child = match &dir {
                Some(d) => At { dir: d.fd(), name },
                None => {
                    full = path.c_path().map(CStr::to_owned);
                    match &full {
                        Some(c) => At::cwd(c),
                        // A path with a NUL cannot be reached; EBADF says so.
                        None => At { dir: -1, name },
                    }
                }
            };
            let cinfo = if need_info || kind == EntryType::Unknown {
                match child.stat(false) {
                    Ok(st) => Some(Info::from_stat(&st)),
                    Err(e) => {
                        eprintln!(
                            "rm: cannot remove {}: {}",
                            qa(path.as_path()),
                            gnu_io_error(&e)
                        );
                        failed = true;
                        path.truncate(mark);
                        continue;
                    }
                }
            } else {
                None
            };
            let is_dir = cinfo.map_or(kind, |i| i.kind()) == EntryType::Dir;
            let status = if is_dir {
                if run.one_file_system && cinfo.is_some_and(|i| i.dev != top_dev) {
                    eprintln!(
                        "rm: skipping {}, since it's on a different device",
                        qa(path.as_path())
                    );
                    RmStatus::Failed
                } else {
                    rm_tree(child, path, cinfo.as_ref(), depth + 1, top_dev, run, ctx)
                }
            } else {
                rm_file(child, path.as_path(), cinfo.as_ref(), run, ctx)
            };
            path.truncate(mark);
            failed |= status == RmStatus::Failed;
            kept |= status == RmStatus::Kept;
        }
        if failed {
            return RmStatus::Failed;
        }
        if kept {
            return RmStatus::Kept;
        }
    }
    let status = names.is_empty().then_some(DirStatus::Empty);
    rm_empty_dir(at, path.as_path(), info, status, run, ctx)
}
