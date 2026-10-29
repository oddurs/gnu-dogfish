//! tail — SIMD reverse newline scan (`memrchr`), `+N`, `-f` follow + NDJSON support

use std::io::{self, Read, Write};
use std::thread;
use std::time::Duration;

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
    name = "tail",
    about = "Output the last part of files (or NDJSON object streams)"
)]
struct TailArgs {
    /// Output the last NUM lines (default 10); or use -n +NUM to skip NUM-1 lines at the start
    #[arg(
        short = 'n',
        long = "lines",
        value_name = "[+]NUM",
        allow_hyphen_values = true
    )]
    lines: Option<String>,

    /// Output the last NUM bytes; or use -c +NUM to output starting with byte NUM
    #[arg(
        short = 'c',
        long = "bytes",
        value_name = "[+]NUM",
        allow_hyphen_values = true
    )]
    bytes: Option<String>,

    /// Output appended data as the file grows
    #[arg(short = 'f', long = "follow")]
    follow: bool,

    #[arg(short = 'q', long = "quiet", visible_alias = "silent")]
    quiet: bool,

    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    #[arg(short = 'J', long = "json")]
    json: bool,

    files: Vec<String>,
}

/// Offset at which tail output starts.
fn tail_start(bytes: &[u8], by_bytes: bool, from_start: bool, count: u64) -> usize {
    let len = bytes.len();
    match (by_bytes, from_start) {
        (true, true) => to_usize(count.saturating_sub(1)).min(len),
        (true, false) => len - to_usize(count).min(len),
        (false, true) => {
            if count <= 1 {
                0
            } else {
                memchr_iter(b'\n', bytes)
                    .nth(to_usize(count - 2))
                    .map(|p| p + 1)
                    .unwrap_or(len)
            }
        }
        (false, false) => last_lines_start(bytes, count),
    }
}

pub fn run_tail(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = TailArgs::try_parse_from(normalize_legacy_num_arg(args))?;
    let by_bytes = parsed.bytes.is_some();
    let spec = parsed
        .bytes
        .as_deref()
        .or(parsed.lines.as_deref())
        .unwrap_or("10");
    let (from_start, num_str) = match spec.strip_prefix('+') {
        Some(rest) => (true, rest),
        None => (false, spec.strip_prefix('-').unwrap_or(spec)),
    };
    let Some(count) = parse_gnu_count(num_str) else {
        eprintln!(
            "tail: invalid number of {}: '{}'",
            if by_bytes { "bytes" } else { "lines" },
            spec
        );
        return Ok(1);
    };
    let sources = InputSource::from_args(&parsed.files);

    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout::stdout());

    if sources.len() == 1
        && matches!(sources[0], InputSource::Stdin)
        && !by_bytes
        && !from_start
        && !parsed.follow
        && !parsed.verbose
        && !ctx.stdin_is_tty
    {
        let mut all = Vec::new();
        io::stdin().lock().read_to_end(&mut all)?;
        if looks_like_json(&all)
            && let Some(objs) = read_ndjson_stdin(&lossy_lines(&all))
        {
            ctx.telemetry
                .set_strategy("NDJSON object stream + auto-table");
            let start = objs.len().saturating_sub(to_usize(count));
            let slice = &objs[start..];
            ctx.telemetry.add_items(slice.len() as u64);
            drop(out);
            emit_objects_or_table(slice, ctx, parsed.json, !parsed.json)?;
            return Ok(0);
        }
        let start = tail_start(&all, false, false, count);
        ignore_broken_pipe(out.write_all(&all[start..]))?;
        ignore_broken_pipe(out.flush())?;
        return Ok(0);
    }

    ctx.telemetry
        .set_strategy("mmap + reverse memrchr SIMD scan");
    let show_headers = parsed.verbose || (!parsed.quiet && sources.len() > 1);
    let mut first_header = true;
    let mut exit_code = 0;

    for src in &sources {
        let mut reader = match FastReader::open(src) {
            Ok(r) => r,
            Err(e) => {
                let _ = out.flush();
                eprintln!(
                    "tail: cannot open '{}' for reading: {}",
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
        let res = reader
            .with_bytes(|bytes| {
                let start = tail_start(bytes, by_bytes, from_start, count);
                let slice = &bytes[start..];
                ctx.telemetry.add_read(slice.len() as u64);
                ctx.telemetry.add_written(slice.len() as u64);
                out.write_all(slice)
            })
            .and_then(|r| r);
        match res {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(exit_code),
            Err(e) => {
                let _ = out.flush();
                eprintln!(
                    "tail: error reading '{}': {}",
                    src.display_name(),
                    gnu_io_error(&e)
                );
                exit_code = 1;
            }
        }
    }
    ignore_broken_pipe(out.flush())?;

    if parsed.follow
        && let Some(InputSource::File(path)) = sources.first()
    {
        use std::io::{Seek, SeekFrom};
        let mut file = std::fs::File::open(path)?;
        let mut pos = file.seek(SeekFrom::End(0))?;
        let mut buf = [0u8; 8192];
        loop {
            thread::sleep(Duration::from_millis(100));
            let len = file.metadata()?.len();
            if len < pos {
                // Truncated: start again from the beginning.
                pos = 0;
            }
            if len > pos {
                file.seek(SeekFrom::Start(pos))?;
                let n = file.read(&mut buf)?;
                if n > 0 {
                    out.write_all(&buf[..n])?;
                    out.flush()?;
                    pos += n as u64;
                }
            }
        }
    }

    Ok(exit_code)
}
