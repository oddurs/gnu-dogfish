//! tail — SIMD reverse newline scan (`memrchr`), `+N`, `-f` follow + NDJSON support

use std::io::{self, Read, Seek, Write};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, emit_objects_or_table, gnu_io_error,
    io::ignore_broken_pipe, read_ndjson_stdin,
};
use memchr::memchr_iter;

use crate::common::head_tail::{
    digit_option, header_name, last_lines_start, parse_count, to_usize,
};
use crate::common::input::{RawStdout, stdin_file};
use crate::common::lines::{line_end, looks_like_json, lossy_lines};
use crate::common::posixver::posix2_version;
use crate::common::quote::{qa, quote_locale};
use crate::common::xstrtol::{StrtolError, xstrtoumax};

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

    /// Line delimiter is NUL, not newline
    #[arg(short = 'z', long = "zero-terminated")]
    zero_terminated: bool,

    #[arg(short = 'J', long = "json")]
    json: bool,

    files: Vec<String>,
}

/// GNU's `parse_obsolete_option`: with at most one file operand, a first
/// argument such as `-5`, `+2c`, `-l`, `-3bf` is the whole option list. It
/// is rewritten as the equivalent options; `Err` is the exit status after
/// GNU's `invalid number` diagnostic.
fn expand_obsolete(mut args: Vec<String>) -> Result<Vec<String>, i32> {
    let argc = args.len();
    let operand_ok = argc == 2
        || (argc == 3 && !(args[2].starts_with('-') && args[2].len() > 1))
        || ((3..=4).contains(&argc) && args[2] == "--");
    if !operand_ok {
        return Ok(args);
    }
    let posix_ver = posix2_version();
    let obsolete_usage = posix_ver < 200112;
    let traditional_usage = obsolete_usage || 200809 <= posix_ver;
    let arg = args[1].as_bytes();
    let from_start = match arg.first() {
        Some(b'+') if traditional_usage => true,
        // "-" alone is standard input and "-c" needs an argument, except in
        // the obsolete usage; "-5" and the like are always accepted.
        Some(b'-')
            if obsolete_usage
                || arg
                    .get(1 + usize::from(arg.get(1) == Some(&b'c')))
                    .is_some() =>
        {
            false
        }
        _ => return Ok(args),
    };
    let rest = &arg[1..];
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    let mut p = digits;
    let mut default_count = 10u64;
    let mut count_lines = true;
    match rest.get(p) {
        Some(b'b') => {
            default_count *= 512;
            count_lines = false;
            p += 1;
        }
        Some(b'c') => {
            count_lines = false;
            p += 1;
        }
        Some(b'l') => p += 1,
        _ => {}
    }
    let forever = rest.get(p) == Some(&b'f');
    if forever {
        p += 1;
    }
    if p != rest.len() {
        return Ok(args);
    }
    let count = if digits == 0 {
        default_count
    } else {
        let digits = std::str::from_utf8(&rest[..digits]).unwrap_or_default();
        match xstrtoumax(digits, "b") {
            (n, None | Some(StrtolError::InvalidSuffix)) => n,
            _ => {
                eprintln!(
                    "tail: invalid number: {}: Numerical result out of range",
                    quote_locale(&args[1])
                );
                return Err(1);
            }
        }
    };
    let option = if count_lines { "-n" } else { "-c" };
    let sign = if from_start { "+" } else { "" };
    let mut replacement = vec![option.to_string(), format!("{sign}{count}")];
    if forever {
        replacement.push("-f".to_string());
    }
    args.splice(1..2, replacement);
    Ok(args)
}

/// Opens an input; standard input that is a regular file at offset 0 is
/// mapped rather than read, as GNU seeks to its end instead of reading it.
fn open_tail_input(src: &InputSource) -> io::Result<FastReader> {
    if let InputSource::Stdin = src {
        let mut stdin = stdin_file();
        if stdin.metadata().is_ok_and(|m| m.is_file() && m.len() > 0)
            && stdin.stream_position().is_ok_and(|p| p == 0)
        {
            // Safety: read-only mapping of a regular file, as FastReader does.
            if let Ok(map) = unsafe { memmap2::Mmap::map(&*stdin) } {
                let _ = stdin.seek(io::SeekFrom::End(0));
                return Ok(FastReader::Mapped(map));
            }
        }
    }
    FastReader::open(src)
}

/// Offset at which tail output starts.
fn tail_start(bytes: &[u8], by_bytes: bool, from_start: bool, count: u64, eol: u8) -> usize {
    let len = bytes.len();
    match (by_bytes, from_start) {
        (true, true) => to_usize(count.saturating_sub(1)).min(len),
        (true, false) => len - to_usize(count).min(len),
        (false, true) => {
            if count <= 1 {
                0
            } else {
                memchr_iter(eol, bytes)
                    .nth(to_usize(count - 2))
                    .map(|p| p + 1)
                    .unwrap_or(len)
            }
        }
        (false, false) => last_lines_start(bytes, count, eol),
    }
}

pub fn run_tail(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let args = match expand_obsolete(args) {
        Ok(args) => args,
        Err(status) => return Ok(status),
    };
    let long_with_arg = [
        "bytes",
        "lines",
        "max-unchanged-stats",
        "pid",
        "sleep-interval",
    ];
    if let Some(c) = digit_option(&args, b"cns", &long_with_arg) {
        eprintln!("tail: option used in invalid context -- {c}");
        return Ok(1);
    }
    let parsed = TailArgs::try_parse_from(args)?;
    let by_bytes = parsed.bytes.is_some();
    let eol = line_end(parsed.zero_terminated);
    let spec = parsed
        .bytes
        .as_deref()
        .or(parsed.lines.as_deref())
        .unwrap_or("10");
    let (from_start, num_str) = match spec.strip_prefix('+') {
        Some(rest) => (true, rest),
        None => (false, spec.strip_prefix('-').unwrap_or(spec)),
    };
    let count = match parse_count(num_str, !by_bytes) {
        Ok(n) => n,
        Err(msg) => {
            eprintln!("tail: {msg}");
            return Ok(1);
        }
    };
    // Don't read anything if nothing will be output.
    if count == 0 && !parsed.follow && !from_start {
        return Ok(0);
    }
    let sources = InputSource::from_args(&parsed.files);

    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, RawStdout);

    if sources.len() == 1
        && matches!(sources[0], InputSource::Stdin)
        && !by_bytes
        && !from_start
        && !parsed.follow
        && !parsed.verbose
        && !parsed.zero_terminated
        && !ctx.stdin_is_tty
        && (ctx.stdout_is_tty || parsed.json)
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
        let start = tail_start(&all, false, false, count, eol);
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
        let mut reader = match open_tail_input(src) {
            Ok(r) => r,
            Err(e) => {
                let _ = out.flush();
                eprintln!(
                    "tail: cannot open {} for reading: {}",
                    qa(src.display_name()),
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
                let start = tail_start(bytes, by_bytes, from_start, count, eol);
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
                    "tail: error reading {}: {}",
                    qa(header_name(src)),
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
