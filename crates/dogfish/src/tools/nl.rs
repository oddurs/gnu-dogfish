//! nl — Number lines of files

use std::io::{self, BufRead, Write};

use anyhow::Result;
use clap::Parser;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, gnu_io_error,
    io::ignore_broken_pipe, stdout,
};

use crate::common::lines::chomp;

#[derive(Parser)]
#[command(
    name = "nl",
    about = "Write each FILE to standard output, with line numbers added"
)]
struct NlArgs {
    /// Use STYLE for numbering body lines (a: all, t: nonempty, n: none)
    #[arg(short = 'b', long = "body-numbering", default_value = "t")]
    body: String,

    /// Number all lines (including empty ones; equivalent to -ba)
    #[arg(short = 'a', long = "all")]
    all: bool,

    /// Line number increment at each line
    #[arg(
        short = 'i',
        long = "line-increment",
        default_value_t = 1,
        allow_hyphen_values = true
    )]
    increment: i64,

    /// Insert line numbers according to FORMAT (ln, rn, rz)
    #[arg(short = 'n', long = "number-format", default_value = "rn")]
    format: String,

    /// Add STRING after (possible) line number
    #[arg(short = 's', long = "number-separator", default_value = "\t")]
    separator: String,

    /// First line number
    #[arg(
        short = 'v',
        long = "starting-line-number",
        default_value_t = 1,
        allow_hyphen_values = true
    )]
    start: i64,

    /// Use NUMBER columns for line numbers
    #[arg(short = 'w', long = "number-width", default_value_t = 6)]
    width: usize,

    files: Vec<String>,
}

pub fn run_nl(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = NlArgs::try_parse_from(args)?;
    let style = if parsed.all {
        "a"
    } else {
        parsed.body.as_str()
    };
    if !matches!(style, "a" | "t" | "n") {
        eprintln!("nl: invalid body numbering style: '{}'", style);
        return Ok(1);
    }
    if !matches!(parsed.format.as_str(), "ln" | "rn" | "rz") {
        eprintln!("nl: invalid line numbering format: '{}'", parsed.format);
        return Ok(1);
    }
    let width = parsed.width;
    if width == 0 {
        eprintln!("nl: invalid line number field width: '0'");
        return Ok(1);
    }
    let sources = InputSource::from_args(&parsed.files);
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout::stdout());
    let st = ctx.style();
    let sep = parsed.separator.as_bytes();
    let blank_pad = vec![b' '; width + sep.len()];
    let mut line_no = parsed.start;
    let mut exit_code = 0;

    for src in &sources {
        let mut reader = match FastReader::open(src) {
            Ok(r) => r.into_bufread(),
            Err(e) => {
                let _ = out.flush();
                eprintln!("nl: {}: {}", src.display_name(), gnu_io_error(&e));
                exit_code = 1;
                continue;
            }
        };
        let mut line = Vec::with_capacity(256);
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    let _ = out.flush();
                    eprintln!("nl: {}: {}", src.display_name(), gnu_io_error(&e));
                    exit_code = 1;
                    break;
                }
            }
            ctx.telemetry.add_read(line.len() as u64);
            let text = chomp(&line);
            let numbered = match style {
                "a" => true,
                "t" => !text.is_empty(),
                _ => false,
            };
            if numbered {
                let num = match parsed.format.as_str() {
                    "ln" => format!("{:<width$}", line_no),
                    "rz" if line_no < 0 => {
                        format!("-{:0>w$}", -line_no, w = width.saturating_sub(1))
                    }
                    "rz" => format!("{:0>width$}", line_no),
                    _ => format!("{:>width$}", line_no),
                };
                if st.enabled {
                    write!(out, "\x1b[2m{}\x1b[0m", num)?;
                } else {
                    out.write_all(num.as_bytes())?;
                }
                out.write_all(sep)?;
                line_no += parsed.increment;
            } else {
                out.write_all(&blank_pad)?;
            }
            out.write_all(text)?;
            ignore_broken_pipe(out.write_all(b"\n"))?;
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(exit_code)
}
