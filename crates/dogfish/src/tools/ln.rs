//! `ln`

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::Path;

use anyhow::Result;
use dogfish_core::{DogfishContext, gnu_io_error, same_file};

use crate::common::backup::{BackupMode, backup_mode, backup_name, backup_suffix};
use crate::common::canonicalize::{CanMode, canonicalize_mode, relpath};
use crate::common::path::{concat_path, operand_base, same_inode, same_name};
use crate::common::prompt::ask;
use crate::common::quote::{qa, qf};
use crate::common::tool_args::{HasArg, ToolOpt, opt, parse_tool_args, usage_error};

const LN_HELP: &str = "\
Usage: ln [OPTION]... [-T] TARGET LINK_NAME
  or:  ln [OPTION]... TARGET
  or:  ln [OPTION]... TARGET... DIRECTORY
  or:  ln [OPTION]... -t DIRECTORY TARGET...
Create links to each TARGET; hard links by default, symbolic links with -s.

      --backup[=CONTROL]      make a backup of each existing destination file
  -b                          like --backup but does not accept an argument
  -d, -F, --directory         (hard links to directories are not allowed)
  -f, --force                 remove existing destination files
  -i, --interactive           prompt whether to remove destinations
  -L, --logical               dereference TARGETs that are symbolic links
  -n, --no-dereference        treat LINK_NAME as a normal file if
                                it is a symbolic link to a directory
  -P, --physical              make hard links directly to symbolic links
  -r, --relative              with -s, create links relative to link location
  -s, --symbolic              make symbolic links instead of hard links
  -S, --suffix=SUFFIX         override the usual backup suffix
  -t, --target-directory=DIRECTORY  specify the DIRECTORY in which to create
                                the links
  -T, --no-target-directory   treat LINK_NAME as a normal file always
  -v, --verbose               print name of each linked file
      --help        display this help and exit
      --version     output version information and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum LnOpt {
    Backup,
    BackupArg,
    Directory,
    Force,
    Interactive,
    Logical,
    NoDeref,
    Physical,
    Relative,
    Symbolic,
    Suffix,
    TargetDir,
    NoTargetDir,
    Verbose,
}

struct LnOpts {
    backup: BackupMode,
    suffix: String,
    force: bool,
    interactive: bool,
    logical: bool,
    relative: bool,
    symbolic: bool,
    verbose: bool,
}

