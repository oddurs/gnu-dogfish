//! chmod — octal and full symbolic modes, -R, -c/-v/-f, --reference

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use anyhow::Result;
use dogfish_core::{DogfishContext, gnu_io_error, outln};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy, print_help, print_version, usage_error,
};
use crate::common::mode::{
    CHMOD_MODE_BITS, ModeChange, mode_adjust, mode_compile, process_umask, strmode9,
};
use crate::common::path::dir_names;
use crate::common::preserve_root::{DevIno, is_root_dev_ino, refuse_root, root_dev_ino};
use crate::common::quote::{qa, qf, quote_locale};

const CHMOD_HELP: &str = "\
Usage: chmod [OPTION]... MODE[,MODE]... FILE...
  or:  chmod [OPTION]... OCTAL-MODE FILE...
  or:  chmod [OPTION]... --reference=RFILE FILE...
Change the mode of each FILE to MODE.
With --reference, change the mode of each FILE to that of RFILE.

  -c, --changes          like verbose but report only when a change is made
  -f, --silent, --quiet  suppress most error messages
  -v, --verbose          output a diagnostic for every file processed
      --no-preserve-root  do not treat '/' specially (the default)
      --preserve-root    fail to operate recursively on '/'
      --reference=RFILE  use RFILE's mode instead of specifying MODE values
  -R, --recursive        change files and directories recursively
      --help        display this help and exit
      --version     output version information and exit

Each MODE is of the form '[ugoa]*([-+=]([rwxXst]*|[ugo]))+|[-+=][0-7]+'.
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChmodOpt {
    Changes,
    Quiet,
    Verbose,
    NoPreserveRoot,
    PreserveRoot,
    Reference,
    Recursive,
    /// A mode that looks like an option (`-w`, `-rx`, `-0`).
    Mode,
    Help,
    Version,
}

/// A mode character that GNU's option string accepts as an option with an
/// optional argument, so that `chmod -w FILE` works.
const fn mode_char(c: u8) -> OptSpec<ChmodOpt> {
    OptSpec::short(c, HasArg::Optional, ChmodOpt::Mode)
}

/// GNU's `long_options` and its option string
/// `"Rcfvr::w::x::X::s::t::u::g::o::a::,::+::=::0::1::2::3::4::5::6::7::"`.
const SPECS: &[OptSpec<ChmodOpt>] = &[
    OptSpec::both(b'c', "changes", HasArg::No, ChmodOpt::Changes),
    OptSpec::both(b'R', "recursive", HasArg::No, ChmodOpt::Recursive),
    OptSpec::long("no-preserve-root", HasArg::No, ChmodOpt::NoPreserveRoot),
    OptSpec::long("preserve-root", HasArg::No, ChmodOpt::PreserveRoot),
    OptSpec::both(b'f', "quiet", HasArg::No, ChmodOpt::Quiet),
    OptSpec::long("reference", HasArg::Required, ChmodOpt::Reference),
    OptSpec::long("silent", HasArg::No, ChmodOpt::Quiet),
    OptSpec::both(b'v', "verbose", HasArg::No, ChmodOpt::Verbose),
    OptSpec::long("help", HasArg::No, ChmodOpt::Help),
    OptSpec::long("version", HasArg::No, ChmodOpt::Version),
    mode_char(b'r'),
    mode_char(b'w'),
    mode_char(b'x'),
    mode_char(b'X'),
    mode_char(b's'),
    mode_char(b't'),
    mode_char(b'u'),
    mode_char(b'g'),
    mode_char(b'o'),
    mode_char(b'a'),
    mode_char(b','),
    mode_char(b'+'),
    mode_char(b'='),
    mode_char(b'0'),
    mode_char(b'1'),
    mode_char(b'2'),
    mode_char(b'3'),
    mode_char(b'4'),
    mode_char(b'5'),
    mode_char(b'6'),
    mode_char(b'7'),
];

struct ChmodRun {
    changes: Option<Vec<ModeChange>>,
    reference_mode: u32,
    umask: u32,
    recursive: bool,
    report_changes: bool,
    verbose: bool,
    quiet: bool,
    diagnose_surprises: bool,
    /// `/`'s identity under `-R --preserve-root` (GNU `root_dev_ino`).
    root_dev_ino: Option<DevIno>,
}

