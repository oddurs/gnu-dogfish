//! head — SIMD newline scan, GNU `-n -N` / `-c -N`, + NDJSON auto-table rendering!

use std::io::{self, BufRead, Read, Write};

use std::ffi::OsString;

use anyhow::Result;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, emit_objects_or_table, gnu_io_error,
    io::ignore_broken_pipe, read_ndjson_stdin, stdout,
};
use memchr::memchr_iter;

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy, lossy_all, print_help, print_version, try_help,
    unsupported,
};
use crate::common::head_tail::{
    header_name, last_lines_start, normalize_legacy_num_arg, parse_gnu_count, to_usize,
};
use crate::common::lines::{looks_like_json, lossy_lines};

const HEAD_HELP: &str = "\
Output the first part of files (or NDJSON object streams)

Usage: head [OPTIONS] [FILES]...

Arguments:
  [FILES]...  

Options:
  -n, --lines <[-]NUM>  Print the first NUM lines (default 10); with a leading '-', all but the last
                        NUM lines
  -c, --bytes <[-]NUM>  Print the first NUM bytes; with a leading '-', all but the last NUM bytes
  -q, --quiet           Never print headers giving file names [alias: --silent]
  -v, --verbose         Always print headers giving file names
  -J, --json            Force JSON output when processing NDJSON
      --help            Print help
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum HeadOpt {
    Bytes,
    Lines,
    PresumeInputPipe,
    Quiet,
    Verbose,
    Json,
    /// A digit after other options (`-c 5 -3`): GNU's "invalid trailing option".
    Digit(u8),
    /// A GNU option Dogfish does not implement yet.
    Unsupported,
    Help,
    Version,
}

const fn digit(c: u8) -> OptSpec<HeadOpt> {
    OptSpec::short(c, HasArg::No, HeadOpt::Digit(c))
}

/// GNU's `long_options` and `"c:n:qvz0123456789"`, plus the Dogfish
/// `-J, --json`. `---presume-input-pipe` is GNU's hidden test option.
const SPECS: &[OptSpec<HeadOpt>] = &[
    OptSpec::both(b'c', "bytes", HasArg::Required, HeadOpt::Bytes),
    OptSpec::both(b'n', "lines", HasArg::Required, HeadOpt::Lines),
    OptSpec::long("-presume-input-pipe", HasArg::No, HeadOpt::PresumeInputPipe),
    OptSpec::both(b'q', "quiet", HasArg::No, HeadOpt::Quiet),
    OptSpec::long("silent", HasArg::No, HeadOpt::Quiet),
    OptSpec::both(b'v', "verbose", HasArg::No, HeadOpt::Verbose),
    OptSpec::both(b'z', "zero-terminated", HasArg::No, HeadOpt::Unsupported),
    OptSpec::long("help", HasArg::No, HeadOpt::Help),
    OptSpec::long("version", HasArg::No, HeadOpt::Version),
    OptSpec::both(b'J', "json", HasArg::No, HeadOpt::Json),
    digit(b'0'),
    digit(b'1'),
    digit(b'2'),
    digit(b'3'),
    digit(b'4'),
    digit(b'5'),
    digit(b'6'),
    digit(b'7'),
    digit(b'8'),
    digit(b'9'),
];

