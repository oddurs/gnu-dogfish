//! tac — Concatenate and print files in reverse: GNU `tac.c` record
//! splitting with `-b`, `-r` and `-s` (SIMD `memrchr` for newlines).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

use anyhow::Result;
use clap::Parser;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, gnu_io_error, io::ignore_broken_pipe,
};
use memchr::memrchr;

use crate::common::gnu_regex::Regex;
use crate::common::input::{RawStdout, close_stdin, stdin_file};
use crate::common::quote::{qa, qf};

#[derive(Parser)]
#[command(
    name = "tac",
    about = "Concatenate and print files in reverse (last line first)"
)]
struct TacArgs {
    /// Attach the separator before instead of after
    #[arg(short = 'b', long = "before")]
    before: bool,

    /// Interpret the separator as a regular expression
    #[arg(short = 'r', long = "regex")]
    regex: bool,

    /// Use STRING as the separator instead of newline
    #[arg(
        short = 's',
        long = "separator",
        value_name = "STRING",
        allow_hyphen_values = true
    )]
    separator: Option<String>,

    files: Vec<String>,
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
fn read_input(src: &InputSource) -> std::result::Result<Vec<u8>, (bool, io::Error)> {
    match src {
        InputSource::Stdin => {
            let mut stdin = stdin_file();
            if stdin.metadata().is_ok_and(|m| m.is_file()) {
                let _ = stdin.seek(SeekFrom::Start(0));
            }
            let mut data = Vec::new();
            stdin.read_to_end(&mut data).map_err(|e| (false, e))?;
            Ok(data)
        }
        InputSource::File(path) => {
            let file = File::open(path).map_err(|e| (true, e))?;
            FastReader::from_file(file)
                .and_then(|r| r.into_bytes())
                .map_err(|e| (false, e))
        }
    }
}

pub fn run_tac(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = TacArgs::try_parse_from(args)?;
    let sep = match (&parsed.separator, parsed.regex) {
        (Some(s), true) if s.is_empty() => {
            eprintln!("tac: separator cannot be empty");
            return Ok(1);
        }
        (Some(s), true) => match Regex::new(s.as_bytes()) {
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
        (Some(s), false) => Separator::Literal(s.as_bytes().to_vec()),
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
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(i32::from(!ok)),
            Err(e) => return Err(e.into()),
        }
    }
    ignore_broken_pipe(out.flush())?;
    if read_stdin && let Err(e) = close_stdin() {
        eprintln!("tac: -: {}", gnu_io_error(&e));
        ok = false;
    }
    Ok(i32::from(!ok))
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
