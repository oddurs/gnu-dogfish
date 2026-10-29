use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::hash::{BuildHasher, RandomState};
use std::io::{self, BufRead, Read, Write};
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use clap::{ArgAction, Parser};
use digest::Digest;
use dogfish_core::{
    ColumnAlign, DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, Table,
    emit_objects_or_table, eval_json_where, gnu_io_error,
    io::ignore_broken_pipe,
    read_ndjson_stdin, sparkline_bar,
    table::{compare_json_field, read_all_stdin_lines},
};
use md5::Md5;
use memchr::{memchr_iter, memmem, memrchr_iter};
use rayon::prelude::*;
use serde::Serialize;
use serde_json::{Map, Value};
use unicode_width::UnicodeWidthChar;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Name GNU uses for an input in `==> NAME <==` headers.
fn header_name(src: &InputSource) -> &str {
    match src {
        InputSource::Stdin => "standard input",
        InputSource::File(p) => p.as_str(),
    }
}

/// Splits a buffer into lines (without their `\n`); a final unterminated line
/// is yielded as well, but a trailing `\n` does not produce an empty line.
fn split_lines(buf: &[u8]) -> impl Iterator<Item = &[u8]> {
    let body = buf.strip_suffix(b"\n").unwrap_or(buf);
    let empty = buf.is_empty();
    body.split(|&b| b == b'\n').filter(move |_| !empty)
}

/// Strips the line terminator (`\n` only — `\r` is data, as in GNU).
fn chomp(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\n").unwrap_or(line)
}

fn is_blank(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

/// Returns true if the (trimmed) input looks like the start of an NDJSON / JSON stream.
fn looks_like_json(bytes: &[u8]) -> bool {
    matches!(bytes.trim_ascii_start().first(), Some(b'{') | Some(b'['))
}

fn lossy_lines(bytes: &[u8]) -> Vec<String> {
    split_lines(bytes)
        .map(|l| String::from_utf8_lossy(chomp_cr(l)).into_owned())
        .collect()
}

fn chomp_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Parses a GNU count with optional multiplier suffix (`b`, `K`, `kB`, `MiB`, ...).
fn parse_gnu_count(s: &str) -> Option<u64> {
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, suffix) = s.split_at(split);
    if num.is_empty() {
        return None;
    }
    let n: u64 = num.parse().unwrap_or(u64::MAX);
    let pow = |base: u64, exp: u32| base.checked_pow(exp).unwrap_or(u64::MAX);
    let mult = match suffix {
        "" => 1,
        "b" => 512,
        "kB" | "KB" => 1000,
        "k" | "K" | "KiB" | "kiB" => 1024,
        _ => {
            let letters = "KMGTPEZYRQ";
            let mut chars = suffix.chars();
            let first = chars.next()?.to_ascii_uppercase();
            let exp = letters.find(first)? as u32 + 1;
            match chars.as_str() {
                "" | "iB" => pow(1024, exp),
                "B" => pow(1000, exp),
                _ => return None,
            }
        }
    };
    Some(n.saturating_mul(mult))
}

/// Converts the obsolete `-5` shorthand (first argument only, as in GNU) to `-n 5`.
fn normalize_legacy_num_arg(mut args: Vec<String>) -> Vec<String> {
    if let Some(a) = args.get(1)
        && a.len() > 1
        && a.starts_with('-')
        && a[1..].chars().all(|c| c.is_ascii_digit())
    {
        let n = a[1..].to_string();
        args.splice(1..2, ["-n".to_string(), n]);
    }
    args
}

// ---------------------------------------------------------------------------
// 1. cat — Zero-copy mmap + GNU line numbering / squeeze / -v ^X M- notation
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "cat", about = "Concatenate FILE(s) to standard output")]
struct CatArgs {
    /// Equivalent to -vET
    #[arg(short = 'A', long = "show-all")]
    show_all: bool,

    /// Number nonempty output lines, overrides -n
    #[arg(short = 'b', long = "number-nonblank")]
    number_nonblank: bool,

    /// Equivalent to -vE
    #[arg(short = 'e')]
    show_nonprinting_ends: bool,

    /// Display $ at end of each line
    #[arg(short = 'E', long = "show-ends")]
    show_ends: bool,

    /// Number all output lines
    #[arg(short = 'n', long = "number")]
    number: bool,

    /// Suppress repeated empty output lines
    #[arg(short = 's', long = "squeeze-blank")]
    squeeze_blank: bool,

    /// Equivalent to -vT
    #[arg(short = 't')]
    show_nonprinting_tabs: bool,

    /// Display TAB characters as ^I
    #[arg(short = 'T', long = "show-tabs")]
    show_tabs: bool,

    /// (ignored)
    #[arg(short = 'u')]
    unbuffered: bool,

    /// Use ^ and M- notation, except for LFD and TAB
    #[arg(short = 'v', long = "show-nonprinting")]
    show_nonprinting: bool,

    /// Files to concatenate
    files: Vec<String>,
}

struct CatOpts {
    number: bool,
    number_nonblank: bool,
    squeeze: bool,
    show_ends: bool,
    show_tabs: bool,
    show_nonprinting: bool,
}

/// Cross-file state, exactly like GNU cat's `newlines2` / line counter.
struct CatState {
    /// Number of consecutive empty lines seen; -1 while inside a line.
    newlines: i32,
    line_num: u64,
}

pub fn run_cat(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let p = CatArgs::try_parse_from(args)?;
    let _ = p.unbuffered;
    let opts = CatOpts {
        number: p.number || p.number_nonblank,
        number_nonblank: p.number_nonblank,
        squeeze: p.squeeze_blank,
        show_ends: p.show_ends || p.show_all || p.show_nonprinting_ends,
        show_tabs: p.show_tabs || p.show_all || p.show_nonprinting_tabs,
        show_nonprinting: p.show_nonprinting
            || p.show_all
            || p.show_nonprinting_ends
            || p.show_nonprinting_tabs,
    };
    let plain =
        !(opts.number || opts.squeeze || opts.show_ends || opts.show_tabs || opts.show_nonprinting);

    ctx.telemetry.set_strategy(if plain {
        "zero-copy mmap slice write"
    } else {
        "line-decorated stream"
    });

    let sources = InputSource::from_args(&p.files);
    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());
    let mut state = CatState {
        newlines: 0,
        line_num: 0,
    };
    let mut exit_code = 0;

    for src in &sources {
        let res = FastReader::open(src).and_then(|reader| {
            if plain {
                cat_plain(reader, &mut out, ctx)
            } else {
                cat_decorated(reader.into_bufread(), &opts, &mut state, &mut out, ctx)
            }
        });
        if let Err(e) = res {
            if e.kind() == io::ErrorKind::BrokenPipe {
                return Ok(exit_code);
            }
            let _ = out.flush();
            eprintln!("cat: {}: {}", src.display_name(), gnu_io_error(&e));
            exit_code = 1;
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(exit_code)
}

fn cat_plain<W: Write>(reader: FastReader, out: &mut W, ctx: &DogfishContext) -> io::Result<()> {
    match reader {
        FastReader::Mapped(map) => {
            ctx.telemetry.add_read(map.len() as u64);
            out.write_all(&map)?;
            ctx.telemetry.add_written(map.len() as u64);
        }
        FastReader::Buffered(mut r) => {
            let n = io::copy(&mut r, out)?;
            ctx.telemetry.add_read(n);
            ctx.telemetry.add_written(n);
        }
    }
    Ok(())
}

fn cat_push_number(buf: &mut Vec<u8>, state: &mut CatState) {
    state.line_num += 1;
    let mut itoa_buf = itoa::Buffer::new();
    let s = itoa_buf.format(state.line_num);
    for _ in s.len()..6 {
        buf.push(b' ');
    }
    buf.extend_from_slice(s.as_bytes());
    buf.push(b'\t');
}

fn cat_push_visible(buf: &mut Vec<u8>, b: u8, o: &CatOpts) {
    if o.show_nonprinting {
        if b >= 128 {
            buf.extend_from_slice(b"M-");
            let c = b - 128;
            if c < 32 {
                buf.push(b'^');
                buf.push(c + 64);
            } else if c == 127 {
                buf.extend_from_slice(b"^?");
            } else {
                buf.push(c);
            }
            return;
        }
        if b == 127 {
            buf.extend_from_slice(b"^?");
            return;
        }
        if b < 32 && b != b'\t' {
            buf.push(b'^');
            buf.push(b + 64);
            return;
        }
    }
    if b == b'\t' && o.show_tabs {
        buf.extend_from_slice(b"^I");
    } else {
        buf.push(b);
    }
}

fn cat_decorated<W: Write>(
    mut reader: Box<dyn BufRead>,
    o: &CatOpts,
    state: &mut CatState,
    out: &mut W,
    ctx: &DogfishContext,
) -> io::Result<()> {
    let mut obuf: Vec<u8> = Vec::with_capacity(DEFAULT_BUF_SIZE + 1024);
    let mut pending_cr = false;
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        let n = chunk.len();
        ctx.telemetry.add_read(n as u64);
        for &b in chunk {
            // GNU 9.x `-E`: a CR immediately before LF is shown as ^M.
            if pending_cr {
                pending_cr = false;
                obuf.extend_from_slice(if b == b'\n' { b"^M" } else { b"\r" });
            }
            if b == b'\n' {
                state.newlines += 1;
                if state.newlines > 0 {
                    if state.newlines >= 2 {
                        state.newlines = 2;
                        if o.squeeze {
                            continue;
                        }
                    }
                    if o.number && !o.number_nonblank {
                        cat_push_number(&mut obuf, state);
                    }
                }
                if o.show_ends {
                    obuf.push(b'$');
                }
                obuf.push(b'\n');
            } else {
                if state.newlines >= 0 {
                    if o.number {
                        cat_push_number(&mut obuf, state);
                    }
                    state.newlines = -1;
                }
                if b == b'\r' && o.show_ends && !o.show_nonprinting {
                    pending_cr = true;
                } else {
                    cat_push_visible(&mut obuf, b, o);
                }
            }
        }
        reader.consume(n);
        if obuf.len() >= DEFAULT_BUF_SIZE {
            out.write_all(&obuf)?;
            ctx.telemetry.add_written(obuf.len() as u64);
            obuf.clear();
        }
    }
    if pending_cr {
        obuf.push(b'\r');
    }
    out.write_all(&obuf)?;
    ctx.telemetry.add_written(obuf.len() as u64);
    Ok(())
}

// ---------------------------------------------------------------------------
// 2. nl — Number lines of files
// ---------------------------------------------------------------------------
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
    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());
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

// ---------------------------------------------------------------------------
// 3. head — SIMD newline scan, GNU `-n -N` / `-c -N`, + NDJSON auto-table rendering!
// ---------------------------------------------------------------------------
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

