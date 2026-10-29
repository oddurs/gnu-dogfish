//! `realpath`

use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use dogfish_core::{DogfishContext, emit_objects_or_table, gnu_io_error, io::ignore_broken_pipe};

use crate::common::canonicalize::{CanMode, canonicalize_mode, relpath};
use crate::common::quote::qf;
use crate::common::tool_args::{HasArg, ToolOpt, opt, parse_tool_args, usage_error};

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
}

pub fn run_realpath(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    use RealpathOpt::*;
    const SPECS: &[ToolOpt<RealpathOpt>] = &[
        opt("e", "canonicalize-existing", HasArg::No, Existing),
        opt("m", "canonicalize-missing", HasArg::No, Missing),
        opt("L", "logical", HasArg::No, Logical),
        opt("P", "physical", HasArg::No, Physical),
        opt("q", "quiet", HasArg::No, Quiet),
        opt("", "relative-to", HasArg::Required, RelativeTo),
        opt("", "relative-base", HasArg::Required, RelativeBase),
        opt("s", "strip", HasArg::No, Strip),
        opt("", "no-symlinks", HasArg::No, Strip),
        opt("z", "zero", HasArg::No, Zero),
        opt("J", "json", HasArg::No, Json),
    ];
    let parsed = match parse_tool_args("realpath", &args, SPECS, REALPATH_HELP) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let (mut mode, mut nolinks, mut logical, mut quiet, mut zero, mut json) =
        (CanMode::AllButLast, false, false, false, false, false);
    let (mut relative_to, mut relative_base) = (None, None);
    for (o, v) in parsed.opts {
        match o {
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
    if parsed.operands.is_empty() {
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
    let canon_dir = |d: &str| -> std::result::Result<PathBuf, i32> {
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

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    let mut exit_code = 0;
    let mut results = Vec::new();
    for arg in &parsed.operands {
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
            results.push(serde_json::json!({ "input": arg, "realpath": shown.to_string_lossy() }));
        } else {
            out.write_all(shown.as_os_str().as_bytes())?;
            out.write_all(if zero { b"\0" } else { b"\n" })?;
        }
    }
    ignore_broken_pipe(out.flush())?;
    drop(out);
    if json {
        emit_objects_or_table(&results, ctx, true, false)?;
    }
    Ok(exit_code)
}
