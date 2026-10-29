//! `mkdir`

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;

use anyhow::Result;
use dogfish_core::{DogfishContext, gnu_io_error};

use crate::common::mode::{
    CHMOD_MODE_BITS, S_IRWXUGO, S_ISGID, S_ISUID, S_ISVTX, mode_adjust, mode_compile,
    process_umask, u32_to_mode_t,
};
use crate::common::path::ancestor_ends;
use crate::common::quote::{qa, quote_locale};
use crate::common::tool_args::{HasArg, ToolOpt, opt, parse_tool_args, usage_error};

const MKDIR_HELP: &str = "\
Usage: mkdir [OPTION]... DIRECTORY...
Create the DIRECTORY(ies), if they do not already exist.

  -m, --mode=MODE   set file mode (as in chmod), not a=rwx - umask
  -p, --parents     no error if existing, make parent directories as needed,
                    with their file modes unaffected by any -m option
  -v, --verbose     print a message for each created directory
  -Z, --context     accepted for compatibility (SELinux is not supported)
      --help        display this help and exit
      --version     output version information and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum MkdirOpt {
    Mode,
    Parents,
    Verbose,
    Context,
}

pub fn run_mkdir(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    use MkdirOpt::*;
    const SPECS: &[ToolOpt<MkdirOpt>] = &[
        opt("m", "mode", HasArg::Required, Mode),
        opt("p", "parents", HasArg::No, Parents),
        opt("v", "verbose", HasArg::No, Verbose),
        opt("Z", "", HasArg::No, Context),
        opt("", "context", HasArg::Optional, Context),
    ];
    let parsed = match parse_tool_args("mkdir", &args, SPECS, MKDIR_HELP) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let (mut mode_arg, mut parents, mut verbose) = (None, false, false);
    for (o, v) in parsed.opts {
        match o {
            Mode => mode_arg = v,
            Parents => parents = true,
            Verbose => verbose = true,
            Context => {}
        }
    }
    if parsed.operands.is_empty() {
        return Ok(usage_error("mkdir", "missing operand"));
    }

    // As in GNU: the final directory is made under `umask & !mode` (so -m wins),
    // ancestors under the umask minus u+wx.
    let umask = process_umask();
    let (mode, mode_bits) = match &mode_arg {
        Some(m) => match mode_compile(m) {
            Some(changes) => mode_adjust(S_IRWXUGO, true, umask, &changes),
            None => {
                eprintln!("mkdir: invalid mode {}", quote_locale(m));
                return Ok(1);
            }
        },
        None => (S_IRWXUGO, 0),
    };
    let masks = MkdirMasks {
        own: if mode_arg.is_some() {
            umask & !mode
        } else {
            umask
        },
        ancestor: umask & !0o300,
    };

    let mut status = 0;
    for dir in &parsed.operands {
        match mkdir_one(dir, parents, mode, mode_bits, masks, verbose) {
            Ok(()) => ctx.telemetry.add_items(1),
            Err(msg) => {
                eprintln!("mkdir: {msg}");
                status = 1;
            }
        }
    }
    Ok(status)
}

/// The umasks `mkdir` creates directories under.
#[derive(Clone, Copy)]
struct MkdirMasks {
    own: u32,
    ancestor: u32,
}

/// mkdir(2) under a given umask (restored afterwards).
fn mkdir_with_umask(path: &str, mode: u32, umask: u32) -> io::Result<()> {
    // SAFETY: umask has no preconditions and cannot fail; mkdir runs on one
    // thread, so nothing else creates files under the temporary mask.
    let old = unsafe { libc::umask(u32_to_mode_t(umask)) };
    let res = fs::DirBuilder::new().mode(mode).create(path);
    // SAFETY: restores the mask saved just above.
    unsafe { libc::umask(old) };
    res
}

fn mkdir_one(
    dir: &str,
    parents: bool,
    mode: u32,
    mode_bits: u32,
    masks: MkdirMasks,
    verbose: bool,
) -> std::result::Result<(), String> {
    let announce = |d: &str| {
        if verbose {
            println!("mkdir: created directory {}", qa(d));
        }
    };
    let cannot =
        |d: &str, why: String| format!("cannot create directory {}: {why}", quote_locale(d));
    if parents {
        for end in ancestor_ends(dir) {
            let prefix = &dir[..end];
            match mkdir_with_umask(prefix, S_IRWXUGO, masks.ancestor) {
                Ok(()) => announce(prefix),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    if !Path::new(prefix).is_dir() {
                        let dangling = fs::metadata(prefix).is_err();
                        let why = if dangling {
                            "File exists"
                        } else {
                            "Not a directory"
                        };
                        return Err(cannot(prefix, why.to_string()));
                    }
                }
                Err(e) => return Err(cannot(prefix, gnu_io_error(&e))),
            }
        }
    }
    // gnulib `make_dir_parents`: when special bits are involved, create the
    // directory without group/other write first, then fix the bits -m asked for.
    let keep_special = ((mode_bits & (S_ISUID | S_ISGID)) | (mode & S_ISVTX)) == 0;
    let mkdir_mode = if keep_special { mode } else { mode & !0o022 };
    match mkdir_with_umask(dir, mkdir_mode, masks.own) {
        Ok(()) => {
            announce(dir);
            let umask_ok = mode & mode_bits & S_IRWXUGO == 0;
            if !(keep_special && umask_ok)
                && let Ok(meta) = fs::metadata(dir)
            {
                let st = meta.mode() & CHMOD_MODE_BITS;
                if (st ^ mode) & mode_bits != 0 {
                    let new = mode | (st & !mode_bits);
                    fs::set_permissions(dir, fs::Permissions::from_mode(new)).map_err(|e| {
                        format!(
                            "cannot change permissions of {}: {}",
                            qa(dir),
                            gnu_io_error(&e)
                        )
                    })?;
                }
            }
            Ok(())
        }
        Err(e)
            if parents && e.kind() == io::ErrorKind::AlreadyExists && Path::new(dir).is_dir() =>
        {
            Ok(())
        }
        Err(e) => Err(cannot(dir, gnu_io_error(&e))),
    }
}
