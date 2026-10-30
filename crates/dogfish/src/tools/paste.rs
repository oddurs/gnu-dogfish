//! paste — Merge lines of files side-by-side or serially (a port of GNU's,
//! streaming: output is written as input is read, so `paste /dev/zero`
//! stops at the first write error)

use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, Write};
use std::os::unix::ffi::OsStringExt;

use anyhow::Result;
use dogfish_core::{DogfishContext, FastReader, InputSource, gnu_io_error, stdout};

use crate::common::getopt::{Getopt, HasArg, OptSpec, getopt_error, print_help, print_version};
use crate::common::quote::{Quoting, qf, quote_bytes};

const PASTE_HELP: &str = "\
Merge lines of files

Usage: paste [OPTIONS] [FILES]...

Arguments:
  [FILES]...  

Options:
  -d, --delimiters <DELIMITERS>  Use characters from LIST instead of TABs [default: \"\\t\"]
  -s, --serial                   Paste one file at a time instead of in parallel
      --help                     Print help
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum PasteOpt {
    Serial,
    Delimiters,
    Zero,
    Help,
    Version,
}

/// GNU's `longopts` and `"d:sz"`.
const SPECS: &[OptSpec<PasteOpt>] = &[
    OptSpec::both(b's', "serial", HasArg::No, PasteOpt::Serial),
    OptSpec::both(b'd', "delimiters", HasArg::Required, PasteOpt::Delimiters),
    OptSpec::both(b'z', "zero-terminated", HasArg::No, PasteOpt::Zero),
    OptSpec::long("help", HasArg::No, PasteOpt::Help),
    OptSpec::long("version", HasArg::No, PasteOpt::Version),
];

/// GNU `collapse_escapes`: the delimiter list with `\n`, `\t`, `\\`, ...
/// resolved; `None` stands for `\0`, no delimiter at all. `Err` when the
/// list ends in an unescaped backslash.
fn collapse_escapes(list: &[u8]) -> std::result::Result<Vec<Option<u8>>, ()> {
    let mut out = Vec::with_capacity(list.len());
    let mut it = list.iter();
    while let Some(&c) = it.next() {
        if c != b'\\' {
            out.push(Some(c));
            continue;
        }
        out.push(match it.next() {
            None => return Err(()),
            Some(b'0') => None,
            Some(b'b') => Some(0x08),
            Some(b'f') => Some(0x0c),
            Some(b'n') => Some(b'\n'),
            Some(b'r') => Some(b'\r'),
            Some(b't') => Some(b'\t'),
            Some(b'v') => Some(0x0b),
            Some(&other) => Some(other),
        });
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

fn open(name: &OsStr) -> io::Result<Box<dyn BufRead>> {
    Ok(FastReader::open(&InputSource::from_arg(name))?.into_bufread())
}

/// Copies the rest of the current line of `r` to `out`, without its
/// terminator. Returns whether anything (even just the terminator) was
/// read, and whether the line was terminated.
fn copy_line<W: Write>(r: &mut dyn BufRead, eol: u8, out: &mut W) -> io::Result<(bool, bool)> {
    let mut any = false;
    loop {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            return Ok((any, false));
        }
        any = true;
        match memchr::memchr(eol, buf) {
            Some(i) => {
                out.write_all(&buf[..i])?;
                r.consume(i + 1);
                return Ok((true, true));
            }
            None => {
                let n = buf.len();
                out.write_all(buf)?;
                r.consume(n);
            }
        }
    }
}

/// GNU `paste_parallel`; false when an input failed. `Err` is a write
/// error. As in GNU, a file that cannot be opened ends the run.
fn paste_parallel<W: Write>(
    files: &[OsString],
    delims: &[Option<u8>],
    eol: u8,
    out: &mut W,
) -> io::Result<bool> {
    let mut ok = true;
    // Every `-` reads the one standard input, as GNU's `stdin` stream:
    // `readers[i]` indexes the open inputs in `inputs`.
    let mut inputs: Vec<Box<dyn BufRead>> = Vec::with_capacity(files.len());
    let mut stdin_index = None;
    let mut readers: Vec<Option<usize>> = Vec::with_capacity(files.len());
    for name in files {
        if name == "-"
            && let Some(idx) = stdin_index
        {
            readers.push(Some(idx));
            continue;
        }
        match open(name) {
            Ok(r) => {
                if name == "-" {
                    stdin_index = Some(inputs.len());
                }
                readers.push(Some(inputs.len()));
                inputs.push(r);
            }
            Err(e) => {
                out.flush()?;
                eprintln!("paste: {}: {}", qf(name), gnu_io_error(&e));
                return Ok(false);
            }
        }
    }
    let mut files_open = readers.len();
    let n = readers.len();
    while files_open > 0 {
        let mut somedone = false;
        let mut delim = Delims {
            list: delims,
            next: 0,
        };
        // Delimiters owed to files that ended: written only if data follows.
        let mut saved: Vec<u8> = Vec::new();
        for i in 0..n {
            if files_open == 0 {
                break;
            }
            let mut sometodo = false;
            if let Some(r) = readers[i].map(|idx| &mut inputs[idx]) {
                let peeked = match r.fill_buf() {
                    Ok(b) => !b.is_empty(),
                    Err(e) => {
                        eprintln!("paste: {}: {}", qf(&files[i]), gnu_io_error(&e));
                        ok = false;
                        false
                    }
                };
                if peeked {
                    out.write_all(&saved)?;
                    saved.clear();
                    let res = copy_line(&mut **r, eol, out);
                    match res {
                        Ok((any, _)) => sometodo = any,
                        Err(e) if dogfish_core::stdout::is_write_error(&e) => return Err(e),
                        Err(e) => {
                            eprintln!("paste: {}: {}", qf(&files[i]), gnu_io_error(&e));
                            ok = false;
                            sometodo = true;
                        }
                    }
                }
            }
            if !sometodo {
                if readers[i].take().is_some() {
                    files_open -= 1;
                }
                if i + 1 == n {
                    if somedone {
                        out.write_all(&saved)?;
                        saved.clear();
                        out.write_all(&[eol])?;
                    }
                } else if let Some(d) = delim.take() {
                    saved.push(d);
                }
            } else {
                somedone = true;
                if i + 1 != n {
                    if let Some(d) = delim.take() {
                        out.write_all(&[d])?;
                    }
                } else {
                    // The last line of the last file gets its terminator
                    // even when the input lacks one.
                    out.write_all(&[eol])?;
                }
            }
        }
    }
    Ok(ok)
}

/// GNU `paste_serial`; false when an input failed. `Err` is a write error.
fn paste_serial<W: Write>(
    files: &[OsString],
    delims: &[Option<u8>],
    eol: u8,
    out: &mut W,
) -> io::Result<bool> {
    let mut ok = true;
    for name in files {
        let mut r = match open(name) {
            Ok(r) => r,
            Err(e) => {
                out.flush()?;
                eprintln!("paste: {}: {}", qf(name), gnu_io_error(&e));
                ok = false;
                continue;
            }
        };
        let mut delim = Delims {
            list: delims,
            next: 0,
        };
        // Whether the previous line ended; its terminator becomes a
        // delimiter only if another line follows.
        let mut pending_eol = false;
        loop {
            let (got, terminated) =
                match copy_line_or_peek(&mut *r, eol, out, pending_eol, &mut delim) {
                    Ok(v) => v,
                    Err(e) if dogfish_core::stdout::is_write_error(&e) => return Err(e),
                    Err(e) => {
                        eprintln!("paste: {}: {}", qf(name), gnu_io_error(&e));
                        ok = false;
                        break;
                    }
                };
            if !got {
                break;
            }
            pending_eol = terminated;
            if !terminated {
                break;
            }
        }
        // As in GNU, each file's line ends with the terminator.
        out.write_all(&[eol])?;
    }
    Ok(ok)
}

/// One line of serial paste: before any byte of a following line, the
/// previous line's terminator is replaced by the next delimiter.
fn copy_line_or_peek<W: Write>(
    r: &mut dyn BufRead,
    eol: u8,
    out: &mut W,
    pending_eol: bool,
    delim: &mut Delims<'_>,
) -> io::Result<(bool, bool)> {
    if r.fill_buf()?.is_empty() {
        return Ok((false, false));
    }
    if pending_eol && let Some(d) = delim.take() {
        out.write_all(&[d])?;
    }
    copy_line(r, eol, out)
}

pub fn run_paste(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let mut serial = false;
    let mut eol = b'\n';
    let mut delim_arg = b"\t".to_vec();
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    while let Some(opt) = opts.next() {
        let (o, v) = match opt {
            Ok(o) => o,
            Err(e) => return Ok(getopt_error("paste", &e)),
        };
        match o {
            PasteOpt::Serial => serial = true,
            PasteOpt::Delimiters => {
                let v = v.map(OsString::into_vec).unwrap_or_default();
                delim_arg = if v.is_empty() { b"\\0".to_vec() } else { v };
            }
            PasteOpt::Zero => eol = 0,
            PasteOpt::Help => return Ok(print_help(PASTE_HELP)),
            PasteOpt::Version => return Ok(print_version("paste")),
        }
    }
    let mut files = opts.operands();
    if files.is_empty() {
        files.push(OsString::from("-"));
    }
    let Ok(delims) = collapse_escapes(&delim_arg) else {
        eprintln!(
            "paste: delimiter list ends with an unescaped backslash: {}",
            quote_bytes(&delim_arg, Quoting::CMaybe)
        );
        return Ok(1);
    };
    // An all-`\0` list still cycles; an empty list cannot happen (`-d ''`
    // is `\0`).
    let delims = if delims.is_empty() {
        vec![None]
    } else {
        delims
    };
    ctx.telemetry.set_strategy("streaming GNU paste");
    let mut out = io::BufWriter::new(stdout::stdout());
    let ok = if serial {
        paste_serial(&files, &delims, eol, &mut out)?
    } else {
        paste_parallel(&files, &delims, eol, &mut out)?
    };
    out.flush()?;
    Ok(if ok { 0 } else { 1 })
}

/// paste's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("paste", PASTE_HELP, SPECS)
}
