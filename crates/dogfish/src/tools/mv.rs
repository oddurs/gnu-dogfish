//! mv — GNU semantics, recorded to the UndoJournal (`dogfish undo` reverses it)

use std::fs;
use std::io::{self, IsTerminal};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use dogfish_core::{DogfishContext, JournalAction, UndoJournal, gnu_io_error, move_path};

use crate::common::backup::{BackupMode, backup_mode, backup_suffix, make_backup};
use crate::common::mode::{CHMOD_MODE_BITS, strmode9};
use crate::common::path::{absolute_lossy, concat_path, operand_base, strip_trailing_slashes};
use crate::common::prompt::{ask, write_protected};
use crate::common::quote::qa;
use crate::common::targets::{
    Clobber, SameFileOpts, Targets, resolve_for_containment, resolve_targets, same_file_ok,
};
use crate::common::tool_args::{HasArg, ToolOpt, argmatch, opt, parse_tool_args, usage_error};

const MV_HELP: &str = "\
Usage: mv [OPTION]... [-T] SOURCE DEST
  or:  mv [OPTION]... SOURCE... DIRECTORY
  or:  mv [OPTION]... -t DIRECTORY SOURCE...
Rename SOURCE to DEST, or move SOURCE(s) to DIRECTORY.
Moves are recorded so that `dogfish undo` can reverse them.

      --backup[=CONTROL]       make a backup of each existing destination file
  -b                           like --backup but does not accept an argument
  -f, --force                  do not prompt before overwriting
  -i, --interactive            prompt before overwrite
  -n, --no-clobber             do not overwrite an existing file
If you specify more than one of -i, -f, -n, only the final one takes effect.
      --strip-trailing-slashes  remove any trailing slashes from each SOURCE
                                 argument
  -S, --suffix=SUFFIX          override the usual backup suffix
  -t, --target-directory=DIRECTORY  move all SOURCE arguments into DIRECTORY
  -T, --no-target-directory    treat DEST as a normal file
      --update[=UPDATE]        control which existing files are updated;
                                 UPDATE={all,none,older(default)}
  -u                           equivalent to --update[=older]
  -v, --verbose                explain what is being done
      --help        display this help and exit
      --version     output version information and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum MvOpt {
    Backup,
    BackupArg,
    Force,
    Interactive,
    NoClobber,
    NoCopy,
    StripSlashes,
    Suffix,
    TargetDir,
    NoTargetDir,
    UpdateOlder,
    Update,
    Verbose,
    Context,
}

struct MvOpts {
    clobber: Clobber,
    update_older: bool,
    backup: BackupMode,
    suffix: String,
    verbose: bool,
    stdin_tty: bool,
}

pub fn run_mv(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    use MvOpt::*;
    const SPECS: &[ToolOpt<MvOpt>] = &[
        opt("b", "", HasArg::No, Backup),
        opt("", "backup", HasArg::Optional, BackupArg),
        opt("f", "force", HasArg::No, Force),
        opt("i", "interactive", HasArg::No, Interactive),
        opt("n", "no-clobber", HasArg::No, NoClobber),
        opt("", "no-copy", HasArg::No, NoCopy),
        opt("", "strip-trailing-slashes", HasArg::No, StripSlashes),
        opt("S", "suffix", HasArg::Required, Suffix),
        opt("t", "target-directory", HasArg::Required, TargetDir),
        opt("T", "no-target-directory", HasArg::No, NoTargetDir),
        opt("u", "", HasArg::No, UpdateOlder),
        opt("", "update", HasArg::Optional, Update),
        opt("v", "verbose", HasArg::No, Verbose),
        opt("Z", "", HasArg::No, Context),
        opt("", "context", HasArg::No, Context),
    ];
    let parsed = match parse_tool_args("mv", &args, SPECS, MV_HELP) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let mut o = MvOpts {
        clobber: Clobber::Yes,
        update_older: false,
        backup: BackupMode::None,
        suffix: String::new(),
        verbose: false,
        stdin_tty: io::stdin().is_terminal(),
    };
    let (mut want_backup, mut backup_arg, mut suffix) = (false, None, None);
    let (mut target_dir, mut no_target_dir, mut strip_slashes) = (None, false, false);
    for (opt, v) in parsed.opts {
        match opt {
            Backup => want_backup = true,
            BackupArg => {
                want_backup = true;
                backup_arg = v;
            }
            Force => o.clobber = Clobber::Yes,
            Interactive => o.clobber = Clobber::Ask,
            NoClobber => o.clobber = Clobber::No,
            NoCopy | Context => {}
            StripSlashes => strip_slashes = true,
            Suffix => {
                want_backup = true;
                suffix = v;
            }
            TargetDir => target_dir = v,
            NoTargetDir => no_target_dir = true,
            UpdateOlder => o.update_older = true,
            Update => {
                const UPDATE: &[(&str, u8)] = &[("all", 0), ("none", 1), ("older", 2)];
                let which = match v {
                    None => 2,
                    Some(w) => match argmatch("mv", "--update", &w, UPDATE) {
                        Ok(u) => u,
                        Err(code) => return Ok(code),
                    },
                };
                o.update_older = which == 2;
                if which == 1 && o.clobber != Clobber::No {
                    o.clobber = Clobber::Skip;
                }
            }
            Verbose => o.verbose = true,
        }
    }
    if want_backup {
        o.backup = match backup_mode("mv", backup_arg.as_deref()) {
            Ok(m) => m,
            Err(code) => return Ok(code),
        };
    }
    o.suffix = backup_suffix(suffix);

    let targets = match resolve_targets("mv", parsed.operands, target_dir, no_target_dir) {
        Ok(t) => t,
        Err(code) => return Ok(code),
    };
    if want_backup && o.clobber == Clobber::No {
        return Ok(usage_error(
            "mv",
            "options --backup and --no-clobber are mutually exclusive",
        ));
    }
    let journal = UndoJournal::open_for_recording();
    let strip = |s: &str| {
        if strip_slashes {
            strip_trailing_slashes(s)
        } else {
            s.to_string()
        }
    };
    let pairs: Vec<(String, PathBuf)> = match targets {
        Targets::Into(dir, sources) => sources
            .iter()
            .map(|s| {
                let s = strip(s);
                let d = concat_path(&dir, &operand_base(&s));
                (s, d)
            })
            .collect(),
        Targets::Single(s, d) => vec![(strip(&s), PathBuf::from(d))],
    };
    let mut ok = true;
    for (src, dst) in &pairs {
        ok &= mv_one(Path::new(src), dst, &o, journal.as_ref(), ctx);
    }
    Ok(if ok { 0 } else { 1 })
}