/// Offset where the last `n` lines of `bytes` begin (an unterminated final line counts).
fn last_lines_start(bytes: &[u8], n: u64) -> usize {
    if n == 0 || bytes.is_empty() {
        return bytes.len();
    }
    let search = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let nth = usize::try_from(n - 1).unwrap_or(usize::MAX);
    memrchr_iter(b'\n', search)
        .nth(nth)
        .map(|pos| pos + 1)
        .unwrap_or(0)
}

fn to_usize(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
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
    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());
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
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

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

// ---------------------------------------------------------------------------
// 4. tail — SIMD reverse newline scan (`memrchr`), `+N`, `-f` follow + NDJSON support
// ---------------------------------------------------------------------------
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

    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());

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

// ---------------------------------------------------------------------------
// 5. wc — SIMD `bytecount` + parallel UTF-8 aware scanner + `--json` + table mode
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "wc",
    about = "Print newline, word, and byte counts for each FILE"
)]
struct WcArgs {
    /// Print the byte counts
    #[arg(short = 'c', long = "bytes")]
    bytes: bool,

    /// Print the character counts
    #[arg(short = 'm', long = "chars")]
    chars: bool,

    /// Print the newline counts
    #[arg(short = 'l', long = "lines")]
    lines: bool,

    /// Print the word counts
    #[arg(short = 'w', long = "words")]
    words: bool,

    /// Print the maximum display width
    #[arg(short = 'L', long = "max-line-length")]
    max_line_length: bool,

    /// Emit structured JSON / NDJSON
    #[arg(short = 'J', long = "json")]
    json: bool,

    /// Render a rich visual breakdown table with sparklines
    #[arg(short = 't', long = "table")]
    table: bool,

    files: Vec<String>,
}

#[derive(Serialize, Clone, Default)]
struct WcRecord {
    file: String,
    lines: u64,
    words: u64,
    chars: u64,
    bytes: u64,
    max_line_length: u64,
}

#[derive(Clone, Copy, Default)]
struct WcScan {
    lines: u64,
    words: u64,
    chars: u64,
    max_line: u64,
}

/// Unicode whitespace per glibc `iswspace`, plus the no-break spaces GNU wc
/// also treats as word separators.
fn is_unicode_space(c: char) -> bool {
    matches!(
        c as u32,
        0x1680 | 0x2000..=0x200A | 0x2028 | 0x2029 | 0x205F | 0x3000 | 0x00A0 | 0x202F | 0x2060
    )
}

/// Single-pass GNU-compatible (UTF-8 locale) word/char/display-width scanner.
/// The slice must start at a line boundary.
fn wc_scan(bytes: &[u8]) -> WcScan {
    let mut s = WcScan::default();
    let mut in_word = false;
    let mut linepos: u64 = 0;
    let mut i = 0;
    let len = bytes.len();
    while i < len {
        let b = bytes[i];
        if b < 0x80 {
            i += 1;
            s.chars += 1;
            match b {
                b'\n' | b'\r' | 0x0c => {
                    if b == b'\n' {
                        s.lines += 1;
                    }
                    s.max_line = s.max_line.max(linepos);
                    linepos = 0;
                    in_word = false;
                }
                b'\t' => {
                    linepos += 8 - (linepos % 8);
                    in_word = false;
                }
                b' ' => {
                    linepos += 1;
                    in_word = false;
                }
                0x0b => in_word = false,
                0x21..=0x7e => {
                    linepos += 1;
                    if !in_word {
                        s.words += 1;
                        in_word = true;
                    }
                }
                _ => {} // other control characters: neither word nor separator
            }
            continue;
        }
        let seq_len = match b {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => 0,
        };
        let decoded = (seq_len > 0 && i + seq_len <= len)
            .then(|| std::str::from_utf8(&bytes[i..i + seq_len]).ok())
            .flatten()
            .and_then(|st| st.chars().next());
        let Some(c) = decoded else {
            // Invalid / truncated sequence: not a character, zero width.
            i += 1;
            continue;
        };
        i += seq_len;
        s.chars += 1;
        let cp = c as u32;
        if (0x80..0xA0).contains(&cp) {
            continue; // C1 controls
        }
        linepos += c.width().unwrap_or(0) as u64;
        if is_unicode_space(c) {
            in_word = false;
        } else if !in_word {
            s.words += 1;
            in_word = true;
        }
    }
    s.max_line = s.max_line.max(linepos);
    s
}

/// Splits `bytes` into ~`target`-sized pieces that each start at a line boundary.
fn line_aligned_chunks(bytes: &[u8], target: usize) -> Vec<&[u8]> {
    let mut chunks = Vec::with_capacity(bytes.len() / target + 1);
    let mut start = 0;
    while start < bytes.len() {
        let nominal = (start + target).min(bytes.len());
        let end = match memchr::memchr(b'\n', &bytes[nominal..]) {
            Some(off) => nominal + off + 1,
            None => bytes.len(),
        };
        chunks.push(&bytes[start..end]);
        start = end;
    }
    chunks
}

fn wc_count_bytes(bytes: &[u8], need_scan: bool) -> WcScan {
    if !need_scan {
        // Fast path: only `-l` / `-c` requested -> pure SIMD `bytecount::count`
        let lines = if bytes.len() >= 512 * 1024 {
            bytes
                .par_chunks(256 * 1024)
                .map(|c| bytecount::count(c, b'\n') as u64)
                .sum()
        } else {
            bytecount::count(bytes, b'\n') as u64
        };
        return WcScan {
            lines,
            ..Default::default()
        };
    }
    if bytes.len() >= 512 * 1024 {
        // Parallel scan over line-aligned chunks: words never span a newline,
        // so each chunk is independent.
        return line_aligned_chunks(bytes, 256 * 1024)
            .par_iter()
            .map(|c| wc_scan(c))
            .reduce(WcScan::default, |a, b| WcScan {
                lines: a.lines + b.lines,
                words: a.words + b.words,
                chars: a.chars + b.chars,
                max_line: a.max_line.max(b.max_line),
            });
    }
    wc_scan(bytes)
}

fn count_source(src: &InputSource, need_scan: bool, ctx: &DogfishContext) -> io::Result<WcRecord> {
    let mut reader = FastReader::open(src)?;
    reader.with_bytes(|bytes| {
        ctx.telemetry.add_read(bytes.len() as u64);
        ctx.telemetry.add_items(1);
        let s = wc_count_bytes(bytes, need_scan);
        WcRecord {
            file: src.display_name().to_string(),
            lines: s.lines,
            words: s.words,
            chars: s.chars,
            bytes: bytes.len() as u64,
            max_line_length: s.max_line,
        }
    })
}

/// (is_regular_file, size) of standard input.
fn stdin_stat() -> Option<(bool, u64)> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // Safety: fstat on fd 0 with a properly sized, zeroed stat buffer.
    if unsafe { libc::fstat(0, &mut st) } != 0 {
        return None;
    }
    Some((
        (st.st_mode & libc::S_IFMT) == libc::S_IFREG,
        st.st_size as u64,
    ))
}

/// GNU `compute_number_width`: digits of the summed regular-file sizes,
/// at least 7 when any input is not a regular file.
fn wc_number_width(sources: &[InputSource]) -> usize {
    let mut min_width = 1;
    let mut total: u64 = 0;
    for src in sources {
        let stat = match src {
            InputSource::Stdin => stdin_stat(),
            InputSource::File(p) => std::fs::metadata(p).ok().map(|m| (m.is_file(), m.len())),
        };
        match stat {
            Some((true, size)) => total = total.saturating_add(size),
            Some((false, _)) => min_width = 7,
            None => {}
        }
    }
    let mut width = 1;
    while total >= 10 {
        total /= 10;
        width += 1;
    }
    width.max(min_width)
}

pub fn run_wc(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = WcArgs::try_parse_from(args)?;
    let sources = InputSource::from_args(&parsed.files);

    let default_cols =
        !parsed.bytes && !parsed.chars && !parsed.lines && !parsed.words && !parsed.max_line_length;
    let show_lines = default_cols || parsed.lines;
    let show_words = default_cols || parsed.words;
    let show_bytes = default_cols || parsed.bytes;
    let need_scan =
        show_words || parsed.chars || parsed.max_line_length || parsed.json || parsed.table;

    ctx.telemetry.set_strategy(if !need_scan {
        "SIMD bytecount::count (newlines + len only)"
    } else {
        "SIMD bytecount + parallel line-aligned UTF-8 scanner"
    });

    let can_parallel =
        sources.len() > 1 && sources.iter().all(|s| matches!(s, InputSource::File(_)));
    let results: Vec<io::Result<WcRecord>> = if can_parallel {
        sources
            .par_iter()
            .map(|s| count_source(s, need_scan, ctx))
            .collect()
    } else {
        sources
            .iter()
            .map(|s| count_source(s, need_scan, ctx))
            .collect()
    };

    let mut exit_code = 0;
    let mut records = Vec::with_capacity(results.len());
    let report = |src: &InputSource, e: &io::Error| {
        eprintln!("wc: {}: {}", src.display_name(), gnu_io_error(e));
    };

    if parsed.json || parsed.table {
        for (src, res) in sources.iter().zip(results) {
            match res {
                Ok(r) => records.push(r),
                Err(e) => {
                    report(src, &e);
                    exit_code = 1;
                }
            }
        }
        if parsed.json {
            emit_objects_or_table(&records, ctx, true, false)?;
            return Ok(exit_code);
        }
        let max_bytes = records.iter().map(|r| r.bytes).max().unwrap_or(1).max(1);
        let st = ctx.style();
        let mut tbl = Table::new(vec![
            ("FILE", ColumnAlign::Left),
            ("LINES", ColumnAlign::Right),
            ("WORDS", ColumnAlign::Right),
            ("CHARS", ColumnAlign::Right),
            ("SIZE", ColumnAlign::Right),
            ("PROPORTION", ColumnAlign::Left),
        ]);
        for r in &records {
            let ratio = r.bytes as f64 / max_bytes as f64;
            tbl.add_row(vec![
                st.bold(&r.file),
                r.lines.to_string(),
                r.words.to_string(),
                r.chars.to_string(),
                st.green(&dogfish_core::format_size(r.bytes, true)),
                sparkline_bar(ratio, 10, st),
            ]);
        }
        let stdout = io::stdout();
        let mut out = stdout.lock();
        tbl.render(&mut out, st)?;
        return Ok(exit_code);
    }

    let ncols = [
        show_lines,
        show_words,
        parsed.chars,
        show_bytes,
        parsed.max_line_length,
    ]
    .iter()
    .filter(|&&b| b)
    .count();
    let width = if ncols == 1 && sources.len() == 1 {
        1
    } else {
        wc_number_width(&sources)
    };

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    let print_row = |out: &mut io::BufWriter<io::StdoutLock<'_>>,
                     r: &WcRecord,
                     name: Option<&str>|
     -> io::Result<()> {
        let mut row = String::new();
        let cols = [
            (show_lines, r.lines),
            (show_words, r.words),
            (parsed.chars, r.chars),
            (show_bytes, r.bytes),
            (parsed.max_line_length, r.max_line_length),
        ];
        for (shown, val) in cols {
            if shown {
                if !row.is_empty() {
                    row.push(' ');
                }
                row.push_str(&format!("{:>width$}", val));
            }
        }
        if let Some(n) = name {
            row.push(' ');
            row.push_str(n);
        }
        row.push('\n');
        ignore_broken_pipe(out.write_all(row.as_bytes()))
    };

    let mut total = WcRecord::default();
    for (src, res) in sources.iter().zip(results) {
        match res {
            Ok(r) => {
                total.lines += r.lines;
                total.words += r.words;
                total.chars += r.chars;
                total.bytes += r.bytes;
                total.max_line_length = total.max_line_length.max(r.max_line_length);
                let name = (!parsed.files.is_empty()).then_some(r.file.as_str());
                print_row(&mut out, &r, name)?;
            }
            Err(e) => {
                out.flush()?;
                report(src, &e);
                exit_code = 1;
            }
        }
    }
    if sources.len() > 1 {
        print_row(&mut out, &total, Some("total"))?;
    }
    ignore_broken_pipe(out.flush())?;
    Ok(exit_code)
}

