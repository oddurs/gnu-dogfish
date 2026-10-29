//! head — SIMD newline scan, GNU `-n -N` / `-c -N`, + NDJSON auto-table rendering!

use std::io::{self, BufRead, Read, Write};

use anyhow::Result;
use clap::Parser;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, emit_objects_or_table, gnu_io_error,
    io::ignore_broken_pipe, read_ndjson_stdin, stdout,
};
use memchr::memchr_iter;

use crate::common::head_tail::{
    header_name, last_lines_start, normalize_legacy_num_arg, parse_gnu_count, to_usize,
};
use crate::common::lines::{looks_like_json, lossy_lines};

#[derive(Parser)]
#[command(
    name = "head",
    about = "Output the first part of files (or NDJSON object streams)"
)]
struct HeadArgs {
    /// Print the first NUM lines (default 10); with a leading '-', all but the last NUM lines
    #[arg(
        short = 'n',
        long = "lines",
        value_name = "[-]NUM",
        allow_hyphen_values = true
    )]
    lines: Option<String>,

    /// Print the first NUM bytes; with a leading '-', all but the last NUM bytes
    #[arg(
        short = 'c',
        long = "bytes",
        value_name = "[-]NUM",
        allow_hyphen_values = true
    )]
    bytes: Option<String>,

    /// Never print headers giving file names
    #[arg(short = 'q', long = "quiet", visible_alias = "silent")]
    quiet: bool,

    /// Always print headers giving file names
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    /// Force JSON output when processing NDJSON
    #[arg(short = 'J', long = "json")]
    json: bool,

    files: Vec<String>,
}

/// Copies the first `n` lines of `r` to `out`.
fn copy_first_lines<R: BufRead + ?Sized, W: Write>(
    r: &mut R,
    mut n: u64,
    out: &mut W,
) -> io::Result<u64> {
    let mut written = 0u64;
    while n > 0 {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        let mut end = buf.len();
        for pos in memchr_iter(b'\n', buf) {
            n -= 1;
            if n == 0 {
                end = pos + 1;
                break;
            }
        }
        out.write_all(&buf[..end])?;
        written += end as u64;
        r.consume(end);
    }
    Ok(written)
}

/// Copies the first `n` bytes of `r` to `out`.
fn copy_first_bytes<R: BufRead + ?Sized, W: Write>(
    r: &mut R,
    mut n: u64,
    out: &mut W,
) -> io::Result<u64> {
    let mut written = 0u64;
    while n > 0 {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        let end = buf.len().min(usize::try_from(n).unwrap_or(usize::MAX));
        out.write_all(&buf[..end])?;
        written += end as u64;
        n -= end as u64;
        r.consume(end);
    }
    Ok(written)
}

pub fn run_head(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = HeadArgs::try_parse_from(normalize_legacy_num_arg(args))?;
    let by_bytes = parsed.bytes.is_some();
    let spec = parsed
        .bytes
        .as_deref()
        .or(parsed.lines.as_deref())
        .unwrap_or("10");
    let (all_but, num_str) = match spec.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, spec.strip_prefix('+').unwrap_or(spec)),
    };
    let Some(count) = parse_gnu_count(num_str) else {
        eprintln!(
            "head: invalid number of {}: '{}'",
            if by_bytes { "bytes" } else { "lines" },
            spec
        );
        return Ok(1);
    };
    let sources = InputSource::from_args(&parsed.files);

    // Adaptive NDJSON detection when reading from piped stdin!
    if sources.len() == 1
        && matches!(sources[0], InputSource::Stdin)
        && !by_bytes
        && !all_but
        && !parsed.verbose
        && !ctx.stdin_is_tty
    {
        return head_stdin_adaptive(count, parsed.json, ctx);
    }

    ctx.telemetry.set_strategy("64KiB chunk + memchr SIMD");
    let show_headers = parsed.verbose || (!parsed.quiet && sources.len() > 1);
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout::stdout());
    let mut first_header = true;
    let mut exit_code = 0;

    for src in &sources {
        let mut reader = match FastReader::open(src) {
            Ok(r) => r,
            Err(e) => {
                let _ = out.flush();
                eprintln!(
                    "head: cannot open '{}' for reading: {}",
                    src.display_name(),
                    gnu_io_error(&e)
                );
                exit_code = 1;
                continue;
            }
        };
        if show_headers {
            if !first_header {
                ignore_broken_pipe(writeln!(out))?;
            }
            first_header = false;
            ignore_broken_pipe(writeln!(out, "==> {} <==", header_name(src)))?;
        }
        let res: io::Result<u64> = if all_but {
            reader
                .with_bytes(|bytes| {
                    let end = if by_bytes {
                        bytes.len().saturating_sub(to_usize(count))
                    } else {
                        last_lines_start(bytes, count)
                    };
                    out.write_all(&bytes[..end]).map(|_| end as u64)
                })
                .and_then(|r| r)
        } else {
            let mut r = reader.into_bufread();
            if by_bytes {
                copy_first_bytes(&mut *r, count, &mut out)
            } else {
                copy_first_lines(&mut *r, count, &mut out)
            }
        };
        match res {
            Ok(n) => {
                ctx.telemetry.add_read(n);
                ctx.telemetry.add_written(n);
            }
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(exit_code),
            Err(e) => {
                let _ = out.flush();
                eprintln!(
                    "head: error reading '{}': {}",
                    src.display_name(),
                    gnu_io_error(&e)
                );
                exit_code = 1;
            }
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(exit_code)
}

fn head_stdin_adaptive(count: u64, json: bool, ctx: &DogfishContext) -> Result<i32> {
    let stdin = io::stdin();
    let mut reader = io::BufReader::with_capacity(DEFAULT_BUF_SIZE, stdin.lock());
    let mut out = io::BufWriter::new(stdout::stdout());

    // Peek the first line to see if it starts with '{' or '[' before reading all of stdin!
    let mut first_line = Vec::new();
    if reader.read_until(b'\n', &mut first_line)? == 0 {
        return Ok(0);
    }
    if looks_like_json(&first_line) {
        let mut all = first_line;
        reader.read_to_end(&mut all)?;
        if let Some(objs) = read_ndjson_stdin(&lossy_lines(&all)) {
            ctx.telemetry
                .set_strategy("NDJSON object stream + auto-table");
            let slice = &objs[..objs.len().min(to_usize(count))];
            ctx.telemetry.add_items(slice.len() as u64);
            drop(out);
            emit_objects_or_table(slice, ctx, json, !json)?;
            return Ok(0);
        }
        ignore_broken_pipe(copy_first_lines(&mut &all[..], count, &mut out).map(|_| ()))?;
        ignore_broken_pipe(out.flush())?;
        return Ok(0);
    }

    // Plain text streaming fast-path: stop immediately after `count` lines!
    if count > 0 {
        ignore_broken_pipe(out.write_all(&first_line))?;
        ignore_broken_pipe(copy_first_lines(&mut reader, count - 1, &mut out).map(|_| ()))?;
    }
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}
