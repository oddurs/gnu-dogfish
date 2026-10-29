//! paste — GNU `paste.c`: merge lines of files side by side (`-s`
//! serially), byte-exact, with `-z` and GNU's delimiter escapes.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;

use anyhow::Result;
use clap::Parser;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, gnu_io_error, io::ignore_broken_pipe,
};
use memchr::memchr;

use crate::common::input::{RawStdout, close_stdin, stdin_file};
use crate::common::lines::line_end;
use crate::common::quote::{Quoting, qf, quote_bytes_raw};

#[derive(Parser)]
#[command(name = "paste", about = "Merge lines of files")]
struct PasteArgs {
    /// Use characters from LIST instead of TABs
    #[arg(short = 'd', long = "delimiters", allow_hyphen_values = true)]
    delimiters: Option<String>,

    /// Paste one file at a time instead of in parallel
    #[arg(short = 's', long = "serial")]
    serial: bool,

    /// Line delimiter is NUL, not newline
    #[arg(short = 'z', long = "zero-terminated")]
    zero_terminated: bool,

    files: Vec<String>,
}

/// GNU's `collapse_escapes`: the delimiter list, `None` standing for `\0`
/// (no delimiter). `Err` when the list ends in an unescaped backslash.
fn collapse_escapes(s: &[u8]) -> Result<Vec<Option<u8>>, ()> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] != b'\\' {
            out.push(Some(s[i]));
            i += 1;
            continue;
        }
        let Some(&e) = s.get(i + 1) else {
            return Err(());
        };
        out.push(match e {
            b'0' => None,
            b'b' => Some(0x08),
            b'f' => Some(0x0c),
            b'n' => Some(b'\n'),
            b'r' => Some(b'\r'),
            b't' => Some(b'\t'),
            b'v' => Some(0x0b),
            other => Some(other),
        });
        i += 2;
    }
    Ok(out)
}

/// Cycles through the delimiter list.
struct Delims<'a> {
    list: &'a [Option<u8>],
    next: usize,
}

impl Delims<'_> {
    fn take(&mut self) -> Option<u8> {
        let d = self.list[self.next];
        self.next = (self.next + 1) % self.list.len();
        d
    }
}

/// GNU's `paste_serial` for one input held in memory: every line
/// terminator but the last becomes the next delimiter, and the output line
/// is always terminated.
fn paste_serial_one<W: Write>(
    data: &[u8],
    list: &[Option<u8>],
    eol: u8,
    out: &mut W,
) -> io::Result<()> {
    let mut delims = Delims { list, next: 0 };
    let Some((&last, body)) = data.split_last() else {
        return out.write_all(&[eol]);
    };
    let mut rest = body;
    while let Some(p) = memchr(eol, rest) {
        out.write_all(&rest[..p])?;
        if let Some(d) = delims.take() {
            out.write_all(&[d])?;
        }
        rest = &rest[p + 1..];
    }
    out.write_all(rest)?;
    out.write_all(&[last])?;
    if last != eol {
        out.write_all(&[eol])?;
    }
    Ok(())
}

/// One input of a parallel paste; every `-` shares standard input.
enum Stream {
    Stdin,
    File(BufReader<File>),
}

/// GNU's `paste_parallel`: one line from each open input per output line;
/// a finished input contributes an empty field (its delimiter is kept only
/// if a later input still has data).
fn paste_parallel<W: Write>(
    names: &[String],
    streams: &mut [Option<Stream>],
    stdin: &mut BufReader<Box<dyn Read>>,
    list: &[Option<u8>],
    eol: u8,
    out: &mut W,
) -> io::Result<bool> {
    let nfiles = streams.len();
    let mut ok = true;
    let mut files_open = streams.iter().filter(|s| s.is_some()).count();
    let mut line = Vec::new();
    while files_open > 0 {
        let mut somedone = false;
        let mut delims = Delims { list, next: 0 };
        let mut saved: Vec<u8> = Vec::new();
        for i in 0..nfiles {
            if files_open == 0 {
                break;
            }
            line.clear();
            let mut read_err = None;
            if let Some(stream) = &mut streams[i] {
                let reader: &mut dyn BufRead = match stream {
                    Stream::Stdin => stdin,
                    Stream::File(f) => f,
                };
                if let Err(e) = reader.read_until(eol, &mut line) {
                    read_err = Some(e);
                }
            }
            if line.is_empty() {
                // End of input, read error, or an input already closed.
                if streams[i].take().is_some() {
                    if let Some(e) = read_err {
                        out.flush()?;
                        eprintln!("paste: {}: {}", qf(&names[i]), gnu_io_error(&e));
                        ok = false;
                    }
                    files_open -= 1;
                }
                if i + 1 == nfiles {
                    if somedone {
                        out.write_all(&saved)?;
                        saved.clear();
                        out.write_all(&[eol])?;
                    }
                } else if let Some(d) = delims.take() {
                    saved.push(d);
                }
                continue;
            }
            out.write_all(&saved)?;
            saved.clear();
            somedone = true;
            let body = line.strip_suffix(&[eol]).unwrap_or(&line);
            out.write_all(body)?;
            if i + 1 != nfiles {
                if let Some(d) = delims.take() {
                    out.write_all(&[d])?;
                }
            } else {
                out.write_all(&[eol])?;
            }
        }
    }
    Ok(ok)
}

