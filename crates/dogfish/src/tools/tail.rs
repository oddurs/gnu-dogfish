//! tail — SIMD reverse newline scan (`memrchr`), `+N`, `-f` follow + NDJSON support

use std::io::{self, BufRead, Read, Seek, Write};
use std::thread;
use std::time::Duration;

use std::ffi::OsString;

use anyhow::Result;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, emit_objects_or_table, gnu_io_error,
    io::ignore_broken_pipe, read_ndjson_stdin, stdout,
};
use memchr::memchr_iter;

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, argmatch, getopt_error, lossy, lossy_all, print_help, print_version,
    unsupported,
};
use crate::common::head_tail::{header_name, last_lines_start, parse_count, to_usize};
use crate::common::input::{RawStdout, stdin_file};
use crate::common::lines::{line_end, looks_like_json, lossy_lines};
use crate::common::posixver::posix2_version;
use crate::common::quote::{qa, quote_locale};
use crate::common::xstrtol::{StrtolError, xstrtoumax};

const TAIL_HELP: &str = "\
Output the last part of files (or NDJSON object streams)

Usage: tail [OPTIONS] [FILES]...

Arguments:
  [FILES]...  

Options:
  -n, --lines <[+]NUM>  Output the last NUM lines (default 10); or use -n +NUM to skip NUM-1 lines
                        at the start
  -c, --bytes <[+]NUM>  Output the last NUM bytes; or use -c +NUM to output starting with byte NUM
  -f, --follow          Output appended data as the file grows
  -q, --quiet           [alias: --silent]
  -v, --verbose         
  -z, --zero-terminated Line delimiter is NUL, not newline
  -J, --json            
      --help            Print help
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum TailOpt {
    Bytes,
    Follow,
    FollowWhen,
    Lines,
    DisableInotify,
    PresumeInputPipe,
    Quiet,
    Verbose,
    ZeroTerminated,
    Json,
    Digit(u8),
    /// A GNU option Dogfish does not implement yet.
    Unsupported,
    Help,
    Version,
}

const fn digit(c: u8) -> OptSpec<TailOpt> {
    OptSpec::short(c, HasArg::No, TailOpt::Digit(c))
}

/// GNU's `long_options` and `"c:n:fFqs:vz0123456789"`, plus `-J, --json`.
/// `---disable-inotify` and `---presume-input-pipe` are GNU's hidden test
/// options.
const SPECS: &[OptSpec<TailOpt>] = &[
    OptSpec::both(b'c', "bytes", HasArg::Required, TailOpt::Bytes),
    OptSpec::long("follow", HasArg::Optional, TailOpt::FollowWhen),
    OptSpec::both(b'n', "lines", HasArg::Required, TailOpt::Lines),
    OptSpec::long(
        "max-unchanged-stats",
        HasArg::Required,
        TailOpt::Unsupported,
    ),
    OptSpec::long("-disable-inotify", HasArg::No, TailOpt::DisableInotify),
    OptSpec::long("pid", HasArg::Required, TailOpt::Unsupported),
    OptSpec::long("-presume-input-pipe", HasArg::No, TailOpt::PresumeInputPipe),
    OptSpec::both(b'q', "quiet", HasArg::No, TailOpt::Quiet),
    OptSpec::long("retry", HasArg::No, TailOpt::Unsupported),
    OptSpec::long("silent", HasArg::No, TailOpt::Quiet),
    OptSpec::both(
        b's',
        "sleep-interval",
        HasArg::Required,
        TailOpt::Unsupported,
    ),
    OptSpec::both(b'v', "verbose", HasArg::No, TailOpt::Verbose),
    OptSpec::both(b'z', "zero-terminated", HasArg::No, TailOpt::ZeroTerminated),
    OptSpec::long("help", HasArg::No, TailOpt::Help),
    OptSpec::long("version", HasArg::No, TailOpt::Version),
    OptSpec::both(b'J', "json", HasArg::No, TailOpt::Json).exact(),
    OptSpec::short(b'f', HasArg::No, TailOpt::Follow),
    OptSpec::short(b'F', HasArg::No, TailOpt::Unsupported),
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

/// What `tail` was asked for; as in GNU, the last `-c`/`-n` wins.
struct TailArgs {
    by_bytes: bool,
    from_start: bool,
    count: u64,
    follow: bool,
    /// `---disable-inotify`: follow by polling (all Dogfish does today).
    disable_inotify: bool,
    quiet: bool,
    verbose: bool,
    zero_terminated: bool,
    json: bool,
    files: Vec<String>,
}

/// Parses the command line; `Err` is the exit status after a diagnostic.
fn parse_tail_args(args: Vec<OsString>) -> std::result::Result<TailArgs, i32> {
    let mut a = TailArgs {
        by_bytes: false,
        from_start: false,
        count: 10,
        follow: false,
        disable_inotify: false,
        quiet: false,
        verbose: false,
        zero_terminated: false,
        json: false,
        files: Vec::new(),
    };
    let mut opts = Getopt::new(expand_obsolete(args)?.into_iter().skip(1).collect(), SPECS);
    while let Some(opt) = opts.next() {
        let (o, v) = opt.map_err(|e| getopt_error("tail", &e))?;
        match o {
            TailOpt::Bytes | TailOpt::Lines => {
                // Like GNU, each count is checked as it is met.
                a.by_bytes = o == TailOpt::Bytes;
                let spec = v.as_deref().map(lossy).unwrap_or_default();
                let (from_start, num_str) = match spec.strip_prefix('+') {
                    Some(rest) => (true, rest),
                    None => (false, spec.strip_prefix('-').unwrap_or(&spec)),
                };
                let count = parse_count(num_str, !a.by_bytes).map_err(|msg| {
                    eprintln!("tail: {msg}");
                    1
                })?;
                (a.from_start, a.count) = (from_start, count);
            }
            TailOpt::Follow => a.follow = true,
            TailOpt::FollowWhen => {
                if let Some(how) = v {
                    argmatch("tail", "--follow", how, &[("descriptor", 0), ("name", 1)])?;
                }
                a.follow = true;
            }
            TailOpt::DisableInotify => a.disable_inotify = true,
            TailOpt::PresumeInputPipe => {}
            TailOpt::Quiet => (a.quiet, a.verbose) = (true, false),
            TailOpt::Verbose => (a.quiet, a.verbose) = (false, true),
            TailOpt::ZeroTerminated => a.zero_terminated = true,
            TailOpt::Json => a.json = true,
            TailOpt::Digit(c) => {
                eprintln!("tail: option used in invalid context -- {}", char::from(c));
                return Err(1);
            }
            TailOpt::Unsupported => return Err(unsupported("tail", &opts.last_spelling())),
            TailOpt::Help => return Err(print_help(TAIL_HELP)),
            TailOpt::Version => return Err(print_version("tail")),
        }
    }
    a.files = lossy_all(opts.operands());
    Ok(a)
}

/// Prints GNU's diagnostic for a failed write while copying; false for any
/// other error.
fn write_failed(e: &io::Error) -> bool {
    if !stdout::is_write_error(e) {
        return false;
    }
    stdout::mark_reported();
    eprintln!("tail: error writing 'standard output': {}", gnu_io_error(e));
    true
}

/// GNU's `parse_obsolete_option`: with at most one file operand, a first
/// argument such as `-5`, `+2c`, `-l`, `-3bf` is the whole option list. It
/// is rewritten as the equivalent options; `Err` is the exit status after
/// GNU's `invalid number` diagnostic.
fn expand_obsolete(mut args: Vec<OsString>) -> Result<Vec<OsString>, i32> {
    use std::os::unix::ffi::OsStrExt;
    let argc = args.len();
    let operand_ok = argc == 2
        || (argc == 3 && !(args[2].as_bytes().starts_with(b"-") && args[2].len() > 1))
        || ((3..=4).contains(&argc) && args[2] == "--");
    if !operand_ok {
        return Ok(args);
    }
    let posix_ver = posix2_version();
    let obsolete_usage = posix_ver < 200112;
    let traditional_usage = obsolete_usage || 200809 <= posix_ver;
    let first = args[1].clone();
    let arg = first.as_bytes();
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
                    quote_locale(&first)
                );
                return Err(1);
            }
        }
    };
    let option = if count_lines { "-n" } else { "-c" };
    let sign = if from_start { "+" } else { "" };
    let mut replacement = vec![
        OsString::from(option),
        OsString::from(format!("{sign}{count}")),
    ];
    if forever {
        replacement.push(OsString::from("-f"));
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
            && let Ok(map @ FastReader::Mapped(_)) =
                stdin.try_clone().and_then(FastReader::from_file)
        {
            let _ = stdin.seek(io::SeekFrom::End(0));
            return Ok(map);
        }
    }
    FastReader::open(src)
}

