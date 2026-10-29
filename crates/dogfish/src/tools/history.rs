//! history — Inspect the Dogfish undo journal

use std::ffi::OsString;

use anyhow::Result;
use dogfish_core::{ColumnAlign, DogfishContext, JournalAction, Table, UndoJournal, outln, stdout};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, print_help, print_version, usage_error,
};
use crate::common::quote::quote_locale;

const HISTORY_HELP: &str = "\
Display the Dogfish reversible operation journal

Usage: history [OPTIONS]

Options:
  -J, --json  
  -h, --help  Print help
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum HistoryOpt {
    Json,
    Help,
    Version,
}

const SPECS: &[OptSpec<HistoryOpt>] = &[
    OptSpec::both(b'J', "json", HasArg::No, HistoryOpt::Json),
    OptSpec::both(b'h', "help", HasArg::No, HistoryOpt::Help),
    OptSpec::long("version", HasArg::No, HistoryOpt::Version),
];

/// Parses the command line into whether to print JSON; `Err` is the exit
/// status after `--help`, `--version` or a usage diagnostic.
fn parse_history_args(args: Vec<OsString>) -> std::result::Result<bool, i32> {
    let mut json = false;
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    while let Some(opt) = opts.next() {
        match opt.map_err(|e| getopt_error("history", &e))?.0 {
            HistoryOpt::Json => json = true,
            HistoryOpt::Help => return Err(print_help(HISTORY_HELP)),
            HistoryOpt::Version => return Err(print_version("history")),
        }
    }
    if let Some(extra) = opts.operands().first() {
        return Err(usage_error(
            "history",
            format!("extra operand {}", quote_locale(extra)),
        ));
    }
    Ok(json)
}

pub fn run_history(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let json = match parse_history_args(args) {
        Ok(j) => j,
        Err(code) => return Ok(code),
    };
    let journal = UndoJournal::open()?;
    let entries = journal.load_entries()?;

    if json {
        outln!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(0);
    }

    let st = ctx.style();
    if entries.is_empty() {
        outln!("{}", st.dim("Undo journal is empty. Use `rm --safe`, `mv`, or `cp` to record reversible actions."));
        return Ok(0);
    }

    let mut tbl = Table::new(vec![
        ("ID", ColumnAlign::Right),
        ("TOOL", ColumnAlign::Left),
        ("DETAILS", ColumnAlign::Left),
    ]);

    for e in entries.iter().rev() {
        let details = match &e.action {
            JournalAction::TrashRemove {
                original_path,
                trash_path: _,
            } => format!("trashed '{}'", original_path.display()),
            JournalAction::Move { src, dst, .. } => {
                format!("'{}' -> '{}'", src.display(), dst.display())
            }
            JournalAction::CopyCreate {
                src, created_path, ..
            } => {
                format!("copied '{}' -> '{}'", src.display(), created_path.display())
            }
        };
        tbl.add_row(vec![
            st.bold_cyan(&format!("#{}", e.id)),
            st.bold_yellow(&e.tool),
            details,
        ]);
    }

    let mut out = stdout::stdout();
    tbl.render(&mut out, st)?;
    Ok(0)
}