pub fn run_chmod(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    use ChmodOpt::*;
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    let mut run = ChmodRun {
        changes: None,
        reference_mode: 0,
        umask: process_umask(),
        recursive: false,
        report_changes: false,
        verbose: false,
        quiet: false,
        diagnose_surprises: false,
        root_dev_ino: None,
    };
    let (mut preserve_root, mut reference) = (false, None);
    // Raw bytes, so a diagnostic quotes the mode exactly as given.
    let mut mode_pieces: Vec<OsString> = Vec::new();
    while let Some(opt) = opts.next() {
        let (o, v) = match opt {
            Ok(o) => o,
            Err(e) => return Ok(getopt_error("chmod", &e)),
        };
        match o {
            Changes => run.report_changes = true,
            Quiet => run.quiet = true,
            Verbose => run.verbose = true,
            NoPreserveRoot => preserve_root = false,
            PreserveRoot => preserve_root = true,
            Reference => reference = v,
            Recursive => run.recursive = true,
            // Like GNU, the whole argument the mode character came from is
            // the mode (`-rwx`), joined to earlier ones with a comma.
            Mode => {
                mode_pieces.push(opts.last_word().unwrap_or_default().to_os_string());
                run.diagnose_surprises = true;
            }
            Help => return Ok(print_help(CHMOD_HELP)),
            Version => return Ok(print_version("chmod")),
        }
    }
    let mut files = opts.operands();
    let mut mode = (!mode_pieces.is_empty()).then(|| mode_pieces.join(OsStr::new(",")));
    if reference.is_none() && mode.is_none() {
        if files.is_empty() {
            return Ok(usage_error("chmod", "missing operand"));
        }
        mode = Some(files.remove(0));
    }
    if files.is_empty() {
        return Ok(match (&mode, &reference) {
            (Some(m), None) => usage_error(
                "chmod",
                format!("missing operand after {}", quote_locale(m)),
            ),
            _ => usage_error("chmod", "missing operand"),
        });
    }
    if let Some(r) = &reference {
        match fs::metadata(r) {
            Ok(m) => run.reference_mode = m.mode() & CHMOD_MODE_BITS,
            Err(e) => {
                eprintln!(
                    "chmod: failed to get attributes of {}: {}",
                    qa(r),
                    gnu_io_error(&e)
                );
                return Ok(1);
            }
        }
    } else if let Some(m) = &mode {
        match mode_compile(&lossy(m)) {
            Some(c) => run.changes = Some(c),
            None => {
                return Ok(usage_error(
                    "chmod",
                    format!("invalid mode: {}", quote_locale(m)),
                ));
            }
        }
    }

    if run.recursive && preserve_root {
        match root_dev_ino() {
            Ok(r) => run.root_dev_ino = Some(r),
            Err(e) => {
                eprintln!(
                    "chmod: failed to get attributes of '/': {}",
                    gnu_io_error(&e)
                );
                return Ok(1);
            }
        }
    }

    let mut ok = true;
    for f in &files {
        ok &= chmod_path(Path::new(f), true, &run, ctx);
    }
    Ok(if ok { 0 } else { 1 })
}

fn chmod_path(path: &Path, top_level: bool, run: &ChmodRun, ctx: &DogfishContext) -> bool {
    let lmeta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => {
            if !run.quiet {
                eprintln!("chmod: cannot access {}: {}", qa(path), gnu_io_error(&e));
            }
            if run.verbose {
                outln!("{} could not be accessed", qa(path));
            }
            return false;
        }
    };
    let meta = if lmeta.is_symlink() {
        if !top_level {
            // During -R traversal, symbolic links are not followed or changed.
            if run.verbose {
                outln!(
                    "neither symbolic link {} nor referent has been changed",
                    qa(path)
                );
            }
            return true;
        }
        match fs::metadata(path) {
            Ok(m) => m,
            Err(_) => {
                if !run.quiet {
                    eprintln!("chmod: cannot operate on dangling symlink {}", qa(path));
                }
                if run.verbose {
                    outln!("{} could not be accessed", qa(path));
                }
                return false;
            }
        }
    } else {
        lmeta
    };
    // GNU `process_file`: checked on the stat the walk uses, so a command-line
    // symlink to `/` (followed, as fts's FTS_COMFOLLOW does) is refused too.
    if is_root_dev_ino(run.root_dev_ino, &meta) {
        refuse_root("chmod", path);
        return false;
    }

    let old = meta.mode() & CHMOD_MODE_BITS;
    let is_dir = meta.is_dir();
    let new = match &run.changes {
        Some(c) => mode_adjust(old, is_dir, run.umask, c).0,
        None => run.reference_mode,
    };
    let mut ok = true;
    let applied = match fs::set_permissions(path, fs::Permissions::from_mode(new)) {
        Ok(()) => {
            ctx.telemetry.add_items(1);
            true
        }
        Err(e) => {
            if !run.quiet {
                eprintln!(
                    "chmod: changing permissions of {}: {}",
                    qa(path),
                    gnu_io_error(&e)
                );
            }
            ok = false;
            false
        }
    };
    let changed = applied && old != new;
    if run.verbose || (run.report_changes && changed) {
        let (o, n) = (strmode9(old), strmode9(new));
        if !applied {
            outln!(
                "failed to change mode of {} from {old:04o} ({o}) to {new:04o} ({n})",
                qa(path)
            );
        } else if changed {
            outln!(
                "mode of {} changed from {old:04o} ({o}) to {new:04o} ({n})",
                qa(path)
            );
        } else {
            outln!("mode of {} retained as {new:04o} ({n})", qa(path));
        }
    }
    if applied
        && run.diagnose_surprises
        && let Some(c) = &run.changes
    {
        let naive = mode_adjust(old, is_dir, 0, c).0;
        if new & !naive != 0 {
            eprintln!(
                "chmod: {}: new permissions are {}, not {}",
                qf(path),
                strmode9(new),
                strmode9(naive)
            );
            ok = false;
        }
    }

    if run.recursive && is_dir {
        match dir_names(path, false) {
            Ok(names) => {
                for name in names {
                    ok &= chmod_path(&path.join(name), false, run, ctx);
                }
            }
            Err(e) => {
                if !run.quiet {
                    eprintln!(
                        "chmod: cannot read directory {}: {}",
                        qa(path),
                        gnu_io_error(&e)
                    );
                }
                ok = false;
            }
        }
    }
    ok
}