pub fn run_ln(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    use LnOpt::*;
    const SPECS: &[ToolOpt<LnOpt>] = &[
        opt("b", "", HasArg::No, Backup),
        opt("", "backup", HasArg::Optional, BackupArg),
        opt("dF", "directory", HasArg::No, Directory),
        opt("f", "force", HasArg::No, Force),
        opt("i", "interactive", HasArg::No, Interactive),
        opt("L", "logical", HasArg::No, Logical),
        opt("n", "no-dereference", HasArg::No, NoDeref),
        opt("P", "physical", HasArg::No, Physical),
        opt("r", "relative", HasArg::No, Relative),
        opt("s", "symbolic", HasArg::No, Symbolic),
        opt("S", "suffix", HasArg::Required, Suffix),
        opt("t", "target-directory", HasArg::Required, TargetDir),
        opt("T", "no-target-directory", HasArg::No, NoTargetDir),
        opt("v", "verbose", HasArg::No, Verbose),
    ];
    let parsed = match parse_tool_args("ln", &args, SPECS, LN_HELP) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let mut o = LnOpts {
        backup: BackupMode::None,
        suffix: String::new(),
        force: false,
        interactive: false,
        logical: false,
        relative: false,
        symbolic: false,
        verbose: false,
    };
    let (mut want_backup, mut backup_arg, mut suffix) = (false, None, None);
    let (mut no_deref, mut target_dir, mut no_target_dir) = (false, None, false);
    for (opt, v) in parsed.opts {
        match opt {
            Backup => want_backup = true,
            BackupArg => {
                want_backup = true;
                backup_arg = v;
            }
            Directory => {}
            Force => {
                o.force = true;
                o.interactive = false;
            }
            Interactive => {
                o.force = false;
                o.interactive = true;
            }
            Logical => o.logical = true,
            Physical => o.logical = false,
            NoDeref => no_deref = true,
            Relative => o.relative = true,
            Symbolic => o.symbolic = true,
            Suffix => {
                want_backup = true;
                suffix = v;
            }
            TargetDir => target_dir = v,
            NoTargetDir => no_target_dir = true,
            Verbose => o.verbose = true,
        }
    }
    if want_backup {
        o.backup = match backup_mode("ln", backup_arg.as_deref()) {
            Ok(m) => m,
            Err(code) => return Ok(code),
        };
    }
    o.suffix = backup_suffix(suffix);
    if o.relative && !o.symbolic {
        eprintln!("ln: cannot do --relative without --symbolic");
        return Ok(1);
    }

    let mut files = parsed.operands;
    let is_dir = |p: &str| {
        let m = if no_deref {
            fs::symlink_metadata(p)
        } else {
            fs::metadata(p)
        };
        m.is_ok_and(|m| m.is_dir())
    };
    let target_dir = match (target_dir, no_target_dir) {
        (Some(_), true) => {
            eprintln!("ln: cannot combine --target-directory and --no-target-directory");
            return Ok(1);
        }
        (Some(d), false) => {
            if let Err(e) = fs::metadata(&d).and_then(|m| {
                if m.is_dir() {
                    Ok(())
                } else {
                    Err(io::Error::from_raw_os_error(libc::ENOTDIR))
                }
            }) {
                eprintln!("ln: target directory {}: {}", qa(&d), gnu_io_error(&e));
                return Ok(1);
            }
            if files.is_empty() {
                return Ok(usage_error("ln", "missing file operand"));
            }
            Some(d)
        }
        (None, true) => {
            match files.len() {
                0 => return Ok(usage_error("ln", "missing file operand")),
                1 => {
                    return Ok(usage_error(
                        "ln",
                        &format!("missing destination file operand after {}", qa(&files[0])),
                    ));
                }
                2 => {}
                _ => {
                    return Ok(usage_error(
                        "ln",
                        &format!("extra operand {}", qa(&files[2])),
                    ));
                }
            }
            None
        }
        (None, false) => match files.len() {
            0 => return Ok(usage_error("ln", "missing file operand")),
            1 => Some(".".to_string()),
            n if is_dir(&files[n - 1]) => files.pop(),
            2 => None,
            n => {
                let last = &files[n - 1];
                let meta = if no_deref {
                    fs::symlink_metadata(last)
                } else {
                    fs::metadata(last)
                };
                let err = meta
                    .err()
                    .unwrap_or_else(|| io::Error::from_raw_os_error(libc::ENOTDIR));
                eprintln!("ln: target {}: {}", qa(last), gnu_io_error(&err));
                return Ok(1);
            }
        },
    };

    let mut status = 0;
    match target_dir {
        Some(dir) => {
            for src in &files {
                let dest = concat_path(&dir, &operand_base(src));
                if !ln_one(src, &dest, &o, ctx) {
                    status = 1;
                }
            }
        }
        None => {
            if !ln_one(&files[0], Path::new(&files[1]), &o, ctx) {
                status = 1;
            }
        }
    }
    Ok(status)
}

