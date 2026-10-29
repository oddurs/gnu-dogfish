//! tac — Concatenate and print files in reverse: GNU `tac.c` record
//! splitting with `-b`, `-r` and `-s` (SIMD `memrchr` for newlines).

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStringExt;

use anyhow::Result;
use dogfish_core::{DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, gnu_io_error};
use memchr::memrchr;

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy_all, print_help, print_version,
};
use crate::common::gnu_regex::Regex;
use crate::common::input::{RawStdout, close_stdin, stdin_file};
use crate::common::quote::{qa, qf};

const TAC_HELP: &str = "\
Usage: tac [OPTION]... [FILE]...
Write each FILE to standard output, last line first.

With no FILE, or when FILE is -, read standard input.

Mandatory arguments to long options are mandatory for short options too.
  -b, --before             attach the separator before instead of after
  -r, --regex              interpret the separator as a regular expression
  -s, --separator=STRING   use STRING as the separator instead of newline
      --help        display this help and exit
      --version     output version information and exit
";

/// What `tac` was asked to do.
#[derive(Default)]
struct TacArgs {
    before: bool,
    regex: bool,
    /// The separator as given (bytes: it need not be UTF-8).
    separator: Option<Vec<u8>>,
    files: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TacOpt {
    Before,
    Regex,
    Separator,
    Help,
    Version,
}

/// GNU's `longopts` and `"brs:"`.
const SPECS: &[OptSpec<TacOpt>] = &[
    OptSpec::both(b'b', "before", HasArg::No, TacOpt::Before),
    OptSpec::both(b'r', "regex", HasArg::No, TacOpt::Regex),
    OptSpec::both(b's', "separator", HasArg::Required, TacOpt::Separator),
    OptSpec::long("help", HasArg::No, TacOpt::Help),
    OptSpec::long("version", HasArg::No, TacOpt::Version),
];

/// Parses the command line; `Err` is the exit status after `--help`,
/// `--version` or a usage diagnostic.
fn parse_tac_args(args: Vec<OsString>) -> std::result::Result<TacArgs, i32> {
    let mut parsed = TacArgs::default();
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    while let Some(opt) = opts.next() {
        let (o, v) = opt.map_err(|e| getopt_error("tac", &e))?;
        match o {
            TacOpt::Before => parsed.before = true,
            TacOpt::Regex => parsed.regex = true,
            TacOpt::Separator => parsed.separator = v.map(OsString::into_vec),
            TacOpt::Help => return Err(print_help(TAC_HELP)),
            TacOpt::Version => return Err(print_version("tac")),
        }
    }
    parsed.files = lossy_all(opts.operands());
    Ok(parsed)
}

/// How records are separated.
enum Separator {
    /// A fixed string (`-s ''` is a NUL byte).
    Literal(Vec<u8>),
    Regex(Regex),
}

impl Separator {
    /// The last separator that starts before `limit` and lies within
    /// `data[..end]` (`end` is where the unprinted data stops): its start
    /// and length.
    fn rfind(&self, data: &[u8], limit: usize, end: usize) -> Option<(usize, usize)> {
        match self {
            Separator::Literal(sep) if sep.len() == 1 => {
                memrchr(sep[0], &data[..limit.min(end)]).map(|p| (p, 1))
            }
            Separator::Literal(sep) => {
                let last_start = limit.min((end + 1).saturating_sub(sep.len()));
                (0..last_start)
                    .rev()
                    .find(|&p| data[p..].starts_with(sep))
                    .map(|p| (p, sep.len()))
            }
            Separator::Regex(re) => {
                if limit == 0 {
                    return None;
                }
                re.rsearch(&data[..limit], limit - 1)
                    .map(|(s, e)| (s, e - s))
            }
        }
    }
}

/// Writes the records of `data` in reverse order (GNU's `tac_seekable`
/// over an input held in memory). With `before`, a separator starts the
/// record that follows it instead of ending the one before.
fn tac_buffer<W: Write>(data: &[u8], sep: &Separator, before: bool, out: &mut W) -> io::Result<()> {
    let mut past_end = data.len();
    let mut first_time = true;
    // Separators must start before `limit`; matches never overlap.
    let mut limit = match sep {
        Separator::Literal(s) => (past_end + 1).saturating_sub(s.len()),
        Separator::Regex(_) => past_end,
    };
    while let Some((start, len)) = sep.rfind(data, limit, past_end) {
        if before {
            out.write_all(&data[start..past_end])?;
            past_end = start;
        } else {
            let match_end = start + len;
            // A separator at the very end of the input ends the last record.
            if !first_time || match_end != past_end {
                out.write_all(&data[match_end..past_end])?;
            }
            past_end = match_end;
            first_time = false;
        }
        limit = match sep {
            Separator::Literal(_) => (start + 1).saturating_sub(len),
            Separator::Regex(_) => start,
        };
    }
    out.write_all(&data[..past_end])
}

/// Reads one input whole; standard input is read from its start when it
/// is a regular file, as GNU's seekable path does.
/// One input's bytes: a regular file stays mapped (no 100 MB copy).
enum TacInput {
    Mapped(memmap2::Mmap),
    Owned(Vec<u8>),
}

impl std::ops::Deref for TacInput {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            TacInput::Mapped(m) => m,
            TacInput::Owned(v) => v,
        }
    }
}