// ---------------------------------------------------------------------------
// 6. sort — GNU key/ordering semantics + parallel merge sort + NDJSON `--by <field>`
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "sort",
    about = "Sort lines of text files or NDJSON object streams",
    disable_help_flag = true
)]
struct SortArgs {
    /// Print help
    #[arg(long = "help", action = ArgAction::Help)]
    help: Option<bool>,

    /// Ignore leading blanks
    #[arg(short = 'b', long = "ignore-leading-blanks")]
    ignore_blanks: bool,

    /// Consider only blanks and alphanumeric characters
    #[arg(short = 'd', long = "dictionary-order")]
    dictionary: bool,

    /// Fold lower case to upper case characters
    #[arg(short = 'f', long = "ignore-case")]
    ignore_case: bool,

    /// Compare according to general numerical value
    #[arg(short = 'g', long = "general-numeric-sort")]
    general_numeric: bool,

    /// Compare human readable numbers (e.g., 2K, 1G)
    #[arg(short = 'h', long = "human-numeric-sort")]
    human_numeric: bool,

    /// Consider only printable characters
    #[arg(short = 'i', long = "ignore-nonprinting")]
    ignore_nonprinting: bool,

    /// Compare (unknown) < 'JAN' < ... < 'DEC'
    #[arg(short = 'M', long = "month-sort")]
    month: bool,

    /// Compare according to string numerical value
    #[arg(short = 'n', long = "numeric-sort")]
    numeric: bool,

    /// Shuffle, but group identical keys
    #[arg(short = 'R', long = "random-sort")]
    random: bool,

    /// Get random bytes from FILE
    #[arg(long = "random-source", value_name = "FILE")]
    random_source: Option<String>,

    /// Sort according to WORD: general-numeric -g, human-numeric -h, month -M,
    /// numeric -n, random -R, version -V
    #[arg(long = "sort", value_name = "WORD", action = ArgAction::Append)]
    sort_words: Vec<String>,

    /// Natural sort of (version) numbers within text
    #[arg(short = 'V', long = "version-sort")]
    version: bool,

    /// Reverse the result of comparisons
    #[arg(short = 'r', long = "reverse", visible_alias = "desc")]
    reverse: bool,

    /// Check for sorted input; do not sort
    #[arg(short = 'c', long = "check")]
    check: bool,

    /// Sort via a key; KEYDEF gives location and type (F[.C][OPTS][,F[.C][OPTS]])
    #[arg(short = 'k', long = "key", action = ArgAction::Append)]
    keys: Vec<String>,

    /// Write result to FILE instead of standard output
    #[arg(short = 'o', long = "output")]
    output: Option<String>,

    /// Stabilize sort by disabling last-resort comparison
    #[arg(short = 's', long = "stable")]
    stable: bool,

    /// Use SEP instead of non-blank to blank transition
    #[arg(short = 't', long = "field-separator")]
    separator: Option<String>,

    /// Output only the first of an equal run
    #[arg(short = 'u', long = "unique")]
    unique: bool,

    /// Modern Dogfish extension: Sort NDJSON objects by named field (e.g., `--by size`)
    #[arg(long = "by")]
    by_field: Option<String>,

    /// Filter NDJSON objects before sorting (e.g., `--where "size>10K"`)
    #[arg(short = 'w', long = "where")]
    where_expr: Option<String>,

    /// Emit JSON / NDJSON output
    #[arg(short = 'J', long = "json")]
    json: bool,

    files: Vec<String>,
}

#[derive(Clone, Copy, Default)]
struct SortOpts {
    skip_sblanks: bool,
    skip_eblanks: bool,
    dictionary: bool,
    fold: bool,
    general: bool,
    human: bool,
    nonprinting: bool,
    month: bool,
    numeric: bool,
    random: bool,
    reverse: bool,
    version: bool,
}

impl SortOpts {
    /// GNU `default_key_compare`: no ordering option at all (reverse excluded).
    fn is_default(&self) -> bool {
        !(self.skip_sblanks
            || self.skip_eblanks
            || self.dictionary
            || self.fold
            || self.general
            || self.human
            || self.nonprinting
            || self.month
            || self.numeric
            || self.random
            || self.version)
    }

    /// True when `-d`, `-i` or `-f` rewrite the key before it is compared.
    fn filters(&self) -> bool {
        self.dictionary || self.nonprinting || self.fold
    }

    /// GNU `key_to_opts`: the key's option letters, in GNU's canonical order.
    fn letters(&self) -> String {
        [
            (self.skip_sblanks || self.skip_eblanks, 'b'),
            (self.dictionary, 'd'),
            (self.fold, 'f'),
            (self.general, 'g'),
            (self.human, 'h'),
            (self.nonprinting, 'i'),
            (self.month, 'M'),
            (self.numeric, 'n'),
            (self.random, 'R'),
            (self.reverse, 'r'),
            (self.version, 'V'),
        ]
        .iter()
        .filter_map(|&(on, c)| on.then_some(c))
        .collect()
    }

    /// GNU `check_ordering_compatibility`: at most one of `-n`, `-g`, `-h`, `-M`
    /// and the string orderings (`-V`, `-R`, `-d`, `-i`, which combine freely).
    fn is_compatible(&self) -> bool {
        let string_like = self.version || self.random || self.dictionary || self.nonprinting;
        [
            self.numeric,
            self.general,
            self.human,
            self.month,
            string_like,
        ]
        .iter()
        .filter(|&&on| on)
        .count()
            <= 1
    }
}

#[derive(Clone, Copy)]
struct SortKey {
    /// Zero-based start field and character offset.
    sword: usize,
    schar: usize,
    /// Zero-based end field (`None` = end of line) and 1-based char (0 = end of field).
    eword: Option<usize>,
    echar: usize,
    opts: SortOpts,
}

fn parse_sort_key(spec: &str) -> std::result::Result<SortKey, String> {
    let invalid = |what: &str| format!("{}: invalid field specification '{}'", what, spec);
    let b = spec.as_bytes();
    let mut i = 0;
    let number = |i: &mut usize| -> Option<usize> {
        let st = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        (st != *i).then(|| spec[st..*i].parse().unwrap_or(usize::MAX))
    };
    let mut opts = SortOpts::default();
    let parse_opts = |i: &mut usize, start: bool, opts: &mut SortOpts| -> Result<(), String> {
        while *i < b.len() && b[*i] != b',' {
            match b[*i] {
                b'b' if start => opts.skip_sblanks = true,
                b'b' => opts.skip_eblanks = true,
                b'd' => opts.dictionary = true,
                b'f' => opts.fold = true,
                b'g' => opts.general = true,
                b'h' => opts.human = true,
                b'i' => opts.nonprinting = true,
                b'M' => opts.month = true,
                b'n' => opts.numeric = true,
                b'R' => opts.random = true,
                b'r' => opts.reverse = true,
                b'V' => opts.version = true,
                _ => return Err(invalid("stray character in field spec")),
            }
            *i += 1;
        }
        Ok(())
    };

    let sfield = number(&mut i).ok_or_else(|| invalid("invalid number at field start"))?;
    if sfield == 0 {
        return Err(invalid("field number is zero"));
    }
    let mut schar = 1;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        schar = number(&mut i).ok_or_else(|| invalid("invalid number after '.'"))?;
        if schar == 0 {
            return Err(invalid("character offset is zero"));
        }
    }
    parse_opts(&mut i, true, &mut opts)?;
    let mut eword = None;
    let mut echar = 0;
    if i < b.len() && b[i] == b',' {
        i += 1;
        let efield = number(&mut i).ok_or_else(|| invalid("invalid number after ','"))?;
        if efield == 0 {
            return Err(invalid("field number is zero"));
        }
        eword = Some(efield - 1);
        if i < b.len() && b[i] == b'.' {
            i += 1;
            echar = number(&mut i).ok_or_else(|| invalid("invalid number after '.'"))?;
        }
        parse_opts(&mut i, false, &mut opts)?;
    }
    Ok(SortKey {
        sword: sfield - 1,
        schar: schar - 1,
        eword,
        echar,
        opts,
    })
}

struct Sorter {
    keys: Vec<SortKey>,
    tab: Option<u8>,
    /// Disable the last-resort whole-line comparison (`-s` / `-u`).
    no_last_resort: bool,
    reverse: bool,
    /// MD5 state primed with the `-R` seed; each key is hashed with a clone.
    random_seed: Md5,
}

impl Sorter {
    fn begfield(&self, line: &[u8], key: &SortKey) -> usize {
        let lim = line.len();
        let mut p = 0;
        let mut sword = key.sword;
        if let Some(t) = self.tab {
            while p < lim && sword > 0 {
                sword -= 1;
                while p < lim && line[p] != t {
                    p += 1;
                }
                if p < lim {
                    p += 1;
                }
            }
        } else {
            while p < lim && sword > 0 {
                sword -= 1;
                while p < lim && is_blank(line[p]) {
                    p += 1;
                }
                while p < lim && !is_blank(line[p]) {
                    p += 1;
                }
            }
        }
        if key.opts.skip_sblanks {
            while p < lim && is_blank(line[p]) {
                p += 1;
            }
        }
        (p + key.schar).min(lim)
    }

    fn limfield(&self, line: &[u8], key: &SortKey) -> usize {
        let lim = line.len();
        let Some(mut eword) = key.eword else {
            return lim;
        };
        let echar = key.echar;
        if echar == 0 {
            eword += 1;
        }
        let mut p = 0;
        if let Some(t) = self.tab {
            while p < lim && eword > 0 {
                eword -= 1;
                while p < lim && line[p] != t {
                    p += 1;
                }
                if p < lim && (eword > 0 || echar > 0) {
                    p += 1;
                }
            }
        } else {
            while p < lim && eword > 0 {
                eword -= 1;
                while p < lim && is_blank(line[p]) {
                    p += 1;
                }
                while p < lim && !is_blank(line[p]) {
                    p += 1;
                }
            }
        }
        if echar != 0 {
            if key.opts.skip_eblanks {
                while p < lim && is_blank(line[p]) {
                    p += 1;
                }
            }
            p = (p + echar).min(lim);
        }
        p
    }

