//! history — Inspect the Dogfish undo journal

use anyhow::Result;
use clap::Parser;
use dogfish_core::{ColumnAlign, DogfishContext, JournalAction, Table, UndoJournal, outln, stdout};

#[derive(Parser)]
#[command(
    name = "history",
    about = "Display the Dogfish reversible operation journal"
)]
struct HistoryArgs {
    #[arg(short = 'J', long = "json")]
    json: bool,
}

pub fn run_history(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = HistoryArgs::try_parse_from(args)?;
    let journal = UndoJournal::open()?;
    let entries = journal.load_entries()?;

    if parsed.json {
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
            } => format!("trashed '{}'", original_path),
            JournalAction::Move { src, dst, .. } => format!("'{}' -> '{}'", src, dst),
            JournalAction::CopyCreate {
                src, created_path, ..
            } => {
                format!("copied '{}' -> '{}'", src, created_path)
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
