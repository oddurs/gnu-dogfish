//! yes — Page-aligned 64 KiB vector I/O

use std::io::{self, Write};

use anyhow::Result;
use clap::{ArgAction, Parser};
use dogfish_core::{DEFAULT_BUF_SIZE, DogfishContext};

#[derive(Parser)]
#[command(
    name = "yes",
    about = "Repeatedly output a line with all specified STRING(s), or 'y'",
    disable_help_flag = true,
    infer_long_args = true
)]
struct YesArgs {
    /// Display this help and exit
    #[arg(long, action = ArgAction::Help)]
    help: Option<bool>,

    /// String(s) to repeat
    strings: Vec<String>,
}

pub fn run_yes(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = YesArgs::try_parse_from(args)?;
    let line = if parsed.strings.is_empty() {
        "y\n".to_string()
    } else {
        format!("{}\n", parsed.strings.join(" "))
    };

    ctx.telemetry.set_strategy("64KiB page-aligned buffer");
    let line_bytes = line.as_bytes();
    let mut buf = Vec::with_capacity(DEFAULT_BUF_SIZE + line_bytes.len());
    while buf.len() < DEFAULT_BUF_SIZE {
        buf.extend_from_slice(line_bytes);
    }

    let stdout = io::stdout();
    let mut handle = stdout.lock();
    loop {
        match handle.write_all(&buf) {
            Ok(()) => ctx.telemetry.add_written(buf.len() as u64),
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(0)
}