    fn key_slice<'a>(&self, line: &'a [u8], key: &SortKey) -> &'a [u8] {
        let beg = self.begfield(line, key);
        let end = self.limfield(line, key).max(beg);
        &line[beg..end]
    }

    fn keycompare(&self, a: &[u8], b: &[u8]) -> Ordering {
        for key in &self.keys {
            let ka = self.key_slice(a, key);
            let kb = self.key_slice(b, key);
            let o = &key.opts;
            let ord = if o.numeric || o.general || o.human || o.month || o.random || o.version {
                // As in GNU, these orderings see the key after `-d`/`-i`/`-f` rewriting.
                let (ka, kb): (Cow<[u8]>, Cow<[u8]>) = if o.filters() {
                    (
                        key_filter(ka, o).collect::<Vec<u8>>().into(),
                        key_filter(kb, o).collect::<Vec<u8>>().into(),
                    )
                } else {
                    (ka.into(), kb.into())
                };
                if o.numeric {
                    numeric_compare(&ka, &kb)
                } else if o.general {
                    general_numeric_compare(&ka, &kb)
                } else if o.human {
                    human_unit_order(&ka)
                        .cmp(&human_unit_order(&kb))
                        .then_with(|| numeric_compare(&ka, &kb))
                } else if o.month {
                    month_of(&ka).cmp(&month_of(&kb))
                } else if o.random {
                    self.random_compare(&ka, &kb)
                } else {
                    filevercmp(&ka, &kb)
                }
            } else if o.filters() {
                key_filter(ka, o).cmp(key_filter(kb, o))
            } else {
                ka.cmp(kb)
            };
            if ord != Ordering::Equal {
                return if o.reverse { ord.reverse() } else { ord };
            }
        }
        Ordering::Equal
    }

    /// GNU `compare_random`: order by the seeded MD5 of each key, falling back
    /// to the bytes themselves so that only identical keys compare equal.
    fn random_compare(&self, a: &[u8], b: &[u8]) -> Ordering {
        let digest = |key: &[u8]| self.random_seed.clone().chain_update(key).finalize();
        digest(a).cmp(&digest(b)).then_with(|| a.cmp(b))
    }

    fn compare(&self, a: &[u8], b: &[u8]) -> Ordering {
        if !self.keys.is_empty() {
            let ord = self.keycompare(a, b);
            if ord != Ordering::Equal || self.no_last_resort {
                return ord;
            }
        }
        let ord = a.cmp(b);
        if self.reverse { ord.reverse() } else { ord }
    }
}

struct SortNum<'a> {
    neg: bool,
    int: &'a [u8],
    frac: &'a [u8],
}

/// Parses the leading `[blanks][-]digits[.digits]` of a key, like GNU `strnumcmp`.
fn parse_sort_num(s: &[u8]) -> SortNum<'_> {
    let mut i = 0;
    while i < s.len() && is_blank(s[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && s[i] == b'-' {
        neg = true;
        i += 1;
    }
    let st = i;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    let mut int = &s[st..i];
    while let [b'0', rest @ ..] = int {
        int = rest;
    }
    let mut frac: &[u8] = &[];
    if i < s.len() && s[i] == b'.' {
        i += 1;
        let fs = i;
        while i < s.len() && s[i].is_ascii_digit() {
            i += 1;
        }
        frac = &s[fs..i];
        while let [rest @ .., b'0'] = frac {
            frac = rest;
        }
    }
    if int.is_empty() && frac.is_empty() {
        neg = false; // -0 == 0, and non-numbers are zero
    }
    SortNum { neg, int, frac }
}

fn numeric_compare(a: &[u8], b: &[u8]) -> Ordering {
    let (x, y) = (parse_sort_num(a), parse_sort_num(b));
    let mag = |p: &SortNum, q: &SortNum| {
        p.int
            .len()
            .cmp(&q.int.len())
            .then_with(|| p.int.cmp(q.int))
            .then_with(|| p.frac.cmp(q.frac))
    };
    match (x.neg, y.neg) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        (false, false) => mag(&x, &y),
        (true, true) => mag(&y, &x),
    }
}

/// GNU `find_unit_order`: signed SI-suffix magnitude of a human-readable number.
fn human_unit_order(s: &[u8]) -> i32 {
    let mut i = 0;
    while i < s.len() && is_blank(s[i]) {
        i += 1;
    }
    let neg = i < s.len() && s[i] == b'-';
    if neg {
        i += 1;
    }
    let mut max_digit = b'0';
    let mut seen_dot = false;
    while i < s.len() && (s[i].is_ascii_digit() || (s[i] == b'.' && !seen_dot)) {
        if s[i] == b'.' {
            seen_dot = true;
        } else {
            max_digit = max_digit.max(s[i]);
        }
        i += 1;
    }
    if max_digit == b'0' {
        return 0;
    }
    let order = match s.get(i) {
        Some(b'K') | Some(b'k') => 1,
        Some(b'M') => 2,
        Some(b'G') => 3,
        Some(b'T') => 4,
        Some(b'P') => 5,
        Some(b'E') => 6,
        Some(b'Z') => 7,
        Some(b'Y') => 8,
        Some(b'R') => 9,
        Some(b'Q') => 10,
        _ => 0,
    };
    if neg { -order } else { order }
}

/// strtod-style prefix parse for `-g`.
fn parse_general_num(s: &[u8]) -> Option<f64> {
    let mut i = 0;
    while i < s.len() && s[i].is_ascii_whitespace() {
        i += 1;
    }
    let st = i;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        i += 1;
    }
    let rest = &s[i..];
    let lower: Vec<u8> = rest
        .iter()
        .take(8)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if lower.starts_with(b"inf") || lower.starts_with(b"nan") {
        let neg = s[st] == b'-';
        return Some(match (&lower[..3], neg) {
            (b"inf", false) => f64::INFINITY,
            (b"inf", true) => f64::NEG_INFINITY,
            _ => f64::NAN,
        });
    }
    // Hexadecimal (strtod accepts `0x1F`, `0x1.8p3`).
    if lower.starts_with(b"0x")
        && lower
            .get(2)
            .is_some_and(|c| c.is_ascii_hexdigit() || *c == b'.')
    {
        let neg = s[st] == b'-';
        let mut j = i + 2;
        let mut v = 0f64;
        let mut any = false;
        while j < s.len() && s[j].is_ascii_hexdigit() {
            v = v * 16.0 + (s[j] as char).to_digit(16).unwrap() as f64;
            j += 1;
            any = true;
        }
        if j < s.len() && s[j] == b'.' {
            j += 1;
            let mut scale = 1.0 / 16.0;
            while j < s.len() && s[j].is_ascii_hexdigit() {
                v += (s[j] as char).to_digit(16).unwrap() as f64 * scale;
                scale /= 16.0;
                j += 1;
                any = true;
            }
        }
        if any {
            if j < s.len() && (s[j] == b'p' || s[j] == b'P') {
                let mut k = j + 1;
                let eneg = k < s.len() && s[k] == b'-';
                if k < s.len() && (s[k] == b'+' || s[k] == b'-') {
                    k += 1;
                }
                let es = k;
                while k < s.len() && s[k].is_ascii_digit() {
                    k += 1;
                }
                if k > es {
                    let e: i32 = std::str::from_utf8(&s[es..k])
                        .ok()?
                        .parse()
                        .unwrap_or(i32::MAX);
                    v *= 2f64.powi(if eneg { -e } else { e });
                }
            }
            return Some(if neg { -v } else { v });
        }
    }
    let digits_start = i;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    let mut has_digits = i > digits_start;
    if i < s.len() && s[i] == b'.' {
        i += 1;
        let fs = i;
        while i < s.len() && s[i].is_ascii_digit() {
            i += 1;
        }
        has_digits |= i > fs;
    }
    if !has_digits {
        return None;
    }
    if i < s.len() && (s[i] == b'e' || s[i] == b'E') {
        let mut j = i + 1;
        if j < s.len() && (s[j] == b'+' || s[j] == b'-') {
            j += 1;
        }
        let es = j;
        while j < s.len() && s[j].is_ascii_digit() {
            j += 1;
        }
        if j > es {
            i = j;
        }
    }
    std::str::from_utf8(&s[st..i]).ok()?.parse().ok()
}

fn general_numeric_compare(a: &[u8], b: &[u8]) -> Ordering {
    match (parse_general_num(a), parse_general_num(b)) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(x), Some(y)) => match x.partial_cmp(&y) {
            Some(o) => o,
            None => x.is_nan().cmp(&y.is_nan()).reverse(),
        },
    }
}

/// The bytes of a key after `-d`/`-i` deletion and `-f` folding.
fn key_filter<'a>(key: &'a [u8], o: &SortOpts) -> impl Iterator<Item = u8> + 'a {
    let SortOpts {
        dictionary,
        nonprinting,
        fold,
        ..
    } = *o;
    key.iter()
        .copied()
        .filter(move |&c| {
            if dictionary {
                c.is_ascii_alphanumeric() || is_blank(c)
            } else if nonprinting {
                (0x20..0x7f).contains(&c)
            } else {
                true
            }
        })
        .map(move |c| if fold { c.to_ascii_uppercase() } else { c })
}

/// GNU `getmonth`: 0 for unknown, else 1..=12 for a key that starts (after
/// blanks) with a case-insensitive `JAN`..`DEC`.
fn month_of(key: &[u8]) -> u8 {
    const MONTHS: [&[u8]; 12] = [
        b"JAN", b"FEB", b"MAR", b"APR", b"MAY", b"JUN", b"JUL", b"AUG", b"SEP", b"OCT", b"NOV",
        b"DEC",
    ];
    let start = key.iter().position(|&c| !is_blank(c)).unwrap_or(key.len());
    let Some(abbr) = key[start..].get(..3) else {
        return 0;
    };
    MONTHS
        .iter()
        .position(|m| abbr.eq_ignore_ascii_case(m))
        .map_or(0, |i| i as u8 + 1)
}

/// gnulib `file_prefixlen`: the length of `s` without its file suffix, the
/// longest tail matching `(\.[A-Za-z~][A-Za-z0-9~]*)*$` that leaves at least
/// one leading byte.
fn file_prefix_len(s: &[u8]) -> usize {
    let n = s.len();
    let mut prefix = 0;
    let mut i = 0;
    while i < n {
        i += 1;
        prefix = i;
        while i + 1 < n && s[i] == b'.' && (s[i + 1].is_ascii_alphabetic() || s[i + 1] == b'~') {
            i += 2;
            while i < n && (s[i].is_ascii_alphanumeric() || s[i] == b'~') {
                i += 1;
            }
        }
    }
    prefix
}

