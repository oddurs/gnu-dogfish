//! tail — SIMD reverse newline scan (`memrchr`), `+N`, `-f` follow + NDJSON support

use std::io::{self, Read, Write};
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
use crate::common::head_tail::{
    header_name, last_lines_start, normalize_legacy_num_arg, parse_gnu_count, to_usize,
};
use crate::common::lines::{looks_like_json, lossy_lines};

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
    OptSpec::both(b'z', "zero-terminated", HasArg::No, TailOpt::Unsupported),
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
        json: false,
        files: Vec::new(),
    };
    let mut opts = Getopt::new(
        normalize_legacy_num_arg(args).into_iter().skip(1).collect(),
        SPECS,
    );
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
                let Some(count) = parse_gnu_count(num_str) else {
                    eprintln!(
                        "tail: invalid number of {}: '{}'",
                        if a.by_bytes { "bytes" } else { "lines" },
                        spec
                    );
                    return Err(1);
                };
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

pub fn run_tail(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_tail_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let _ = parsed.disable_inotify;
    let (by_bytes, from_start, count) = (parsed.by_bytes, parsed.from_start, parsed.count);
    let sources = InputSource::from_args(&parsed.files);

    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout::stdout());

    if sources.len() == 1
        && matches!(sources[0], InputSource::Stdin)
        && !by_bytes
        && !from_start
        && !parsed.follow
        && !parsed.verbose
        && !ctx.stdin_is_tty()
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
            Err(e) if write_failed(&e) => return Ok(1),
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