/// Copies all but the last `n` bytes or lines of `r` to `out`, holding back
/// only what may still be among the last `n`.
fn elide_tail_stream<R: BufRead + ?Sized, W: Write>(
    r: &mut R,
    by_bytes: bool,
    n: u64,
    out: &mut W,
) -> io::Result<u64> {
    let mut pending: Vec<u8> = Vec::new();
    // Newlines in `pending`, when counting lines.
    let mut complete = 0u64;
    let mut written = 0u64;
    loop {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        if !by_bytes {
            complete += memchr_iter(b'\n', buf).count() as u64;
        }
        pending.extend_from_slice(buf);
        let len = buf.len();
        r.consume(len);
        // Bytes that are surely not among the last `n` units: for lines, the
        // complete lines beyond the last `n` (a partial line still to come
        // would only push more lines out).
        let ready = if by_bytes {
            pending.len().saturating_sub(to_usize(n))
        } else if complete > n {
            let excess = complete - n;
            complete = n;
            memchr_iter(b'\n', &pending)
                .nth(to_usize(excess - 1))
                .map_or(0, |p| p + 1)
        } else {
            0
        };
        if ready > 0 {
            out.write_all(&pending[..ready])?;
            written += ready as u64;
            pending.drain(..ready);
        }
    }
    let end = if by_bytes {
        pending.len().saturating_sub(to_usize(n))
    } else {
        last_lines_start(&pending, n)
    };
    out.write_all(&pending[..end])?;
    Ok(written + end as u64)
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

/// What `head` was asked for; as in GNU, the last `-c`/`-n` wins.
struct HeadArgs {
    by_bytes: bool,
    all_but: bool,
    count: u64,
    quiet: bool,
    verbose: bool,
    json: bool,
    files: Vec<String>,
}

/// Parses the command line; `Err` is the exit status after a diagnostic.
fn parse_head_args(args: Vec<OsString>) -> std::result::Result<HeadArgs, i32> {
    let mut a = HeadArgs {
        by_bytes: false,
        all_but: false,
        count: 10,
        quiet: false,
        verbose: false,
        json: false,
        files: Vec::new(),
    };
    let mut opts = Getopt::new(
        normalize_legacy_num_arg(args).into_iter().skip(1).collect(),
        SPECS,
    );
    while let Some(opt) = opts.next() {
        let (o, v) = opt.map_err(|e| getopt_error("head", &e))?;
        match o {
            HeadOpt::Bytes | HeadOpt::Lines => {
                // Like GNU, each count is checked as it is met.
                a.by_bytes = o == HeadOpt::Bytes;
                let spec = v.as_deref().map(lossy).unwrap_or_default();
                let (all_but, num_str) = match spec.strip_prefix('-') {
                    Some(rest) => (true, rest),
                    None => (false, spec.strip_prefix('+').unwrap_or(&spec)),
                };
                let Some(count) = parse_gnu_count(num_str) else {
                    eprintln!(
                        "head: invalid number of {}: '{}'",
                        if a.by_bytes { "bytes" } else { "lines" },
                        spec
                    );
                    return Err(1);
                };
                (a.all_but, a.count) = (all_but, count);
            }
            HeadOpt::PresumeInputPipe => {}
            HeadOpt::Quiet => (a.quiet, a.verbose) = (true, false),
            HeadOpt::Verbose => (a.quiet, a.verbose) = (false, true),
            HeadOpt::Json => a.json = true,
            HeadOpt::Digit(c) => {
                eprintln!("head: invalid trailing option -- {}", char::from(c));
                return Err(try_help("head"));
            }
            HeadOpt::Unsupported => return Err(unsupported("head", &opts.last_spelling())),
            HeadOpt::Help => return Err(print_help(HEAD_HELP)),
            HeadOpt::Version => return Err(print_version("head")),
        }
    }
    a.files = lossy_all(opts.operands());
    Ok(a)
}

/// Prints GNU's diagnostic for a failed write while copying; `Ok` for any
/// other error.
fn write_failed(e: &io::Error) -> bool {
    if !stdout::is_write_error(e) {
        return false;
    }
    stdout::mark_reported();
    eprintln!("head: error writing 'standard output': {}", gnu_io_error(e));
    true
}

pub fn run_head(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_head_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let (by_bytes, all_but, count) = (parsed.by_bytes, parsed.all_but, parsed.count);
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
        let res: io::Result<u64> = if all_but && !reader.is_mapped() {
            // A pipe may never end: write out what cannot be among the last
            // COUNT units as it arrives, as GNU does.
            let mut r = reader.into_bufread();
            elide_tail_stream(&mut *r, by_bytes, count, &mut out)
        } else if all_but {
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
            Err(e) if write_failed(&e) => return Ok(1),
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
