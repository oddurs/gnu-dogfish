//! mv — GNU semantics (rename, or copy and remove across file systems, as
//! GNU's copy.c does), recorded to the UndoJournal (`dogfish undo` reverses it)

use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::Result;
use dogfish_core::{DogfishContext, UndoJournal, gnu_io_error, outln};

use crate::common::backup::{backup_mode, backup_suffix};
use crate::common::copy::{Copier, CopyOptions};
use crate::common::getopt::{
    Getopt, HasArg, OptSpec, argmatch, getopt_error, lossy, print_help, print_version, usage_error,
};
use crate::common::mode::process_umask;
use crate::common::path::{
    base_len, dir_names, file_name_concat, last_component, rename_noreplace, strip_trailing_slashes,
};
use crate::common::quote::qa;
use crate::common::targets::{Targets, resolve_targets};

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
    Debug,
    Help,
    Version,
}

/// GNU's `long_options` and `"bfint:uvS:TZ"`.
const SPECS: &[OptSpec<MvOpt>] = &[
    OptSpec::long("backup", HasArg::Optional, MvOpt::BackupArg),
    OptSpec::both(b'Z', "context", HasArg::No, MvOpt::Context),
    OptSpec::long("debug", HasArg::No, MvOpt::Debug),
    OptSpec::both(b'f', "force", HasArg::No, MvOpt::Force),
    OptSpec::both(b'i', "interactive", HasArg::No, MvOpt::Interactive),
    OptSpec::both(b'n', "no-clobber", HasArg::No, MvOpt::NoClobber),
    OptSpec::long("no-copy", HasArg::No, MvOpt::NoCopy),
    OptSpec::both(b'T', "no-target-directory", HasArg::No, MvOpt::NoTargetDir),
    OptSpec::long("strip-trailing-slashes", HasArg::No, MvOpt::StripSlashes),
    OptSpec::both(b'S', "suffix", HasArg::Required, MvOpt::Suffix),
    OptSpec::both(b't', "target-directory", HasArg::Required, MvOpt::TargetDir),
    OptSpec::long("update", HasArg::Optional, MvOpt::Update),
    OptSpec::both(b'v', "verbose", HasArg::No, MvOpt::Verbose),
    OptSpec::long("help", HasArg::No, MvOpt::Help),
    OptSpec::long("version", HasArg::No, MvOpt::Version),
    OptSpec::short(b'b', HasArg::No, MvOpt::Backup),
    OptSpec::short(b'u', HasArg::No, MvOpt::UpdateOlder),
];

