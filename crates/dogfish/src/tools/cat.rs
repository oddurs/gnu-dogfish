//! cat — Zero-copy mmap + GNU line numbering / squeeze / -v ^X M- notation

use std::io::{self, BufRead, Write};

use anyhow::Result;
use clap::Parser;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, gnu_io_error, io::ignore_broken_pipe,
};

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
