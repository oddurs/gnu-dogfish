//! history — Inspect the Dogfish undo journal

use std::ffi::OsString;

use anyhow::Result;
use dogfish_core::{
    ColumnAlign, DogfishContext, JournalAction, JournalEntry, Record, RecordWriter, Table,
    UndoJournal, outln, stdout,
};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, print_help, print_version, usage_error,
};
use crate::common::json_opt::{check_json_version, json_long, json_short};
use crate::common::quote::quote_locale;

const HISTORY_HELP: &str = "\
Display the Dogfish reversible operation journal

Usage: history [OPTIONS]

Options:
  -J, --json[=VERSION]
              One NDJSON record per journal entry
  -h, --help  Print help
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum HistoryOpt {
    Json,
    Help,
    Version,
}

const SPECS: &[OptSpec<HistoryOpt>] = &[
    json_short(HistoryOpt::Json),
    json_long(HistoryOpt::Json),
    OptSpec::both(b'h', "help", HasArg::No, HistoryOpt::Help),
    OptSpec::long("version", HasArg::No, HistoryOpt::Version),
];

/// Parses the command line into whether to print JSON; `Err` is the exit
/// status after `--help`, `--version` or a usage diagnostic.
fn parse_history_args(args: Vec<OsString>) -> std::result::Result<bool, i32> {
    let mut json = false;
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    while let Some(opt) = opts.next() {
        let (key, value) = opt.map_err(|e| getopt_error("history", &e))?;
        match key {
            HistoryOpt::Json => {
                check_json_version("history", value.as_deref())?;
                json = true;
            }
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
        let mut w = RecordWriter::stdout();
        for e in &entries {
            w.record(|r| history_record(r, e))?;
        }
        w.flush()?;
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

/// One `-J` record: the entry's id, time and tool, the action (`trash`,
/// `move` or `copy`) and its paths, and the journal's state flags.
fn history_record(r: &mut Record<'_>, e: &JournalEntry) {
    let secs = i64::try_from(e.timestamp_unix).unwrap_or(i64::MAX);
    r.u64("id", e.id).time("time", secs, 0).str("tool", &e.tool);
    match &e.action {
        JournalAction::TrashRemove {
            original_path,
            trash_path,
        } => {
            r.str("action", "trash")
                .os_str("path", original_path)
                .os_str("trash", trash_path);
        }
        JournalAction::Move {
            src,
            dst,
            backup_path,
            ..
        } => {
            r.str("action", "move")
                .os_str("source", src)
                .os_str("dest", dst);
            if let Some(b) = backup_path {
                r.os_str("backup", b);
            }
        }
        JournalAction::CopyCreate {
            src,
            created_path,
            backup_path,
            ..
        } => {
            r.str("action", "copy")
                .os_str("source", src)
                .os_str("dest", created_path);
            if let Some(b) = backup_path {
                r.os_str("backup", b);
            }
        }
    }
    if e.backup_bytes > 0 {
        r.u64("backup_size", e.backup_bytes);
    }
    for (key, on) in [
        ("backup_purged", e.backup_purged),
        ("pending", e.pending),
        ("cancelled", e.cancelled),
    ] {
        if on {
            r.bool(key, true);
        }
    }
}

/// history's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("history", HISTORY_HELP, SPECS)
}