/// gnulib `order`: the version-sort weight of the byte at `pos`. `~` sorts
/// first, then the end of the string, then letters, then all other bytes.
fn version_weight(s: &[u8], pos: usize) -> i32 {
    match s.get(pos) {
        None => -1,
        Some(b'~') => -2,
        Some(c) if c.is_ascii_digit() => 0,
        Some(c) if c.is_ascii_alphabetic() => i32::from(*c),
        Some(&c) => i32::from(c) + 256,
    }
}

/// gnulib `verrevcmp`: Debian-style comparison of alternating non-digit and
/// digit runs, the latter compared numerically.
fn verrevcmp(a: &[u8], b: &[u8]) -> Ordering {
    let digit_at = |s: &[u8], i: usize| s.get(i).is_some_and(u8::is_ascii_digit);
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        while (i < a.len() && !digit_at(a, i)) || (j < b.len() && !digit_at(b, j)) {
            let ord = version_weight(a, i).cmp(&version_weight(b, j));
            if ord != Ordering::Equal {
                return ord;
            }
            i += 1;
            j += 1;
        }
        while a.get(i) == Some(&b'0') {
            i += 1;
        }
        while b.get(j) == Some(&b'0') {
            j += 1;
        }
        let mut first_diff = Ordering::Equal;
        while digit_at(a, i) && digit_at(b, j) {
            first_diff = first_diff.then(a[i].cmp(&b[j]));
            i += 1;
            j += 1;
        }
        if digit_at(a, i) {
            return Ordering::Greater;
        }
        if digit_at(b, j) {
            return Ordering::Less;
        }
        if first_diff != Ordering::Equal {
            return first_diff;
        }
    }
    Ordering::Equal
}

/// gnulib `filenvercmp`, the `-V` ordering: empty first, then `.`, `..` and
/// other dot-files, then a version comparison that ignores file suffixes
/// (`.tar.gz`) unless they are needed to break a tie.
fn filevercmp(a: &[u8], b: &[u8]) -> Ordering {
    match (a, b) {
        ([], []) => return Ordering::Equal,
        ([], _) => return Ordering::Less,
        (_, []) => return Ordering::Greater,
        ([b'.', ..], [b'.', ..]) => {
            for special in [&b"."[..], b".."] {
                match (a == special, b == special) {
                    (true, true) => return Ordering::Equal,
                    (true, false) => return Ordering::Less,
                    (false, true) => return Ordering::Greater,
                    (false, false) => {}
                }
            }
        }
        ([b'.', ..], _) => return Ordering::Less,
        (_, [b'.', ..]) => return Ordering::Greater,
        _ => {}
    }
    let (pa, pb) = (file_prefix_len(a), file_prefix_len(b));
    let ord = verrevcmp(&a[..pa], &b[..pb]);
    if ord != Ordering::Equal || (pa == a.len() && pb == b.len()) {
        ord
    } else {
        verrevcmp(a, b)
    }
}

/// Turns on the ordering a `--sort=WORD` names.
type SetOrdering = fn(&mut SortOpts);

/// Resolves a `--sort=WORD` argument the way GNU `argmatch` does: an exact
/// name or an unambiguous prefix.
fn parse_sort_word(word: &str) -> std::result::Result<SetOrdering, String> {
    const WORDS: [(&str, SetOrdering); 6] = [
        ("general-numeric", |o| o.general = true),
        ("human-numeric", |o| o.human = true),
        ("month", |o| o.month = true),
        ("numeric", |o| o.numeric = true),
        ("random", |o| o.random = true),
        ("version", |o| o.version = true),
    ];
    if let Some(&(_, set)) = WORDS.iter().find(|(name, _)| *name == word) {
        return Ok(set);
    }
    let mut matches = WORDS.iter().filter(|(name, _)| name.starts_with(word));
    let problem = match (matches.next(), matches.next()) {
        (Some(&(_, set)), None) => return Ok(set),
        (None, _) => "invalid",
        (Some(_), Some(_)) => "ambiguous",
    };
    let valid: String = WORDS
        .iter()
        .map(|(name, _)| format!("\n  - '{name}'"))
        .collect();
    Err(format!(
        "{problem} argument '{word}' for '--sort'\nValid arguments are:{valid}\nTry 'sort --help' for more information."
    ))
}

/// The 16 bytes GNU sort mixes into every `-R` hash: the start of
/// `--random-source=FILE`, or fresh randomness.
fn sort_random_seed(source: Option<&str>) -> std::result::Result<[u8; 16], String> {
    let mut seed = [0u8; 16];
    match source {
        Some(path) => {
            let mut file = std::fs::File::open(path)
                .map_err(|e| format!("open failed: {}: {}", path, gnu_io_error(&e)))?;
            file.read_exact(&mut seed).map_err(|e| match e.kind() {
                io::ErrorKind::UnexpectedEof => format!("'{path}': end of file"),
                _ => format!("'{}': read error: {}", path, gnu_io_error(&e)),
            })?;
        }
        None => {
            // std's `RandomState` keys are drawn from the OS entropy source.
            let state = RandomState::new();
            for (i, chunk) in seed.as_chunks_mut::<8>().0.iter_mut().enumerate() {
                *chunk = state.hash_one(i).to_ne_bytes();
            }
        }
    }
    Ok(seed)
}

pub fn run_sort(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = SortArgs::try_parse_from(args)?;
    let _ = parsed.help;

    let mut global = SortOpts {
        skip_sblanks: parsed.ignore_blanks,
        skip_eblanks: parsed.ignore_blanks,
        dictionary: parsed.dictionary,
        fold: parsed.ignore_case,
        general: parsed.general_numeric,
        human: parsed.human_numeric,
        nonprinting: parsed.ignore_nonprinting,
        month: parsed.month,
        numeric: parsed.numeric,
        random: parsed.random,
        reverse: parsed.reverse,
        version: parsed.version,
    };
    for word in &parsed.sort_words {
        match parse_sort_word(word) {
            Ok(set) => set(&mut global),
            Err(msg) => {
                eprintln!("sort: {msg}");
                return Ok(1);
            }
        }
    }
    let tab = match parsed.separator.as_deref() {
        None => None,
        Some("\\0") => Some(0),
        Some("") => {
            eprintln!("sort: empty tab");
            return Ok(2);
        }
        Some(s) if s.len() == 1 => Some(s.as_bytes()[0]),
        Some(s) => {
            eprintln!("sort: multi-character tab '{}'", s);
            return Ok(2);
        }
    };
    let mut keys = Vec::with_capacity(parsed.keys.len());
    for spec in &parsed.keys {
        match parse_sort_key(spec) {
            Ok(mut k) => {
                // Keys without ordering options inherit the global ones (GNU semantics).
                if k.opts.is_default() && !k.opts.reverse {
                    k.opts = global;
                }
                keys.push(k);
            }
            Err(msg) => {
                eprintln!("sort: {}", msg);
                return Ok(2);
            }
        }
    }
    if keys.is_empty() && !global.is_default() {
        keys.push(SortKey {
            sword: 0,
            schar: 0,
            eword: None,
            echar: 0,
            opts: global,
        });
    }
    if let Some(key) = keys.iter().find(|k| !k.opts.is_compatible()) {
        eprintln!("sort: options '-{}' are incompatible", key.opts.letters());
        return Ok(2);
    }
    let mut random_seed = Md5::new();
    if keys.iter().any(|k| k.opts.random) {
        match sort_random_seed(parsed.random_source.as_deref()) {
            Ok(seed) => random_seed.update(seed),
            Err(msg) => {
                eprintln!("sort: {msg}");
                return Ok(2);
            }
        }
    }
    let sorter = Sorter {
        keys,
        tab,
        no_last_resort: parsed.unique || parsed.stable,
        reverse: parsed.reverse,
        random_seed,
    };

    let sources = InputSource::from_args(&parsed.files);
    let mut bufs: Vec<Vec<u8>> = Vec::with_capacity(sources.len());
    for src in &sources {
        match FastReader::open(src).and_then(|r| r.into_bytes()) {
            Ok(b) => {
                ctx.telemetry.add_read(b.len() as u64);
                bufs.push(b);
            }
            Err(e) => {
                eprintln!(
                    "sort: cannot read: {}: {}",
                    src.display_name(),
                    gnu_io_error(&e)
                );
                return Ok(2);
            }
        }
    }

    // Check if input is an NDJSON object stream!
    if !parsed.check
        && bufs
            .iter()
            .find(|b| !b.is_empty())
            .is_some_and(|b| looks_like_json(b))
    {
        let all_lines: Vec<String> = bufs.iter().flat_map(|b| lossy_lines(b)).collect();
        if let Some(mut objs) = read_ndjson_stdin(&all_lines) {
            ctx.telemetry
                .set_strategy("NDJSON field-aware parallel sort");
            if let Some(ref expr) = parsed.where_expr {
                objs.retain(|o| eval_json_where(o, expr));
            }
            let field = parsed
                .by_field
                .clone()
                .unwrap_or_else(|| "name".to_string());
            objs.par_sort_by(|a, b| {
                let ord = compare_json_field(a, b, &field);
                if parsed.reverse { ord.reverse() } else { ord }
            });
            if parsed.unique {
                objs.dedup_by(|a, b| compare_json_field(a, b, &field) == Ordering::Equal);
            }
            ctx.telemetry.add_items(objs.len() as u64);
            emit_objects_or_table(&objs, ctx, parsed.json, !parsed.json)?;
            return Ok(0);
        }
    }

    let mut lines: Vec<&[u8]> = bufs.iter().flat_map(|b| split_lines(b)).collect();

    if parsed.check {
        let name = sources[0].display_name();
        for (idx, pair) in lines.windows(2).enumerate() {
            let ord = sorter.compare(pair[0], pair[1]);
            if ord == Ordering::Greater || (parsed.unique && ord == Ordering::Equal) {
                eprintln!(
                    "sort: {}:{}: disorder: {}",
                    name,
                    idx + 2,
                    String::from_utf8_lossy(pair[1])
                );
                return Ok(1);
            }
        }
        return Ok(0);
    }

    if sorter.keys.is_empty() {
        ctx.telemetry
            .set_strategy("zero-alloc &[u8] slices + rayon par_sort_unstable");
        if parsed.reverse {
            lines.par_sort_unstable_by(|a, b| b.cmp(a));
        } else {
            lines.par_sort_unstable();
        }
    } else {
        ctx.telemetry
            .set_strategy("zero-alloc &[u8] slices + rayon stable par_sort_by (GNU keys)");
        lines.par_sort_by(|a, b| sorter.compare(a, b));
    }
    if parsed.unique {
        lines.dedup_by(|cur, prev| sorter.compare(prev, cur) == Ordering::Equal);
    }

    let sink: Box<dyn Write> = match parsed.output.as_deref() {
        Some(path) if path != "-" => match std::fs::File::create(path) {
            Ok(f) => Box::new(f),
            Err(e) => {
                eprintln!("sort: open failed: {}: {}", path, gnu_io_error(&e));
                return Ok(2);
            }
        },
        _ => Box::new(io::stdout().lock()),
    };
    let mut out = io::BufWriter::with_capacity(256 * 1024, sink);
    let mut written = 0u64;
    for s in &lines {
        ignore_broken_pipe(out.write_all(s))?;
        ignore_broken_pipe(out.write_all(b"\n"))?;
        written += s.len() as u64 + 1;
    }
    ctx.telemetry.add_items(lines.len() as u64);
    ctx.telemetry.add_written(written);
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// 7. uniq — Deduplicate lines or NDJSON objects (`--by <field>`)
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "uniq",
    about = "Report or omit repeated lines or NDJSON objects"
)]
struct UniqArgs {
    /// Prefix lines by the number of occurrences
    #[arg(short = 'c', long = "count")]
    count: bool,