/// `tail -n +N` / `-c +N` of a stream: skips N-1 records (or bytes), then
/// copies the rest. Returns the bytes copied.
fn copy_from_start<R: BufRead + ?Sized, W: Write>(
    r: &mut R,
    by_bytes: bool,
    count: u64,
    eol: u8,
    out: &mut W,
) -> io::Result<u64> {
    let mut skip = count.saturating_sub(1);
    let mut copied = 0;
    loop {
        let buf = match r.fill_buf() {
            Ok(buf) => buf,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if buf.is_empty() {
            return Ok(copied);
        }
        let n = buf.len();
        let mut from = 0;
        if skip > 0 {
            if by_bytes {
                from = to_usize(skip).min(n);
                skip -= from as u64;
            } else {
                from = n;
                for i in memchr_iter(eol, buf) {
                    skip -= 1;
                    if skip == 0 {
                        from = i + 1;
                        break;
                    }
                }
            }
        }
        out.write_all(&buf[from..])?;
        copied += (n - from) as u64;
        r.consume(n);
    }
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

pub fn run_tail(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_tail_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let _ = parsed.disable_inotify;
    let (by_bytes, from_start, count) = (parsed.by_bytes, parsed.from_start, parsed.count);
    let eol = line_end(parsed.zero_terminated);
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
        && !ctx.stdin_is_tty()
        && (ctx.stdout_is_tty() || parsed.json)
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
        let res = if let (FastReader::Buffered(r), true) = (&mut reader, from_start) {
            // From the start, a stream is copied as it is read, so an
            // endless input works (and a write error stops it).
            copy_from_start(r, by_bytes, count, eol, &mut out).map(|n| {
                ctx.telemetry.add_read(n);
                ctx.telemetry.add_written(n);
            })
        } else {
            reader
                .with_bytes(|bytes| {
                    let start = tail_start(bytes, by_bytes, from_start, count, eol);
                    let slice = &bytes[start..];
                    ctx.telemetry.add_read(slice.len() as u64);
                    ctx.telemetry.add_written(slice.len() as u64);
                    out.write_all(slice)
                })
                .and_then(|r| r)
        };
        match res {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(exit_code),
            Err(e) if write_failed(&e) => return Ok(1),
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