fn read_input(src: &InputSource) -> std::result::Result<TacInput, (bool, io::Error)> {
    match src {
        InputSource::Stdin => {
            let mut stdin = stdin_file();
            if stdin.metadata().is_ok_and(|m| m.is_file()) {
                let _ = stdin.seek(SeekFrom::Start(0));
            }
            let mut data = Vec::new();
            stdin.read_to_end(&mut data).map_err(|e| (false, e))?;
            Ok(TacInput::Owned(data))
        }
        InputSource::File(path) => {
            let file = File::open(path).map_err(|e| (true, e))?;
            FastReader::from_file(file)
                .and_then(|r| match r {
                    FastReader::Mapped(m) => Ok(TacInput::Mapped(m)),
                    r => r.into_bytes().map(TacInput::Owned),
                })
                .map_err(|e| (false, e))
        }
    }
}

pub fn run_tac(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_tac_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let sep = match (&parsed.separator, parsed.regex) {
        (Some(s), true) if s.is_empty() => {
            eprintln!("tac: separator cannot be empty");
            return Ok(1);
        }
        (Some(s), true) => match Regex::new(s) {
            Ok(re) => Separator::Regex(re),
            Err(msg) => {
                eprintln!("tac: {msg}");
                return Ok(1);
            }
        },
        (None, true) => match Regex::new(b"\n") {
            Ok(re) => Separator::Regex(re),
            Err(msg) => {
                eprintln!("tac: {msg}");
                return Ok(1);
            }
        },
        // `-s ''` separates records with NUL bytes.
        (Some(s), false) if s.is_empty() => Separator::Literal(vec![0]),
        (Some(s), false) => Separator::Literal(s.clone()),
        (None, false) => Separator::Literal(vec![b'\n']),
    };
    let sources = InputSource::from_args(&parsed.files);
    ctx.telemetry
        .set_strategy("mmap + memrchr reverse SIMD line scan");

    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, RawStdout);
    let mut ok = true;
    let mut read_stdin = false;

    for src in &sources {
        read_stdin |= matches!(src, InputSource::Stdin);
        let data = match read_input(src) {
            Ok(data) => data,
            Err((opening, e)) => {
                let _ = out.flush();
                match src {
                    InputSource::File(path) if opening => eprintln!(
                        "tac: failed to open {} for reading: {}",
                        qa(path),
                        gnu_io_error(&e)
                    ),
                    InputSource::File(path) => {
                        eprintln!("tac: {}: read error: {}", qf(path), gnu_io_error(&e))
                    }
                    InputSource::Stdin => eprintln!(
                        "tac: {}: read error: {}",
                        qf("standard input"),
                        gnu_io_error(&e)
                    ),
                }
                ok = false;
                continue;
            }
        };
        ctx.telemetry.add_read(data.len() as u64);
        match tac_buffer(&data, &sep, parsed.before, &mut out) {
            Ok(()) => ctx.telemetry.add_written(data.len() as u64),
            Err(e) => return Err(e.into()),
        }
    }
    out.flush()?;
    if read_stdin && let Err(e) = close_stdin() {
        eprintln!("tac: -: {}", gnu_io_error(&e));
        ok = false;
    }
    Ok(i32::from(!ok))
}

/// tac's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("tac", TAC_HELP, SPECS)
}

#[cfg(test)]
mod tests {
    use super::{Separator, tac_buffer};
    use crate::common::gnu_regex::Regex;

    fn tac(data: &str, sep: Separator, before: bool) -> String {
        let mut out = Vec::new();
        tac_buffer(data.as_bytes(), &sep, before, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn lit(s: &str) -> Separator {
        Separator::Literal(s.as_bytes().to_vec())
    }

    fn re(s: &str) -> Separator {
        Separator::Regex(Regex::new(s.as_bytes()).unwrap())
    }

    #[test]
    fn records() {
        assert_eq!(tac("a\nb", lit("\n"), false), "ba\n");
        assert_eq!(tac("a\nb\n", lit("\n"), false), "b\na\n");
        assert_eq!(tac("\na\nb\nc", lit("\n"), true), "\nc\nb\na");
        assert_eq!(tac("a:b:c:", lit(":"), false), "c:b:a:");
        assert_eq!(tac("1::2::3", lit("::"), false), "32::1::");
        assert_eq!(
            tac("1._2.__3.___4._", re("\\._+"), false),
            "4._3.___2.__1._"
        );
        assert_eq!(tac("._1._2.__3.___4", re("\\._+"), true), ".___4.__3._2._1");
    }
}