fn ln_one(src: &str, dest: &Path, o: &LnOpts, ctx: &DogfishContext) -> bool {
    let src_path = Path::new(src);
    let src_meta = if o.symbolic {
        None
    } else {
        let m = if o.logical {
            fs::metadata(src_path)
        } else {
            fs::symlink_metadata(src_path)
        };
        match m {
            Err(e) => {
                eprintln!("ln: failed to access {}: {}", qa(src), gnu_io_error(&e));
                return false;
            }
            Ok(m) if m.is_dir() => {
                eprintln!("ln: {}: hard link not allowed for directory", qf(src));
                return false;
            }
            Ok(m) => Some(m),
        }
    };
    // With -L, resolve the source before DEST (which may be that symlink) is removed.
    let real_src = if o.logical && !o.symbolic {
        match fs::canonicalize(src_path) {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("ln: failed to access {}: {}", qa(src), gnu_io_error(&e));
                return false;
            }
        }
    } else {
        None
    };
    let target: OsString = if o.relative {
        relative_link_target(src_path, dest)
    } else {
        OsString::from(src)
    };

    let mut backup = None;
    if (o.force || o.interactive || o.backup != BackupMode::None)
        && let Ok(dm) = fs::symlink_metadata(dest)
    {
        if dm.is_dir() {
            eprintln!("ln: {}: cannot overwrite directory", qf(dest));
            return false;
        }
        // Replacing DEST must not delete the file SOURCE names.
        let same = (o.force || o.interactive || (o.backup != BackupMode::None && !o.symbolic))
            && if o.symbolic {
                fs::metadata(src_path).ok()
            } else {
                src_meta.clone()
            }
            .is_some_and(|sm| {
                same_inode(&sm, &dm) && (sm.nlink() == 1 || same_name(src_path, dest))
            });
        // Nor may `ln -sf` turn a file into a link to itself.
        let self_link = o.symbolic && o.force && !dm.is_symlink() && {
            let dir = dest.parent().unwrap_or(Path::new(""));
            same_file(&dir.join(&target), dest)
        };
        let fatal_same = same && (o.force || (o.backup != BackupMode::None && !o.symbolic));
        if same && !fatal_same {
            // `ln -i` replacing a file with itself is a no-op (GNU renames a fresh
            // link over it); never remove it.
            return ask("ln", &format!("replace {}? ", qa(dest)));
        }
        if fatal_same || self_link {
            eprintln!("ln: {} and {} are the same file", qa(src), qa(dest));
            return false;
        }
        if o.interactive && !ask("ln", &format!("replace {}? ", qa(dest))) {
            return false;
        }
        let remove = o.force || o.interactive;
        if o.backup != BackupMode::None {
            let name = backup_name(dest, o.backup, &o.suffix);
            match fs::rename(dest, &name) {
                Ok(()) => backup = Some(name),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    eprintln!("ln: cannot backup {}: {}", qa(dest), gnu_io_error(&e));
                    return false;
                }
            }
        } else if remove
            && let Err(e) = fs::remove_file(dest)
            && e.kind() != io::ErrorKind::NotFound
        {
            eprintln!("ln: cannot remove {}: {}", qa(dest), gnu_io_error(&e));
            return false;
        }
    }

    let res = if o.symbolic {
        symlink(&target, dest)
    } else if let Some(real) = &real_src {
        fs::hard_link(real, dest)
    } else {
        fs::hard_link(src_path, dest)
    };
    let arrow = if o.symbolic { "->" } else { "=>" };
    match res {
        Ok(()) => {
            ctx.telemetry.add_items(1);
            if o.verbose {
                match &backup {
                    Some(b) => println!("{} ~ {} {arrow} {}", qa(b), qa(dest), qa(&target)),
                    None => println!("{} {arrow} {}", qa(dest), qa(&target)),
                }
            }
            true
        }
        Err(e) => {
            let errno = e.raw_os_error().unwrap_or(0);
            let msg = if o.symbolic {
                if errno != libc::ENAMETOOLONG && !target.is_empty() {
                    format!("failed to create symbolic link {}", qa(dest))
                } else {
                    format!(
                        "failed to create symbolic link {} -> {}",
                        qa(dest),
                        qa(&target)
                    )
                }
            } else if errno == libc::EMLINK {
                format!("failed to create hard link to {}", qa(src))
            } else if [libc::EDQUOT, libc::EEXIST, libc::ENOSPC, libc::EROFS].contains(&errno) {
                format!("failed to create hard link {}", qa(dest))
            } else {
                format!("failed to create hard link {} => {}", qa(dest), qa(src))
            };
            eprintln!("ln: {msg}: {}", gnu_io_error(&e));
            if let Some(b) = backup {
                let _ = fs::rename(b, dest);
            }
            false
        }
    }
}

/// `ln -sr`: SOURCE relative to the directory that will contain DEST.
fn relative_link_target(src: &Path, dest: &Path) -> OsString {
    let real_src =
        canonicalize_mode(src, CanMode::Missing, false).unwrap_or_else(|_| src.to_path_buf());
    let real_dest =
        canonicalize_mode(dest, CanMode::Missing, false).unwrap_or_else(|_| dest.to_path_buf());
    let dir = real_dest.parent().unwrap_or(Path::new("/"));
    relpath(&real_src, dir).into_os_string()
}