    /// Only print duplicate lines, one for each group
    #[arg(short = 'd', long = "repeated")]
    repeated: bool,

    /// Avoid comparing the first N fields
    #[arg(short = 'f', long = "skip-fields", default_value_t = 0)]
    skip_fields: usize,

    /// Ignore differences in case when comparing
    #[arg(short = 'i', long = "ignore-case")]
    ignore_case: bool,

    /// Avoid comparing the first N characters
    #[arg(short = 's', long = "skip-chars", default_value_t = 0)]
    skip_chars: usize,

    /// Only print unique lines
    #[arg(short = 'u', long = "unique")]
    unique: bool,

    /// Compare no more than N characters in lines
    #[arg(short = 'w', long = "check-chars")]
    check_chars: Option<usize>,

    /// Deduplicate NDJSON objects by named field
    #[arg(long = "by")]
    by_field: Option<String>,

    #[arg(short = 'J', long = "json")]
    json: bool,

    input: Option<String>,

    output: Option<String>,
}

fn uniq_key<'a>(line: &'a [u8], p: &UniqArgs) -> &'a [u8] {
    let mut i = 0;
    for _ in 0..p.skip_fields {
        while i < line.len() && is_blank(line[i]) {
            i += 1;
        }
        while i < line.len() && !is_blank(line[i]) {
            i += 1;
        }
    }
    let start = (i + p.skip_chars).min(line.len());
    let end = match p.check_chars {
        Some(n) => (start + n).min(line.len()),
        None => line.len(),
    };
    &line[start..end]
}

pub fn run_uniq(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = UniqArgs::try_parse_from(args)?;
    let src = match parsed.input {
        Some(ref f) if f != "-" => InputSource::File(f.clone()),
        _ => InputSource::Stdin,
    };

    let mut reader = match FastReader::open(&src) {
        Ok(r) => r.into_bufread(),
        Err(e) => {
            eprintln!("uniq: {}: {}", src.display_name(), gnu_io_error(&e));
            return Ok(1);
        }
    };
    let mut first_line = Vec::new();
    if reader.read_until(b'\n', &mut first_line)? == 0 {
        return Ok(0);
    }

    if looks_like_json(&first_line) {
        let mut all = first_line.clone();
        reader.read_to_end(&mut all)?;
        if let Some(objs) = read_ndjson_stdin(&lossy_lines(&all)) {
            ctx.telemetry.set_strategy("NDJSON object deduplication");
            let field = parsed.by_field.as_deref().unwrap_or("name");
            let mut groups: Vec<(Value, usize)> = Vec::new();
            for obj in objs {
                if let Some((last, cnt)) = groups.last_mut()
                    && compare_json_field(last, &obj, field) == Ordering::Equal
                {
                    *cnt += 1;
                    continue;
                }
                groups.push((obj, 1));
            }
            let filtered: Vec<Value> = groups
                .into_iter()
                .filter(|(_, c)| {
                    if parsed.repeated {
                        *c > 1
                    } else if parsed.unique {
                        *c == 1
                    } else {
                        true
                    }
                })
                .map(|(mut val, c)| {
                    if parsed.count
                        && let Some(map) = val.as_object_mut()
                    {
                        map.insert("count".to_string(), serde_json::json!(c));
                    }
                    val
                })
                .collect();
            emit_objects_or_table(&filtered, ctx, parsed.json, !parsed.json)?;
            return Ok(0);
        }
        // Not NDJSON after all: dedupe the buffered text.
        reader = Box::new(io::Cursor::new(all));
        first_line.clear();
        reader.read_until(b'\n', &mut first_line)?;
    }

    // O(1) memory single-pass streaming deduplicator!
    ctx.telemetry
        .set_strategy("O(1) memory streaming deduplicator");
    let sink: Box<dyn Write> = match parsed.output.as_deref() {
        Some(path) if path != "-" => Box::new(std::fs::File::create(path)?),
        _ => Box::new(io::stdout().lock()),
    };
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, sink);
    let mut prev = chomp(&first_line).to_vec();
    let mut count = 1usize;
    let mut cur = Vec::with_capacity(256);

    let emit_group =
        |out: &mut io::BufWriter<Box<dyn Write>>, line: &[u8], cnt: usize| -> io::Result<()> {
            if (parsed.repeated && cnt <= 1) || (parsed.unique && cnt != 1) {
                return Ok(());
            }
            if parsed.count {
                ignore_broken_pipe(write!(out, "{:7} ", cnt))?;
            }
            ignore_broken_pipe(out.write_all(line))?;
            ignore_broken_pipe(out.write_all(b"\n"))
        };

    loop {
        cur.clear();
        if reader.read_until(b'\n', &mut cur)? == 0 {
            break;
        }
        let line = chomp(&cur);
        let (ka, kb) = (uniq_key(&prev, &parsed), uniq_key(line, &parsed));
        let is_same = if parsed.ignore_case {
            ka.eq_ignore_ascii_case(kb)
        } else {
            ka == kb
        };
        if is_same {
            count += 1;
        } else {
            emit_group(&mut out, &prev, count)?;
            prev.clear();
            prev.extend_from_slice(line);
            count = 1;
        }
    }
    emit_group(&mut out, &prev, count)?;
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// 8. cut — Supports byte/char/field slicing AND NDJSON `--field name,size` projection!
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "cut",
    about = "Remove sections from each line of files or project fields from NDJSON"
)]
struct CutArgs {
    /// Select only these bytes (e.g. 1-5, 2,4)
    #[arg(short = 'b', long = "bytes", allow_hyphen_values = true)]
    bytes: Option<String>,

    /// Select only these characters (bytes, as in GNU coreutils)
    #[arg(short = 'c', long = "characters", allow_hyphen_values = true)]
    chars: Option<String>,

    /// (ignored)
    #[arg(short = 'n')]
    no_split: bool,

    /// Select only these fields (1-indexed numbers or NDJSON field names like `name,size`)
    #[arg(
        short = 'f',
        long = "fields",
        visible_alias = "field",
        allow_hyphen_values = true
    )]
    fields: Option<String>,

    /// Use DELIM instead of TAB for field delimiter
    #[arg(short = 'd', long = "delimiter", allow_hyphen_values = true)]
    delimiter: Option<String>,

    /// Complement the set of selected bytes, characters or fields
    #[arg(long = "complement")]
    complement: bool,

    /// Do not print lines not containing delimiters
    #[arg(short = 's', long = "only-delimited")]
    only_delimited: bool,

    /// Output delimiter (defaults to input delimiter)
    #[arg(long = "output-delimiter")]
    output_delimiter: Option<String>,

    /// Filter NDJSON objects before projecting (e.g., `--where "size>1K"`)
    #[arg(short = 'w', long = "where")]
    where_expr: Option<String>,

    #[arg(short = 'J', long = "json")]
    json: bool,

    files: Vec<String>,
}

/// `-b` and `-c` are the same byte selection (as in GNU coreutils, which has
/// no multibyte `cut`); `-f` selects delimited fields.
#[derive(Clone, Copy, PartialEq)]
enum CutMode {
    Bytes,
    Fields,
}

struct CutSpec {
    mode: CutMode,
    /// Sorted, disjoint 1-based selected ranges (already complemented).
    ranges: Vec<(usize, usize)>,
    delim: Vec<u8>,
    out_delim: Vec<u8>,
    /// `--output-delimiter` was given: separate byte ranges with it.
    delimit_bytes: bool,
    only_delimited: bool,
}

impl CutSpec {
    fn selected(&self, pos: usize) -> bool {
        let idx = self.ranges.partition_point(|&(_, end)| end < pos);
        self.ranges.get(idx).is_some_and(|&(start, _)| start <= pos)
    }
}