pub fn run_paste(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = PasteArgs::try_parse_from(args)?;
    let eol = line_end(parsed.zero_terminated);
    // GNU: `-d ''` means no delimiter at all.
    let delim_arg = match parsed.delimiters.as_deref() {
        None => "\t",
        Some("") => "\\0",
        Some(d) => d,
    };
    let Ok(list) = collapse_escapes(delim_arg.as_bytes()) else {
        let shown = quote_bytes_raw(delim_arg.as_bytes(), Quoting::CMaybe, b":");
        eprintln!(
            "paste: delimiter list ends with an unescaped backslash: {}",
            String::from_utf8_lossy(&shown)
        );
        return Ok(1);
    };
    let names: Vec<String> = if parsed.files.is_empty() {
        vec!["-".to_string()]
    } else {
        parsed.files.clone()
    };
    let read_stdin = names.iter().any(|n| n == "-");
    ctx.telemetry.set_strategy("byte-exact streaming paste");

    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, RawStdout);
    let mut ok = true;
    let result = if parsed.serial {
        let mut res = Ok(());
        for name in &names {
            let src = InputSource::from_args(std::slice::from_ref(name)).remove(0);
            let data = match &src {
                InputSource::Stdin => {
                    let mut data = Vec::new();
                    stdin_file().read_to_end(&mut data).map(|_| data)
                }
                InputSource::File(_) => FastReader::open(&src).and_then(|r| r.into_bytes()),
            };
            let data = match data {
                Ok(d) => d,
                Err(e) => {
                    let _ = out.flush();
                    eprintln!("paste: {}: {}", qf(name), gnu_io_error(&e));
                    ok = false;
                    continue;
                }
            };
            ctx.telemetry.add_read(data.len() as u64);
            res = paste_serial_one(&data, &list, eol, &mut out);
            if res.is_err() {
                break;
            }
        }
        res
    } else {
        let mut streams = Vec::with_capacity(names.len());
        let mut opened_stdin = false;
        for name in &names {
            if name == "-" {
                streams.push(Some(Stream::Stdin));
                continue;
            }
            match File::open(name) {
                Ok(f) => {
                    opened_stdin |= f.as_raw_fd() == 0;
                    streams.push(Some(Stream::File(BufReader::with_capacity(
                        DEFAULT_BUF_SIZE,
                        f,
                    ))));
                }
                Err(e) => {
                    eprintln!("paste: {}: {}", qf(name), gnu_io_error(&e));
                    return Ok(1);
                }
            }
        }
        if opened_stdin && read_stdin {
            eprintln!("paste: standard input is closed");
            return Ok(1);
        }
        let raw: Box<dyn Read> = Box::new(StdinReader);
        let mut stdin = BufReader::with_capacity(DEFAULT_BUF_SIZE, raw);
        paste_parallel(&names, &mut streams, &mut stdin, &list, eol, &mut out).map(|file_ok| {
            ok &= file_ok;
        })
    };
    match result {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(i32::from(!ok)),
        Err(e) => return Err(e.into()),
    }
    ignore_broken_pipe(out.flush())?;
    if read_stdin && let Err(e) = close_stdin() {
        eprintln!("paste: -: {}", gnu_io_error(&e));
        return Ok(1);
    }
    Ok(i32::from(!ok))
}

/// Reads fd 0 directly, so a closed standard input reports EBADF.
struct StdinReader;

impl Read for StdinReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        stdin_file().read(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::{collapse_escapes, paste_serial_one};

    #[test]
    fn escapes() {
        assert_eq!(
            collapse_escapes(b"a\\0\\n\\x"),
            Ok(vec![Some(b'a'), None, Some(b'\n'), Some(b'x')])
        );
        assert_eq!(collapse_escapes(b"ab\\"), Err(()));
    }

    #[test]
    fn serial() {
        let mut out = Vec::new();
        paste_serial_one(b"a\nb\nc", &[Some(b','), None], b'\n', &mut out).unwrap();
        assert_eq!(out, b"a,bc\n");
        out.clear();
        paste_serial_one(b"", &[Some(b',')], b'\n', &mut out).unwrap();
        assert_eq!(out, b"\n");
    }
}
