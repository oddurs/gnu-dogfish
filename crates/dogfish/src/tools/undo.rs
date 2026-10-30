//! undo — Reverses the most recent `rm --safe`, `mv`, or `cp` operation

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;

use anyhow::Result;
use dogfish_core::{DogfishContext, JournalEntry, UndoJournal, Undone, outln};

use crate::common::actions;
use crate::common::getopt::{
    Getopt, HasArg, OptSpec, count_arg, getopt_error, lossy, print_help, print_version, usage_error,
};
use crate::common::json_opt::{check_json_version, json_long, json_short};
use crate::common::quote::quote_locale;
use crate::tools::history::action_fields;

const UNDO_HELP: &str = "\
Undo the most recent reversible file operation (rm --safe, mv, cp)

Usage: undo [OPTIONS]

Options:
  -n, --count <COUNT>  Number of operations to undo [default: 1]
  -J, --json[=VERSION]
                       One NDJSON record per operation undone
  -h, --help           Print help
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum UndoOpt {
    Count,
    Json,
    Help,
    Version,
}

const SPECS: &[OptSpec<UndoOpt>] = &[
    OptSpec::both(b'n', "count", HasArg::Required, UndoOpt::Count),
    OptSpec::both(b'h', "help", HasArg::No, UndoOpt::Help),
    OptSpec::long("version", HasArg::No, UndoOpt::Version),
    json_short(UndoOpt::Json),
    json_long(UndoOpt::Json),
];

/// Parses the command line into the number of operations to undo and
/// whether to write records (`-J`); `Err` is the exit status after
/// `--help`, `--version` or a usage diagnostic.
fn parse_undo_args(args: Vec<OsString>) -> std::result::Result<(usize, bool), i32> {
    let (mut count, mut json) = (1, false);
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
            UndoOpt::Json => {
                check_json_version("undo", v.as_deref())?;
                json = true;
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
    Ok((count, json))
}

pub fn run_undo(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let (count, json) = match parse_undo_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    if json {
        actions::start();
    }
    let journal = UndoJournal::open()?;
    let st = ctx.style();

    for _ in 0..count {
        match journal.undo_last()? {
            Some((entry, undone)) => {
                if json {
                    undo_record(&entry, &undone);
                } else {
                    outln!(
                        "{} [id #{}, tool: {}] {}",
                        st.bold_green("✔ Undone:"),
                        entry.id,
                        st.bold_cyan(&entry.tool),
                        undone.summary
                    );
                }
                ctx.telemetry.add_items(1);
            }
            // With -J, an empty journal is no records.
            None if json => break,
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

/// The `-J` record of one entry undone: `{"action":"undo","undo_id":N,
/// "tool":...,"undone":"trash"|"move"|"copy", the entry's paths as `history
/// -J` names them, "restored":[...],"removed":[...]}`.
fn undo_record(entry: &JournalEntry, undone: &Undone) {
    fn bytes(paths: &[std::path::PathBuf]) -> Vec<&[u8]> {
        paths.iter().map(|p| p.as_os_str().as_bytes()).collect()
    }
    actions::record("undo", |r| {
        r.u64("undo_id", entry.id).str("tool", &entry.tool);
        action_fields(r, "undone", &entry.action);
        r.bytes_list("restored", &bytes(&undone.restored))
            .bytes_list("removed", &bytes(&undone.removed));
    });
}

/// undo's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("undo", UNDO_HELP, SPECS)
}