/// Parses a GNU list (`1,3`, `2-5`, `-3`, `4-`) into sorted 1-based ranges,
/// merging only overlapping ones like GNU, so that `1-2,3-4` stays two ranges
/// for `--output-delimiter`.
fn parse_cut_list(spec: &str, mode: CutMode) -> std::result::Result<Vec<(usize, usize)>, String> {
    let what = if mode == CutMode::Fields {
        "fields are"
    } else {
        "byte/character positions are"
    };
    let mut ranges = Vec::new();
    for part in spec.split([',', ' ']).filter(|p| !p.is_empty()) {
        let num = |s: &str| -> std::result::Result<usize, String> {
            let n: usize = s.parse().map_err(|_| {
                format!(
                    "invalid {} value '{}'",
                    if mode == CutMode::Fields {
                        "field"
                    } else {
                        "byte/character position"
                    },
                    part
                )
            })?;
            if n == 0 {
                return Err(format!("{} numbered from 1", what));
            }
            Ok(n)
        };
        let (start, end) = match part.split_once('-') {
            Some(("", "")) => return Err("invalid range with no endpoint: -".to_string()),
            Some((s, e)) => (
                if s.is_empty() { 1 } else { num(s)? },
                if e.is_empty() { usize::MAX } else { num(e)? },
            ),
            None => {
                let n = num(part)?;
                (n, n)
            }
        };
        if start > end {
            return Err("invalid decreasing range".to_string());
        }
        ranges.push((start, end));
    }
    if ranges.is_empty() {
        return Err(format!("{} numbered from 1", what));
    }
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (s, e) in ranges {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    Ok(merged)
}

/// GNU `complement_rp`: the positions not covered by sorted, disjoint `ranges`.
fn complement_cut_ranges(ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(ranges.len() + 1);
    let mut next = 1;
    for &(start, end) in ranges {
        if start > next {
            out.push((next, start - 1));
        }
        next = end.saturating_add(1);
    }
    if ranges.last().is_some_and(|&(_, end)| end < usize::MAX) {
        out.push((next, usize::MAX));
    }
    out
}

fn cut_line<W: Write>(line: &[u8], spec: &CutSpec, out: &mut W) -> io::Result<()> {
    match spec.mode {
        CutMode::Bytes => {
            let mut printed = false;
            for &(start, end) in &spec.ranges {
                if start > line.len() {
                    break;
                }
                if printed && spec.delimit_bytes {
                    out.write_all(&spec.out_delim)?;
                }
                out.write_all(&line[start - 1..end.min(line.len())])?;
                printed = true;
            }
        }
        CutMode::Fields => {
            if memmem::find(line, &spec.delim).is_none() {
                if !spec.only_delimited {
                    out.write_all(line)?;
                    out.write_all(b"\n")?;
                }
                return Ok(());
            }
            let mut first = true;
            let mut rest = line;
            let mut idx = 1;
            loop {
                let (field, next) = match memmem::find(rest, &spec.delim) {
                    Some(p) => (&rest[..p], Some(&rest[p + spec.delim.len()..])),
                    None => (rest, None),
                };
                if spec.selected(idx) {
                    if !first {
                        out.write_all(&spec.out_delim)?;
                    }
                    first = false;
                    out.write_all(field)?;
                }
                match next {
                    Some(n) => rest = n,
                    None => break,
                }
                idx += 1;
            }
        }
    }
    out.write_all(b"\n")
}

fn cut_buffer<W: Write>(bytes: &[u8], spec: &CutSpec, out: &mut W) -> io::Result<()> {
    for line in split_lines(bytes) {
        cut_line(line, spec, out)?;
    }
    Ok(())
}

pub fn run_cut(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = CutArgs::try_parse_from(args)?;
    let sources = InputSource::from_args(&parsed.files);

    let mut all_stdin: Option<Vec<u8>> = None;
    // Check for NDJSON named-field projection (e.g., `dogfish ls -J | dogfish cut -f name,size`)
    if sources.len() == 1 && matches!(sources[0], InputSource::Stdin) && !ctx.stdin_is_tty {
        let mut all = Vec::new();
        io::stdin().lock().read_to_end(&mut all)?;
        if looks_like_json(&all)
            && let Some(mut objs) = read_ndjson_stdin(&lossy_lines(&all))
        {
            if let Some(ref expr) = parsed.where_expr {
                objs.retain(|o| eval_json_where(o, expr));
            }
            if let Some(ref f_spec) = parsed.fields {
                ctx.telemetry.set_strategy("NDJSON field projection");
                let requested: Vec<&str> = f_spec.split(',').map(|s| s.trim()).collect();
                let projected: Vec<Value> = objs
                    .into_iter()
                    .map(|obj| {
                        let mut m = Map::new();
                        if let Some(orig) = obj.as_object() {
                            for &k in &requested {
                                if let Some(v) = orig.get(k) {
                                    m.insert(k.to_string(), v.clone());
                                }
                            }
                        }
                        Value::Object(m)
                    })
                    .collect();
                emit_objects_or_table(&projected, ctx, parsed.json, !parsed.json)?;
                return Ok(0);
            } else if parsed.where_expr.is_some() {
                emit_objects_or_table(&objs, ctx, parsed.json, !parsed.json)?;
                return Ok(0);
            }
        }
        all_stdin = Some(all);
    }

    let usage_error = |msg: &str| {
        eprintln!("cut: {msg}\nTry 'cut --help' for more information.");
        Ok(1)
    };
    let delim = parsed.delimiter.clone().unwrap_or_else(|| "\t".to_string());
    if delim.chars().count() != 1 {
        return usage_error("the delimiter must be a single character");
    }
    let (mode, list) = match (&parsed.bytes, &parsed.chars, &parsed.fields) {
        (Some(l), None, None) | (None, Some(l), None) => (CutMode::Bytes, l),
        (None, None, Some(l)) => (CutMode::Fields, l),
        (None, None, None) => {
            return usage_error("you must specify a list of bytes, characters, or fields");
        }
        _ => return usage_error("only one type of list may be specified"),
    };
    if mode == CutMode::Bytes && parsed.delimiter.is_some() {
        return usage_error("an input delimiter may be specified only when operating on fields");
    }
    if mode == CutMode::Bytes && parsed.only_delimited {
        return usage_error(
            "suppressing non-delimited lines makes sense\n\tonly when operating on fields",
        );
    }
    let mut ranges = match parse_cut_list(list, mode) {
        Ok(r) => r,
        Err(msg) => return usage_error(&msg),
    };
    if parsed.complement {
        ranges = complement_cut_ranges(&ranges);
    }
    let spec = CutSpec {
        mode,
        ranges,
        delimit_bytes: parsed.output_delimiter.is_some(),
        out_delim: match parsed.output_delimiter.as_deref() {
            // GNU treats an empty `--output-delimiter` as a NUL byte.
            Some("") => vec![0],
            Some(d) => d.as_bytes().to_vec(),
            None => delim.as_bytes().to_vec(),
        },
        delim: delim.into_bytes(),
        only_delimited: parsed.only_delimited,
    };

    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());
    let mut exit_code = 0;
    if let Some(all) = all_stdin {
        ignore_broken_pipe(cut_buffer(&all, &spec, &mut out))?;
    } else {
        for src in &sources {
            let res = FastReader::open(src).and_then(|mut r| {
                r.with_bytes(|bytes| cut_buffer(bytes, &spec, &mut out))
                    .and_then(|r| r)
            });
            match res {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(exit_code),
                Err(e) => {
                    let _ = out.flush();
                    eprintln!("cut: {}: {}", src.display_name(), gnu_io_error(&e));
                    exit_code = 1;
                }
            }
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(exit_code)
}

// ---------------------------------------------------------------------------
// 9. tr — Byte-oriented GNU translate / squeeze / delete with classes & escapes
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "tr",
    about = "Translate, squeeze, and/or delete characters from standard input"
)]
struct TrArgs {
    /// Use the complement of SET1
    #[arg(short = 'c', visible_short_alias = 'C', long = "complement")]
    complement: bool,

    /// Delete characters in SET1, do not translate
    #[arg(short = 'd', long = "delete")]
    delete: bool,

    /// Replace each sequence of a repeated character with a single occurrence
    #[arg(short = 's', long = "squeeze-repeats")]
    squeeze: bool,

    /// First truncate SET1 to length of SET2
    #[arg(short = 't', long = "truncate-set1")]
    truncate: bool,

    #[arg(allow_hyphen_values = true)]
    sets: Vec<String>,
}

/// An expanded tr SET: the bytes, plus where a `[c*]` fill repeat goes (SET2 only).
struct TrSet {
    bytes: Vec<u8>,
    fill: Option<(usize, u8)>,
}

/// Parses one (possibly escaped) character at `s[*i]`.
fn tr_char(s: &[u8], i: &mut usize) -> u8 {
    let c = s[*i];
    *i += 1;
    if c != b'\\' || *i >= s.len() {
        return c;
    }
    let e = s[*i];
    *i += 1;
    match e {
        b'a' => 0x07,
        b'b' => 0x08,
        b'f' => 0x0c,
        b'n' => b'\n',
        b'r' => b'\r',
        b't' => b'\t',
        b'v' => 0x0b,
        b'0'..=b'7' => {
            let mut v = (e - b'0') as u32;
            for _ in 0..2 {
                match s.get(*i) {
                    Some(&d @ b'0'..=b'7') if v * 8 + ((d - b'0') as u32) <= 255 => {
                        v = v * 8 + (d - b'0') as u32;
                        *i += 1;
                    }
                    _ => break,
                }
            }
            v as u8
        }
        other => other,
    }
}

fn tr_class(name: &[u8]) -> Option<Vec<u8>> {
    let pred: fn(u8) -> bool = match name {
        b"alnum" => |c| c.is_ascii_alphanumeric(),
        b"alpha" => |c| c.is_ascii_alphabetic(),
        b"blank" => |c| c == b' ' || c == b'\t',
        b"cntrl" => |c| c.is_ascii_control(),
        b"digit" => |c| c.is_ascii_digit(),
        b"graph" => |c| c.is_ascii_graphic(),
        b"lower" => |c| c.is_ascii_lowercase(),
        b"print" => |c| (0x20..0x7f).contains(&c),
        b"punct" => |c| c.is_ascii_punctuation(),
        b"space" => |c| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c),
        b"upper" => |c| c.is_ascii_uppercase(),
        b"xdigit" => |c| c.is_ascii_hexdigit(),
        _ => return None,
    };
    Some((0..=255u8).filter(|&c| pred(c)).collect())
}

fn parse_tr_set(raw: &str) -> std::result::Result<TrSet, String> {
    let s = raw.as_bytes();
    let mut out = TrSet {
        bytes: Vec::new(),
        fill: None,
    };
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'[' && i + 1 < s.len() {
            // [:class:]
            if s[i + 1] == b':'
                && let Some(end) = memmem::find(&s[i + 2..], b":]")
            {
                let name = &s[i + 2..i + 2 + end];
                let bytes = tr_class(name).ok_or_else(|| {
                    format!(
                        "invalid character class '{}'",
                        String::from_utf8_lossy(name)
                    )
                })?;
                out.bytes.extend(bytes);
                i += end + 4;
                continue;
            }
            // [=c=]
            if s[i + 1] == b'=' {
                let mut j = i + 2;
                if j < s.len() {
                    let c = tr_char(s, &mut j);
                    if s[j..].starts_with(b"=]") {
                        out.bytes.push(c);
                        i = j + 2;
                        continue;
                    }
                }
            }
            // [c*n] / [c*]
            let mut j = i + 1;
            let c = tr_char(s, &mut j);
            if j < s.len() && s[j] == b'*' {
                let ns = j + 1;
                let mut k = ns;
                while k < s.len() && s[k].is_ascii_digit() {
                    k += 1;
                }
                if k < s.len() && s[k] == b']' {
                    let digits = &raw[ns..k];
                    let n = if digits.starts_with('0') {
                        usize::from_str_radix(digits, 8).unwrap_or(0)
                    } else {
                        digits.parse().unwrap_or(0)
                    };
                    if n == 0 {
                        out.fill = Some((out.bytes.len(), c));
                    } else {
                        out.bytes.extend(std::iter::repeat_n(c, n));
                    }
                    i = k + 1;
                    continue;
                }
            }
        }
        let c = tr_char(s, &mut i);
        if i + 1 < s.len() && s[i] == b'-' {
            let mut j = i + 1;
            let end = tr_char(s, &mut j);
            if end < c {
                return Err(format!(
                    "range-endpoints of '{}' are in reverse collating sequence order",
                    String::from_utf8_lossy(&s[i - 1..j])
                ));
            }
            out.bytes.extend(c..=end);
            i = j;
            continue;
        }
        out.bytes.push(c);
    }
    Ok(out)
}

