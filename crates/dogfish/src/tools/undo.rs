//! undo — Reverses the most recent `rm --safe`, `mv`, or `cp` operation

use std::ffi::OsString;

use anyhow::Result;
use dogfish_core::{DogfishContext, UndoJournal, outln};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, count_arg, getopt_error, lossy, print_help, print_version, usage_error,
};
use crate::common::quote::quote_locale;

const UNDO_HELP: &str = "\
Undo the most recent reversible file operation (rm --safe, mv, cp)

Usage: undo [OPTIONS]

Options:
  -n, --count <COUNT>  Number of operations to undo [default: 1]
  -h, --help           Print help
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum UndoOpt {
    Count,
    Help,
    Version,
}

const SPECS: &[OptSpec<UndoOpt>] = &[
    OptSpec::both(b'n', "count", HasArg::Required, UndoOpt::Count),
    OptSpec::both(b'h', "help", HasArg::No, UndoOpt::Help),
    OptSpec::long("version", HasArg::No, UndoOpt::Version),
];

/// Parses the command line into the number of operations to undo; `Err` is
/// the exit status after `--help`, `--version` or a usage diagnostic.
fn parse_undo_args(args: Vec<OsString>) -> std::result::Result<usize, i32> {
    let mut count = 1;
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    while let Some(opt) = opts.next() {
        let (o, v) = opt.map_err(|e| getopt_error("undo", &e))?;
        match o {
            UndoOpt::Count => {
                count = count_arg(
                    "undo",
                    "--count",
                    &v.as_deref().map(lossy).unwrap_or_default(),
                )?
            }
            UndoOpt::Help => return Err(print_help(UNDO_HELP)),
            UndoOpt::Version => return Err(print_version("undo")),
        }
    }
    if let Some(extra) = opts.operands().first() {
        return Err(usage_error(
            "undo",
            format!("extra operand {}", quote_locale(extra)),
        ));
    }
    Ok(count)
}

pub fn run_undo(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let count = match parse_undo_args(args) {
        Ok(n) => n,
        Err(code) => return Ok(code),
    };
    let journal = UndoJournal::open()?;
    let st = ctx.style();

    for _ in 0..count {
        match journal.undo_last()? {
            Some((entry, summary)) => {
                outln!(
                    "{} [id #{}, tool: {}] {}",
                    st.bold_green("✔ Undone:"),
                    entry.id,
                    st.bold_cyan(&entry.tool),
                    summary
                );
                ctx.telemetry.add_items(1);
            }
            None => {
                outln!(
                    "{}",
                    st.yellow("No more operations in Dogfish undo journal.")
                );
                break;
            }
        }
    }
    Ok(0)
}

/// undo's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("undo", UNDO_HELP, SPECS)
}
