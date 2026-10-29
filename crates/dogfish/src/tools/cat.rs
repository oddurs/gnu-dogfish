//! cat — Zero-copy mmap + GNU line numbering / squeeze / -v ^X M- notation

use std::io::{self, BufRead, Write};

use std::ffi::OsString;

use anyhow::Result;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, gnu_io_error,
    io::ignore_broken_pipe, stdout,
};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy_all, print_help, print_version,
};

const CAT_HELP: &str = "\
Concatenate FILE(s) to standard output

Usage: cat [OPTIONS] [FILES]...

Arguments:
  [FILES]...  Files to concatenate

Options:
  -A, --show-all          Equivalent to -vET
  -b, --number-nonblank   Number nonempty output lines, overrides -n
  -e                      Equivalent to -vE
  -E, --show-ends         Display $ at end of each line
  -n, --number            Number all output lines
  -s, --squeeze-blank     Suppress repeated empty output lines
  -t                      Equivalent to -vT
  -T, --show-tabs         Display TAB characters as ^I
  -u                      (ignored)
  -v, --show-nonprinting  Use ^ and M- notation, except for LFD and TAB
      --help              Print help
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum CatOpt {
    NumberNonblank,
    Number,
    Squeeze,
    ShowNonprinting,
    ShowEnds,
    ShowTabs,
    ShowAll,
    /// `-e`: `-vE`.
    NonprintingEnds,
    /// `-t`: `-vT`.
    NonprintingTabs,
    Unbuffered,
    Help,
    Version,
}

/// GNU's `long_options` and `"benstuvAET"`.
const SPECS: &[OptSpec<CatOpt>] = &[
    OptSpec::both(b'b', "number-nonblank", HasArg::No, CatOpt::NumberNonblank),
    OptSpec::both(b'n', "number", HasArg::No, CatOpt::Number),
    OptSpec::both(b's', "squeeze-blank", HasArg::No, CatOpt::Squeeze),
    OptSpec::both(
        b'v',
        "show-nonprinting",
        HasArg::No,
        CatOpt::ShowNonprinting,
    ),
    OptSpec::both(b'E', "show-ends", HasArg::No, CatOpt::ShowEnds),
    OptSpec::both(b'T', "show-tabs", HasArg::No, CatOpt::ShowTabs),
    OptSpec::both(b'A', "show-all", HasArg::No, CatOpt::ShowAll),
    OptSpec::long("help", HasArg::No, CatOpt::Help),
    OptSpec::long("version", HasArg::No, CatOpt::Version),
    OptSpec::short(b'e', HasArg::No, CatOpt::NonprintingEnds),
    OptSpec::short(b't', HasArg::No, CatOpt::NonprintingTabs),
    OptSpec::short(b'u', HasArg::No, CatOpt::Unbuffered),
];

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

pub fn run_cat(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let mut opts = CatOpts {
        number: false,
        number_nonblank: false,
        squeeze: false,
        show_ends: false,
        show_tabs: false,
        show_nonprinting: false,
    };
    let mut getopt = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    while let Some(opt) = getopt.next() {
        match opt {
            Ok((o, _)) => match o {
                CatOpt::NumberNonblank => {
                    opts.number = true;
                    opts.number_nonblank = true;
                }
                CatOpt::Number => opts.number = true,
                CatOpt::Squeeze => opts.squeeze = true,
                CatOpt::ShowNonprinting => opts.show_nonprinting = true,
                CatOpt::ShowEnds => opts.show_ends = true,
                CatOpt::ShowTabs => opts.show_tabs = true,
                CatOpt::ShowAll => {
                    opts.show_nonprinting = true;
                    opts.show_ends = true;
                    opts.show_tabs = true;
                }
                CatOpt::NonprintingEnds => {
                    opts.show_nonprinting = true;
                    opts.show_ends = true;
                }
                CatOpt::NonprintingTabs => {
                    opts.show_nonprinting = true;
                    opts.show_tabs = true;
                }
                CatOpt::Unbuffered => {}
                CatOpt::Help => return Ok(print_help(CAT_HELP)),
                CatOpt::Version => return Ok(print_version("cat")),
            },
            Err(e) => return Ok(getopt_error("cat", &e)),
        }
    }
    let files = lossy_all(getopt.operands());
    // GNU checks standard output before reading anything.
    // SAFETY: fstat writes into the zeroed buffer it is given.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstat on descriptor 1 with a properly sized, writable buffer.
    if unsafe { libc::fstat(libc::STDOUT_FILENO, &mut st) } < 0 {
        eprintln!(
            "cat: standard output: {}",
            gnu_io_error(&io::Error::last_os_error())
        );
        return Ok(1);
    }
    let plain =
        !(opts.number || opts.squeeze || opts.show_ends || opts.show_tabs || opts.show_nonprinting);

    ctx.telemetry.set_strategy(if plain {
        "zero-copy mmap slice write"
    } else {
        "line-decorated stream"
    });

    let sources = InputSource::from_args(&files);
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout::stdout());
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
            // A failed write ends the run with GNU's `write error`.
            if stdout::is_write_error(&e) {
                return Err(e.into());
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