pub fn run_tr(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = TrArgs::try_parse_from(args)?;
    let usage_err = |msg: String| {
        eprintln!("tr: {}\nTry 'tr --help' for more information.", msg);
        Ok(1)
    };
    let nsets = parsed.sets.len();
    let translating = !parsed.delete && nsets == 2;
    if nsets == 0 {
        return usage_err("missing operand".to_string());
    }
    if nsets == 1 && !parsed.delete && !parsed.squeeze {
        return usage_err(format!(
            "missing operand after '{}'\nTwo strings must be given when translating.",
            parsed.sets[0]
        ));
    }
    if parsed.delete && !parsed.squeeze && nsets == 2 {
        return usage_err(format!(
            "extra operand '{}'\nOnly one string may be given when deleting without squeezing repeats.",
            parsed.sets[1]
        ));
    }
    if nsets > 2 {
        return usage_err(format!("extra operand '{}'", parsed.sets[2]));
    }

    let set1 = match parse_tr_set(&parsed.sets[0]) {
        Ok(s) => s,
        Err(m) => {
            eprintln!("tr: {}", m);
            return Ok(1);
        }
    };
    let mut s1: Vec<u8> = set1.bytes;
    if parsed.complement {
        let mut member = [false; 256];
        for &b in &s1 {
            member[b as usize] = true;
        }
        s1 = (0..=255u8).filter(|&b| !member[b as usize]).collect();
    }
    let s2: Vec<u8> = match parsed.sets.get(1) {
        None => Vec::new(),
        Some(raw) => match parse_tr_set(raw) {
            Ok(mut set2) => {
                if let Some((pos, c)) = set2.fill {
                    let n = s1.len().saturating_sub(set2.bytes.len());
                    set2.bytes.splice(pos..pos, std::iter::repeat_n(c, n));
                }
                set2.bytes
            }
            Err(m) => {
                eprintln!("tr: {}", m);
                return Ok(1);
            }
        },
    };

    let mut map: [u8; 256] = std::array::from_fn(|i| i as u8);
    let mut delete = [false; 256];
    let mut squeeze = [false; 256];
    if translating {
        let mut s1 = s1.clone();
        let mut s2 = s2.clone();
        if parsed.truncate {
            s1.truncate(s2.len());
        } else if s2.len() < s1.len() {
            let Some(&last) = s2.last() else {
                eprintln!("tr: when not truncating set1, string2 must be non-empty");
                return Ok(1);
            };
            s2.resize(s1.len(), last);
        }
        for (&a, &b) in s1.iter().zip(s2.iter()) {
            map[a as usize] = b;
        }
    }
    if parsed.delete {
        for &b in &s1 {
            delete[b as usize] = true;
        }
    }
    if parsed.squeeze {
        let sq = if nsets == 2 { &s2 } else { &s1 };
        for &b in sq {
            squeeze[b as usize] = true;
        }
    }

    ctx.telemetry.set_strategy("256-byte O(1) LUT stream");
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut buf = vec![0u8; DEFAULT_BUF_SIZE];
    let mut obuf = Vec::with_capacity(DEFAULT_BUF_SIZE);
    let mut last: Option<u8> = None;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        ctx.telemetry.add_read(n as u64);
        obuf.clear();
        for &b in &buf[..n] {
            if delete[b as usize] {
                continue;
            }
            let c = map[b as usize];
            if squeeze[c as usize] && last == Some(c) {
                continue;
            }
            obuf.push(c);
            last = Some(c);
        }
        ignore_broken_pipe(out.write_all(&obuf))?;
        ctx.telemetry.add_written(obuf.len() as u64);
    }
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// 10. paste — Merge lines of files side-by-side or serially
// ---------------------------------------------------------------------------
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

// ---------------------------------------------------------------------------
// 11. tac — Concatenate and print files in reverse (SIMD memrchr_iter)
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "tac",
    about = "Concatenate and print files in reverse (last line first)"
)]
struct TacArgs {
    files: Vec<String>,
}

pub fn run_tac(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = TacArgs::try_parse_from(args)?;
    let sources = InputSource::from_args(&parsed.files);
    ctx.telemetry
        .set_strategy("mmap + memrchr reverse SIMD line scan");

    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());
    let mut exit_code = 0;

    for src in &sources {
        let res = FastReader::open(src).and_then(|mut reader| {
            reader
                .with_bytes(|bytes| -> io::Result<()> {
                    ctx.telemetry.add_read(bytes.len() as u64);
                    // Records end with '\n'; an unterminated final record is
                    // emitted as-is (GNU tac does not add a separator).
                    let mut end = bytes.len();
                    let search_end = if bytes.last() == Some(&b'\n') {
                        end - 1
                    } else {
                        end
                    };
                    for pos in memrchr_iter(b'\n', &bytes[..search_end]) {
                        out.write_all(&bytes[pos + 1..end])?;
                        end = pos + 1;
                    }
                    out.write_all(&bytes[..end])?;
                    ctx.telemetry.add_written(bytes.len() as u64);
                    Ok(())
                })
                .and_then(|r| r)
        });
        match res {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(exit_code),
            Err(e) => {
                let _ = out.flush();
                eprintln!(
                    "tac: failed to open '{}' for reading: {}",
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

// ---------------------------------------------------------------------------
// 12. shuf — Fast Fisher-Yates shuffler for lines, numeric ranges (-i), or NDJSON!
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "shuf",
    about = "Generate random permutations of lines, ranges, or NDJSON objects"
)]
struct ShufArgs {
    /// Treat each ARG as an input line
    #[arg(short = 'e', long = "echo")]
    echo: bool,

    /// Act as if input came from a file containing the range LO-HI
    #[arg(short = 'i', long = "input-range")]
    input_range: Option<String>,

    /// Output at most COUNT lines/objects
    #[arg(short = 'n', long = "head-count")]
    head_count: Option<usize>,

    #[arg(short = 'J', long = "json")]
    json: bool,

    args: Vec<String>,
}

pub fn run_shuf(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = ShufArgs::try_parse_from(args)?;
    ctx.telemetry
        .set_strategy("xorshift64* Fisher-Yates shuffle");

    // Seed from system time + pid
    let mut rng_state = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0xdeadbeef_cafebabe)
        ^ (std::process::id() as u64).wrapping_mul(0x9e3779b97f4a7c15);
    if rng_state == 0 {
        rng_state = 1;
    }
    let mut next_u64 = || -> u64 {
        rng_state ^= rng_state >> 12;
        rng_state ^= rng_state << 25;
        rng_state ^= rng_state >> 27;
        rng_state.wrapping_mul(0x2545F4914F6CDD1D)
    };

    let mut lines: Vec<String> = if parsed.echo {
        parsed.args.clone()
    } else if let Some(ref range_str) = parsed.input_range {
        let (lo_s, hi_s) = range_str
            .split_once('-')
            .ok_or_else(|| anyhow!("shuf: invalid input range '{}'", range_str))?;
        let lo: i64 = lo_s.parse()?;
        let hi: i64 = hi_s.parse()?;
        if lo > hi {
            return Err(anyhow!("shuf: invalid input range '{}'", range_str));
        }
        let mut nums: Vec<i64> = (lo..=hi).collect();
        let len = nums.len();
        let count = parsed.head_count.unwrap_or(len).min(len);
        for i in 0..count {
            let j = i + (next_u64() as usize) % (len - i);
            nums.swap(i, j);
        }
        let stdout = io::stdout();
        let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());
        let mut itoa_buf = itoa::Buffer::new();
        for &n in &nums[..count] {
            ignore_broken_pipe(out.write_all(itoa_buf.format(n).as_bytes()))?;
            ignore_broken_pipe(out.write_all(b"\n"))?;
        }
        ctx.telemetry.add_items(count as u64);
        ignore_broken_pipe(out.flush())?;
        return Ok(0);
    } else {
        let sources = InputSource::from_args(&parsed.args);
        let mut all = Vec::new();
        for s in &sources {
            let r = FastReader::open(s)
                .map_err(|e| anyhow!("shuf: {}: {}", s.display_name(), gnu_io_error(&e)))?
                .into_bufread();
            for l in r.lines() {
                all.push(l?);
            }
        }
        all
    };

    if !parsed.echo
        && parsed.input_range.is_none()
        && let Some(mut objs) = read_ndjson_stdin(&lines)
    {
        let len = objs.len();
        for i in (1..len).rev() {
            let j = (next_u64() as usize) % (i + 1);
            objs.swap(i, j);
        }
        if let Some(n) = parsed.head_count {
            objs.truncate(n);
        }
        ctx.telemetry.add_items(objs.len() as u64);
        emit_objects_or_table(&objs, ctx, parsed.json, !parsed.json)?;
        return Ok(0);
    }

    let len = lines.len();
    let count = parsed.head_count.unwrap_or(len).min(len);
    for i in 0..count {
        let j = i + (next_u64() as usize) % (len - i);
        lines.swap(i, j);
    }

    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());
    for line in &lines[..count] {
        ignore_broken_pipe(writeln!(out, "{}", line))?;
    }
    ctx.telemetry.add_items(count as u64);
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// 13. query / filter — Dedicated NDJSON SQL-like pipeline tool (`where`, `sort`, `select`, `limit`)
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "query",
    visible_alias = "filter",
    about = "Filter, sort, and project NDJSON object streams (`dogfish ls -J | dogfish query -w \"size>1K\" -s size -d -f name,size`)"
)]
struct QueryArgs {
    /// Filter expression (`size>10K`, `kind=file`, `name~rs`, `lines>=100`)
    #[arg(short = 'w', long = "where")]
    where_expr: Vec<String>,

    /// Sort by field name
    #[arg(short = 's', long = "sort", visible_alias = "by")]
    sort_by: Option<String>,

    /// Sort descending
    #[arg(short = 'd', long = "desc", visible_short_alias = 'r')]
    desc: bool,

    /// Comma-separated fields to select/project (`name,size,kind`)
    #[arg(short = 'f', long = "select", visible_alias = "fields")]
    select: Option<String>,

    /// Limit number of output rows
    #[arg(short = 'n', long = "limit")]
    limit: Option<usize>,

    /// Emit raw JSON / NDJSON instead of auto-table on TTY
    #[arg(short = 'J', long = "json")]
    json: bool,
}

pub fn run_query(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = QueryArgs::try_parse_from(args)?;
    ctx.telemetry
        .set_strategy("NDJSON query engine (where + sort + select)");

    let all_lines = read_all_stdin_lines()?;
    let Some(mut objs) = read_ndjson_stdin(&all_lines) else {
        return Err(anyhow!(
            "query: stdin is not an NDJSON object stream. Tip: pass `-J` to the upstream command (e.g. `dogfish ls -J | dogfish query ...`)"
        ));
    };

    for expr in &parsed.where_expr {
        objs.retain(|o| eval_json_where(o, expr));
    }

    if let Some(ref field) = parsed.sort_by {
        objs.par_sort_by(|a, b| {
            let ord = compare_json_field(a, b, field);
            if parsed.desc { ord.reverse() } else { ord }
        });
    }

    if let Some(lim) = parsed.limit {
        objs.truncate(lim);
    }

    if let Some(ref f_spec) = parsed.select {
        let requested: Vec<&str> = f_spec.split(',').map(|s| s.trim()).collect();
        objs = objs
            .into_iter()
            .map(|obj| {
                let mut m = Map::new();
                if let Some(orig) = obj.as_object() {
                    for &k in &requested {
                        if let Some(v) = orig.get(k) {
                            m.insert(k.to_string(), v.clone());
                        }
                    }
                }
                Value::Object(m)
            })
            .collect();
    }

    ctx.telemetry.add_items(objs.len() as u64);
    emit_objects_or_table(&objs, ctx, parsed.json, !parsed.json)?;
    Ok(0)
}
