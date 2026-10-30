//! `realpath`

use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use dogfish_core::{DogfishContext, emit_objects_or_table, gnu_io_error, stdout};

use crate::common::canonicalize::{CanMode, canonicalize_mode, relpath};
use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, print_help, print_version, usage_error,
};
use crate::common::quote::qf;

const REALPATH_HELP: &str = "\
Usage: realpath [OPTION]... FILE...
Print the resolved absolute file name;
all but the last component must exist

  -e, --canonicalize-existing  all components of the path must exist
  -m, --canonicalize-missing   no path components need exist or be a directory
  -L, --logical                resolve '..' components before symlinks
  -P, --physical               resolve symlinks as encountered (default)
  -q, --quiet                  suppress most error messages
      --relative-to=DIR        print the resolved path relative to DIR
      --relative-base=DIR      print absolute paths unless paths below DIR
  -s, --strip, --no-symlinks   don't expand symlinks
  -z, --zero                   end each output line with NUL, not newline
  -J, --json                   emit {input, realpath} objects (Dogfish)
      --help        display this help and exit
      --version     output version information and exit
";

/// True if `prefix` is `path` or one of its ancestors (component-wise).
fn path_prefix(prefix: &Path, path: &Path) -> bool {
    path.starts_with(prefix)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RealpathOpt {
    Existing,
    Missing,
    Logical,
    Physical,
    Quiet,
    RelativeTo,
    RelativeBase,
    Strip,
    Zero,
    Json,
    Help,
    Version,
}

/// GNU's `longopts` and `"eLmPqsz"`, plus the Dogfish `-J, --json`.
const SPECS: &[OptSpec<RealpathOpt>] = &[
    OptSpec::both(
        b'e',
        "canonicalize-existing",
        HasArg::No,
        RealpathOpt::Existing,
    ),
    OptSpec::both(
        b'm',
        "canonicalize-missing",
        HasArg::No,
        RealpathOpt::Missing,
    ),
    OptSpec::long("relative-to", HasArg::Required, RealpathOpt::RelativeTo),
    OptSpec::long("relative-base", HasArg::Required, RealpathOpt::RelativeBase),
    OptSpec::both(b'q', "quiet", HasArg::No, RealpathOpt::Quiet),
    OptSpec::both(b's', "strip", HasArg::No, RealpathOpt::Strip),
    OptSpec::long("no-symlinks", HasArg::No, RealpathOpt::Strip),
    OptSpec::both(b'z', "zero", HasArg::No, RealpathOpt::Zero),
    OptSpec::both(b'L', "logical", HasArg::No, RealpathOpt::Logical),
    OptSpec::both(b'P', "physical", HasArg::No, RealpathOpt::Physical),
    OptSpec::long("help", HasArg::No, RealpathOpt::Help),
    OptSpec::long("version", HasArg::No, RealpathOpt::Version),
    OptSpec::both(b'J', "json", HasArg::No, RealpathOpt::Json).exact(),
];

pub fn run_realpath(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    use RealpathOpt::*;
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    let (mut mode, mut nolinks, mut logical, mut quiet, mut zero, mut json) =
        (CanMode::AllButLast, false, false, false, false, false);
    let (mut relative_to, mut relative_base): (Option<OsString>, Option<OsString>) = (None, None);
    while let Some(opt) = opts.next() {
        let (o, v) = match opt {
            Ok(o) => o,
            Err(e) => return Ok(getopt_error("realpath", &e)),
        };
        match o {
            Help => return Ok(print_help(REALPATH_HELP)),
            Version => return Ok(print_version("realpath")),
            Existing => mode = CanMode::Existing,
            Missing => mode = CanMode::Missing,
            Logical => {
                logical = true;
                nolinks = true;
            }
            Physical => {
                logical = false;
                nolinks = false;
            }
            Strip => {
                nolinks = true;
                logical = false;
            }
            Quiet => quiet = true,
            RelativeTo => relative_to = v,
            RelativeBase => relative_base = v,
            Zero => zero = true,
            Json => json = true,
        }
    }
    let operands = opts.operands();
    if operands.is_empty() {
        return Ok(usage_error("realpath", "missing operand"));
    }
    let canon = |p: &Path| -> io::Result<PathBuf> {
        let first = canonicalize_mode(p, mode, nolinks)?;
        if logical {
            // -L: '..' was resolved lexically; now resolve the symlinks.
            canonicalize_mode(&first, mode, false)
        } else {
            Ok(first)
        }
    };
    let canon_dir = |d: &OsStr| -> std::result::Result<PathBuf, i32> {
        match canon(Path::new(d)) {
            Ok(p) if mode == CanMode::Existing && !p.is_dir() => {
                eprintln!("realpath: {}: Not a directory", qf(d));
                Err(1)
            }
            Ok(p) => Ok(p),
            Err(e) => {
                eprintln!("realpath: {}: {}", qf(d), gnu_io_error(&e));
                Err(1)
            }
        }
    };
    if relative_base.is_some() && relative_to.is_none() {
        relative_to = relative_base.clone();
    }
    let mut rel_to = match &relative_to {
        Some(d) => match canon_dir(d) {
            Ok(p) => Some(p),
            Err(code) => return Ok(code),
        },
        None => None,
    };
    let rel_base = match &relative_base {
        Some(b) if Some(b) == relative_to.as_ref() => rel_to.clone(),
        Some(b) => match canon_dir(b) {
            // --relative-to is a no-op unless --relative-base is a prefix of it.
            Ok(base) if rel_to.as_ref().is_some_and(|to| path_prefix(&base, to)) => Some(base),
            Ok(_) => rel_to.take(),
            Err(code) => return Ok(code),
        },
        None => None,
    };

    let mut out = io::BufWriter::new(stdout::stdout());
    let mut exit_code = 0;
    let mut results = Vec::new();
    for arg in &operands {
        let resolved = match canon(Path::new(arg)) {
            Ok(p) => p,
            Err(e) => {
                if !quiet {
                    out.flush()?;
                    eprintln!("realpath: {}: {}", qf(arg), gnu_io_error(&e));
                }
                exit_code = 1;
                continue;
            }
        };
        let shown = match (&rel_to, &rel_base) {
            (Some(to), base) if base.as_ref().is_none_or(|b| path_prefix(b, &resolved)) => {
                relpath(&resolved, to)
            }
            _ => resolved,
        };
        ctx.telemetry.add_items(1);
        if json {
            results.push(
                serde_json::json!({ "input": arg.to_string_lossy(), "realpath": shown.to_string_lossy() }),
            );
        } else {
            out.write_all(shown.as_os_str().as_bytes())?;
            out.write_all(if zero { b"\0" } else { b"\n" })?;
        }
    }
    out.flush()?;
    drop(out);
    if json {
        emit_objects_or_table(&results, ctx, true, false)?;
    }
    Ok(exit_code)
}

/// realpath's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("realpath", REALPATH_HELP, SPECS)
}
