//! paste — Merge lines of files side-by-side or serially

use std::collections::VecDeque;
use std::io::{self, BufRead, Write};

use anyhow::{Result, anyhow};
use clap::Parser;
use dogfish_core::{DogfishContext, FastReader, InputSource, gnu_io_error, io::ignore_broken_pipe};

#[derive(Parser)]
#[command(name = "paste", about = "Merge lines of files")]
struct PasteArgs {
    /// Use characters from LIST instead of TABs
    #[arg(short = 'd', long = "delimiters", default_value = "\t")]
    delimiters: String,

    /// Paste one file at a time instead of in parallel
    #[arg(short = 's', long = "serial")]
    serial: bool,

    files: Vec<String>,
}

pub fn run_paste(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = PasteArgs::try_parse_from(args)?;
    let sources = InputSource::from_args(&parsed.files);
    let delims: Vec<char> = parsed
        .delimiters
        .replace("\\n", "\n")
        .replace("\\t", "\t")
        .chars()
        .collect();
    let delims = if delims.is_empty() {
        vec!['\t']
    } else {
        delims
    };

    let mut file_lines: Vec<VecDeque<String>> = Vec::with_capacity(sources.len());
    for src in &sources {
        let reader = FastReader::open(src)
            .map_err(|e| anyhow!("paste: {}: {}", src.display_name(), gnu_io_error(&e)))?
            .into_bufread();
        let q: VecDeque<String> = reader.lines().collect::<io::Result<_>>()?;
        file_lines.push(q);
    }

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    if parsed.serial {
        for q in &file_lines {
            let mut row = String::new();
            for (i, line) in q.iter().enumerate() {
                if i > 0 {
                    row.push(delims[(i - 1) % delims.len()]);
                }
                row.push_str(line);
            }
            ignore_broken_pipe(writeln!(out, "{}", row))?;
            ctx.telemetry.add_written(row.len() as u64 + 1);
        }
    } else {
        loop {
            if file_lines.iter().all(|q| q.is_empty()) {
                break;
            }
            let mut row = String::new();
            for (i, q) in file_lines.iter_mut().enumerate() {
                if i > 0 {
                    row.push(delims[(i - 1) % delims.len()]);
                }
                if let Some(line) = q.pop_front() {
                    row.push_str(&line);
                }
            }
            ignore_broken_pipe(writeln!(out, "{}", row))?;
            ctx.telemetry.add_written(row.len() as u64 + 1);
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}