fn mv_one(
    src: &Path,
    dst: &Path,
    o: &MvOpts,
    journal: Option<&UndoJournal>,
    ctx: &DogfishContext,
) -> bool {
    let src_meta = match src.symlink_metadata() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("mv: cannot stat {}: {}", qa(src), gnu_io_error(&e));
            return false;
        }
    };
    let mut backup_made = None;
    let dst_meta = match dst.symlink_metadata() {
        Ok(m) => Some(m),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            eprintln!("mv: cannot stat {}: {}", qa(dst), gnu_io_error(&e));
            return false;
        }
    };
    // GNU's order: the no-clobber and update checks come first, then same-file
    // and prompts; type conflicts only matter when no backup is made.
    if let Some(dm) = &dst_meta {
        match o.clobber {
            Clobber::No => {
                eprintln!("mv: not replacing {}", qa(dst));
                return false;
            }
            Clobber::Skip => return true,
            _ => {}
        }
        let x = SameFileOpts {
            move_mode: true,
            deref_never: true,
            backup: o.backup != BackupMode::None,
            ..SameFileOpts::default()
        };
        if !same_file_ok(src, &src_meta, dst, dm, &x).0 {
            eprintln!("mv: {} and {} are the same file", qa(src), qa(dst));
            return false;
        }
        // -u only ever skips non-directories.
        if o.update_older
            && !src_meta.is_dir()
            && (dm.mtime(), dm.mtime_nsec()) >= (src_meta.mtime(), src_meta.mtime_nsec())
        {
            return true;
        }
        let question = match o.clobber {
            Clobber::Ask => Some(format!("overwrite {}? ", qa(dst))),
            _ if o.stdin_tty && !dm.is_symlink() && write_protected(dst) => {
                let m = dm.mode() & CHMOD_MODE_BITS;
                Some(format!(
                    "replace {}, overriding mode {m:04o} ({})? ",
                    qa(dst),
                    strmode9(m)
                ))
            }
            _ => None,
        };
        if let Some(q) = question
            && !ask("mv", &q)
        {
            return false;
        }
        if o.backup == BackupMode::None {
            if src_meta.is_dir() && !dm.is_dir() {
                eprintln!(
                    "mv: cannot overwrite non-directory {} with directory {}",
                    qa(dst),
                    qa(src)
                );
                return false;
            }
            if !src_meta.is_dir() && dm.is_dir() {
                eprintln!(
                    "mv: cannot overwrite directory {} with non-directory",
                    qa(dst)
                );
                return false;
            }
        }
    }
    if src_meta.is_dir()
        && let Ok(abs_src) = fs::canonicalize(src)
        && resolve_for_containment(dst).starts_with(&abs_src)
    {
        eprintln!(
            "mv: cannot move {} to a subdirectory of itself, {}",
            qa(src),
            qa(dst)
        );
        return false;
    }
    if let Some(dm) = &dst_meta {
        if o.backup != BackupMode::None {
            match make_backup(dst, o.backup, &o.suffix, journal, "mv") {
                Ok(b) => backup_made = Some(b),
                Err(e) => {
                    eprintln!("mv: cannot backup {}: {}", qa(dst), gnu_io_error(&e));
                    return false;
                }
            }
        } else if dm.is_dir() && fs::read_dir(dst).is_ok_and(|mut d| d.next().is_some()) {
            // Checked up front: the journal moves the old target aside before the
            // rename, which must never make a non-empty directory replaceable.
            eprintln!("mv: cannot overwrite {}: Directory not empty", qa(dst));
            return false;
        }
    }

    let abs_src = absolute_lossy(src);
    // Move an existing target aside first so `dogfish undo` can restore it.
    let saved = match journal {
        Some(j) => match j.backup_existing(dst, false) {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "mv: cannot back up {} before overwriting: {}",
                    qa(dst),
                    gnu_io_error(&e)
                );
                return false;
            }
        },
        None => None,
    };
    if let Err(e) = move_path(src, dst) {
        if let Some(b) = &saved {
            let _ = move_path(Path::new(b), dst);
        }
        eprintln!(
            "mv: cannot move {} to {}: {}",
            qa(src),
            qa(dst),
            gnu_io_error(&e)
        );
        return false;
    }
    if let Some(j) = journal {
        let _ = j.record(
            "mv",
            JournalAction::Move {
                src: abs_src,
                dst: absolute_lossy(dst),
                backup_path: saved,
            },
        );
    }
    ctx.telemetry.add_items(1);
    if o.verbose {
        match &backup_made {
            Some(b) => println!("renamed {} -> {} (backup: {})", qa(src), qa(dst), qa(b)),
            None => println!("renamed {} -> {}", qa(src), qa(dst)),
        }
    }
    true
}
