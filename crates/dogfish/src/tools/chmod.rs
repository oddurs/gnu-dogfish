//! chmod — octal and full symbolic modes, -R, -c/-v/-f, --reference

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use anyhow::Result;
use dogfish_core::{DogfishContext, gnu_io_error};

use crate::common::mode::{
    CHMOD_MODE_BITS, ModeChange, mode_adjust, mode_compile, process_umask, strmode9,
};
use crate::common::path::dir_names;
use crate::common::preserve_root::{is_root_dir, refuse_root};
use crate::common::quote::{qa, qf, quote_locale};
use crate::common::tool_args::{HasArg, ToolOpt, opt, parse_tool_args, usage_error};

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
}

struct ChmodRun {
    changes: Option<Vec<ModeChange>>,
    reference_mode: u32,
    umask: u32,
    recursive: bool,
    report_changes: bool,
    verbose: bool,
    quiet: bool,
    diagnose_surprises: bool,
}

pub fn run_chmod(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    use ChmodOpt::*;
    const SPECS: &[ToolOpt<ChmodOpt>] = &[
        opt("c", "changes", HasArg::No, Changes),
        opt("f", "silent", HasArg::No, Quiet),
        opt("", "quiet", HasArg::No, Quiet),
        opt("v", "verbose", HasArg::No, Verbose),
        opt("", "no-preserve-root", HasArg::No, NoPreserveRoot),
        opt("", "preserve-root", HasArg::No, PreserveRoot),
        opt("", "reference", HasArg::Required, Reference),
        opt("R", "recursive", HasArg::No, Recursive),
    ];
    // Like GNU, accept modes that look like options (`chmod -w file`).
    let mut mode_pieces = Vec::new();
    let mut rest = vec![args.first().cloned().unwrap_or_default()];
    let mut ended = false;
    for a in args.iter().skip(1) {
        ended |= a == "--";
        let mode_like = !ended
            && a.len() > 1
            && a.starts_with('-')
            && a[1..2].chars().all(|c| "rwxXstugoa,+=01234567".contains(c));
        if mode_like {
            mode_pieces.push(a.clone());
        } else {
            rest.push(a.clone());
        }
    }
    let parsed = match parse_tool_args("chmod", &rest, SPECS, CHMOD_HELP) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let mut run = ChmodRun {
        changes: None,
        reference_mode: 0,
        umask: process_umask(),
        recursive: false,
        report_changes: false,
        verbose: false,
        quiet: false,
        diagnose_surprises: !mode_pieces.is_empty(),
    };
    let (mut preserve_root, mut reference) = (false, None);
    for (o, v) in parsed.opts {
        match o {
            Changes => run.report_changes = true,
            Quiet => run.quiet = true,
            Verbose => run.verbose = true,
            NoPreserveRoot => preserve_root = false,
            PreserveRoot => preserve_root = true,
            Reference => reference = v,
            Recursive => run.recursive = true,
        }
    }
    let mut files = parsed.operands;
    let mut mode = (!mode_pieces.is_empty()).then(|| mode_pieces.join(","));
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
                &format!("missing operand after {}", quote_locale(m)),
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
        match mode_compile(m) {
            Some(c) => run.changes = Some(c),
            None => {
                return Ok(usage_error(
                    "chmod",
                    &format!("invalid mode: {}", quote_locale(m)),
                ));
            }
        }
    }

    let mut ok = true;
    for f in &files {
        if run.recursive && preserve_root && is_root_dir(Path::new(f)) {
            refuse_root("chmod", f);
            ok = false;
            continue;
        }
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
                println!("{} could not be accessed", qa(path));
            }
            return false;
        }
    };
    let meta = if lmeta.is_symlink() {
        if !top_level {
            // During -R traversal, symbolic links are not followed or changed.
            if run.verbose {
                println!(
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
                    println!("{} could not be accessed", qa(path));
                }
                return false;
            }
        }
    } else {
        lmeta
    };

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
            println!(
                "failed to change mode of {} from {old:04o} ({o}) to {new:04o} ({n})",
                qa(path)
            );
        } else if changed {
            println!(
                "mode of {} changed from {old:04o} ({o}) to {new:04o} ({n})",
                qa(path)
            );
        } else {
            println!("mode of {} retained as {new:04o} ({n})", qa(path));
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
