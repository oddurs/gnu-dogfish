//! undo — Reverses the most recent `rm --safe`, `mv`, or `cp` operation

use anyhow::Result;
use clap::Parser;
use dogfish_core::{DogfishContext, UndoJournal};

#[derive(Parser)]
#[command(
    name = "undo",
    about = "Undo the most recent reversible file operation (rm --safe, mv, cp)"
)]
struct UndoArgs {
    /// Number of operations to undo
    #[arg(short = 'n', long = "count", default_value_t = 1)]
    count: usize,
}

pub fn run_undo(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = UndoArgs::try_parse_from(args)?;
    let journal = UndoJournal::open()?;
    let st = ctx.style();

    for _ in 0..parsed.count {
        match journal.undo_last()? {
            Some((entry, summary)) => {
                println!(
                    "{} [id #{}, tool: {}] {}",
                    st.bold_green("✔ Undone:"),
                    entry.id,
                    st.bold_cyan(&entry.tool),
                    summary
                );
                ctx.telemetry.add_items(1);
            }
            None => {
                println!(
                    "{}",
                    st.yellow("No more operations in Dogfish undo journal.")
                );
                break;
            }
        }
    }
    Ok(0)
}
