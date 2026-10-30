//! `mkdir`

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;

use anyhow::Result;
use dogfish_core::{DogfishContext, gnu_io_error, outln};

use crate::common::actions;
use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy, print_help, print_version, usage_error,
};
use crate::common::json_opt::{check_json_version, json_long, json_short};
use crate::common::mode::{
    CHMOD_MODE_BITS, S_IRWXUGO, S_ISGID, S_ISUID, S_ISVTX, mode_adjust, mode_compile,
    process_umask, u32_to_mode_t,
};
use crate::common::path::ancestor_ends;
use crate::common::quote::{qa, quote_locale};

const MKDIR_HELP: &str = "\
Usage: mkdir [OPTION]... DIRECTORY...
Create the DIRECTORY(ies), if they do not already exist.

  -m, --mode=MODE   set file mode (as in chmod), not a=rwx - umask
  -p, --parents     no error if existing, make parent directories as needed,
                    with their file modes unaffected by any -m option
  -v, --verbose     print a message for each created directory
  -Z, --context     accepted for compatibility (SELinux is not supported)
  -J, --json[=VERSION]  Dogfish: one NDJSON record per created directory
                    (what -v reports), instead of -v's text
      --help        display this help and exit
      --version     output version information and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum MkdirOpt {
    Mode,
    Parents,
    Verbose,
    Context,
    Json,
    Help,
    Version,
}

/// GNU's `longopts` and `"pm:vZ"` (`-Z` takes no argument, `--context` an
/// optional one).
const SPECS: &[OptSpec<MkdirOpt>] = &[
    OptSpec::long("context", HasArg::Optional, MkdirOpt::Context),
    OptSpec::both(b'm', "mode", HasArg::Required, MkdirOpt::Mode),
    OptSpec::both(b'p', "parents", HasArg::No, MkdirOpt::Parents),
    OptSpec::both(b'v', "verbose", HasArg::No, MkdirOpt::Verbose),
    OptSpec::long("help", HasArg::No, MkdirOpt::Help),
    OptSpec::long("version", HasArg::No, MkdirOpt::Version),
    OptSpec::short(b'Z', HasArg::No, MkdirOpt::Context),
    json_short(MkdirOpt::Json),
    json_long(MkdirOpt::Json),
];

pub fn run_mkdir(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    use MkdirOpt::*;
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    let (mut mode_arg, mut parents, mut verbose) = (None, false, false);
    while let Some(opt) = opts.next() {
        let (o, v) = match opt {
            Ok(o) => o,
            Err(e) => return Ok(getopt_error("mkdir", &e)),
        };
        match o {
            Mode => mode_arg = v,
            Parents => parents = true,
            Verbose => verbose = true,
            Context => {}
            Json => {
                if let Err(code) = check_json_version("mkdir", v.as_deref()) {
                    return Ok(code);
                }
                verbose = true;
                actions::start();
            }
            Help => return Ok(print_help(MKDIR_HELP)),
            Version => return Ok(print_version("mkdir")),
        }
    }
    let operands = opts.operands();
    if operands.is_empty() {
        return Ok(usage_error("mkdir", "missing operand"));
    }

    // As in GNU: the final directory is made under `umask & !mode` (so -m wins),
    // ancestors under the umask minus u+wx.
    let umask = process_umask();
    let (mode, mode_bits) = match &mode_arg {
        Some(m) => match mode_compile(&lossy(m)) {
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
    for dir in &operands {
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
fn mkdir_with_umask(path: &OsStr, mode: u32, umask: u32) -> io::Result<()> {
    // SAFETY: umask has no preconditions and cannot fail; mkdir runs on one
    // thread, so nothing else creates files under the temporary mask.
    let old = unsafe { libc::umask(u32_to_mode_t(umask)) };
    let res = fs::DirBuilder::new().mode(mode).create(path);
    // SAFETY: restores the mask saved just above.
    unsafe { libc::umask(old) };
    res
}

fn mkdir_one(
    dir: &OsStr,
    parents: bool,
    mode: u32,
    mode_bits: u32,
    masks: MkdirMasks,
    verbose: bool,
) -> std::result::Result<(), String> {
    let announce = |d: &OsStr| {
        if actions::on() {
            actions::record("mkdir", |r| {
                r.os_str("path", d);
            });
        } else if verbose {
            outln!("mkdir: created directory {}", qa(d));
        }
    };
    let cannot =
        |d: &OsStr, why: String| format!("cannot create directory {}: {why}", quote_locale(d));
    if parents {
        for end in ancestor_ends(dir.as_bytes()) {
            let prefix = OsStr::from_bytes(&dir.as_bytes()[..end]);
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

/// mkdir's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("mkdir", MKDIR_HELP, SPECS)
}