pub fn run_mv(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    use MvOpt::*;
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    let mut x = CopyOptions::for_mv(io::stdin().is_terminal());
    let (mut want_backup, mut backup_arg, mut suffix) = (false, None, None);
    let (mut target_dir, mut no_target_dir, mut strip_slashes) = (None, false, false);
    while let Some(opt) = opts.next() {
        let (opt, v) = match opt {
            Ok(o) => o,
            Err(e) => return Ok(getopt_error("mv", &e)),
        };
        let raw = v.clone();
        let v = v.as_deref().map(lossy);
        match opt {
            Help => return Ok(print_help(MV_HELP)),
            Version => return Ok(print_version("mv")),
            // GNU: `--debug` implies `--verbose`.
            Debug => x.verbose = true,
            Backup => want_backup = true,
            BackupArg => {
                want_backup = true;
                backup_arg = v;
            }
            Force => x.interactive = crate::common::copy::Interactive::AlwaysYes,
            Interactive => x.interactive = crate::common::copy::Interactive::AskUser,
            NoClobber => x.interactive = crate::common::copy::Interactive::AlwaysNo,
            NoCopy => x.no_copy = true,
            Context => {}
            StripSlashes => strip_slashes = true,
            Suffix => {
                want_backup = true;
                suffix = v;
            }
            TargetDir => {
                if target_dir.is_some() {
                    eprintln!("mv: multiple target directories specified");
                    return Ok(1);
                }
                target_dir = raw;
            }
            NoTargetDir => no_target_dir = true,
            UpdateOlder => x.update = true,
            Update => {
                const UPDATE: &[(&str, u8)] = &[("all", 0), ("none", 1), ("older", 2)];
                match v {
                    None => x.update = true,
                    // -n takes precedence; otherwise the last --update wins.
                    Some(_) if x.interactive == crate::common::copy::Interactive::AlwaysNo => {}
                    Some(w) => {
                        let which = match argmatch("mv", "--update", &w, UPDATE) {
                            Ok(u) => u,
                            Err(code) => return Ok(code),
                        };
                        x.update = which == 2;
                        x.interactive = if which == 1 {
                            crate::common::copy::Interactive::AlwaysSkip
                        } else {
                            crate::common::copy::Interactive::Unspecified
                        };
                    }
                }
            }
            Verbose => x.verbose = true,
        }
    }

    // With two operands GNU first tries the rename itself, and only then
    // looks at whether DEST is a directory. (Not with `-b -n`, which GNU 9.4
    // rejects only after that rename; Dogfish rejects it without moving.)
    let mut operands = opts.operands();
    let backup_no_clobber =
        want_backup && x.interactive == crate::common::copy::Interactive::AlwaysNo;
    if target_dir.is_none() && !no_target_dir && operands.len() == 2 && !backup_no_clobber {
        x.rename_errno = Some(
            match rename_noreplace(Path::new(&operands[0]), Path::new(&operands[1])) {
                Ok(()) => 0,
                Err(e) => e.raw_os_error().unwrap_or(libc::EIO),
            },
        );
    }
    let targets = if x.rename_errno == Some(0) {
        let dst = operands.pop().unwrap_or_default();
        Targets::Single(operands.pop().unwrap_or_default(), dst)
    } else {
        match resolve_targets("mv", operands, target_dir, no_target_dir) {
            Ok(t) => t,
            Err(code) => return Ok(code),
        }
    };
    let strip = |s: OsString| {
        if strip_slashes {
            strip_trailing_slashes(&s)
        } else {
            s
        }
    };

    if x.interactive == crate::common::copy::Interactive::AlwaysNo {
        x.update = false;
    }
    if backup_no_clobber {
        return Ok(usage_error(
            "mv",
            "options --backup and --no-clobber are mutually exclusive",
        ));
    }
    if want_backup {
        x.backup = match backup_mode("mv", backup_arg.as_deref()) {
            Ok(m) => m,
            Err(code) => return Ok(code),
        };
    }
    x.suffix = backup_suffix(suffix);

    let mut copier = Copier::new(
        "mv",
        x,
        UndoJournal::open_for_recording(),
        process_umask(),
        ctx,
    );
    let mut ok = true;
    match targets {
        Targets::Into(dir, sources) => {
            copier.x.rename_errno = None;
            if sources.len() >= 2 {
                copier.track_destinations();
            }
            let n = sources.len();
            for (i, src) in sources.into_iter().enumerate() {
                copier.x.last_file = i + 1 == n;
                let src = strip(src);
                let last = last_component(src.as_bytes());
                let (dst, _) = file_name_concat(dir.as_bytes(), &last[..base_len(last)]);
                ok &= do_move(&mut copier, Path::new(&src), &dst, ctx);
            }
        }
        Targets::Single(src, dst) => {
            copier.x.last_file = true;
            let (src, dst) = (strip(src), strip(dst));
            ok = do_move(&mut copier, Path::new(&src), Path::new(&dst), ctx);
        }
    }
    Ok(if ok { 0 } else { 1 })
}

/// GNU `do_move`: renames `src` to `dst`, or across file systems copies it
/// and then removes it.
fn do_move(copier: &mut Copier<'_>, src: &Path, dst: &Path, ctx: &DogfishContext) -> bool {
    let out = copier.copy(src, dst, 0);
    let mut ok = out.ok;
    if ok {
        if out.copy_into_self {
            ok = false;
        } else if !out.rename_succeeded {
            // Copied: now remove the source, as `rm -r` would.
            ok = remove_tree(src, copier.x.verbose);
        }
    }
    copier.commit_top(src, Some(src));
    if ok {
        ctx.telemetry.add_items(1);
    }
    ok
}

/// Removes `p` and everything under it, with `rm -r`'s messages (what GNU mv
/// runs after copying across file systems).
fn remove_tree(p: &Path, verbose: bool) -> bool {
    let fail = |e: &io::Error| {
        eprintln!("mv: cannot remove {}: {}", qa(p), gnu_io_error(e));
        false
    };
    let meta = match fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) => return fail(&e),
    };
    if !meta.is_dir() {
        if let Err(e) = fs::remove_file(p) {
            return fail(&e);
        }
        if verbose {
            outln!("removed {}", qa(p));
        }
        return true;
    }
    let names = match dir_names(p, false) {
        Ok(n) => n,
        Err(e) => return fail(&e),
    };
    let mut ok = true;
    for name in names {
        let (child, _) = file_name_concat(p.as_os_str().as_bytes(), name.as_bytes());
        ok &= remove_tree(&child, verbose);
    }
    if !ok {
        return false;
    }
    if let Err(e) = fs::remove_dir(p) {
        return fail(&e);
    }
    if verbose {
        outln!("removed directory {}", qa(p));
    }
    true
}

/// mv's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("mv", MV_HELP, SPECS)
        .choices("backup", crate::common::backup::backup_words())
        .choices("update", ["all", "none", "older"])
}
