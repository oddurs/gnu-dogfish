//! cat — copy_file_range / large reads + GNU line numbering / squeeze / -v ^X M- notation

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;

use anyhow::Result;
use dogfish_core::{DogfishContext, InputSource, gnu_io_error, stdout};
use memchr::memchr;

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy_all, print_help, print_version,
};
use crate::common::input::{RawStdout, bytes_available, safe_read, stdin_file};
use crate::common::quote::qf;

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
    /// `-E` saw a CR at the end of a read: `^M` if a newline follows, even
    /// in the next file (GNU's `pending_cr`).
    pending_cr: bool,
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
    // GNU checks standard output before reading anything; a closed one
    // fails with EBADF (here it is held by /dev/null).
    // SAFETY: fstat writes into the zeroed buffer it is given.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let fstat_error = if stdout::was_closed() {
        Some(io::Error::from_raw_os_error(libc::EBADF))
    // SAFETY: fstat on descriptor 1 with a properly sized, writable buffer.
    } else if unsafe { libc::fstat(libc::STDOUT_FILENO, &mut st) } < 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };
    if let Some(e) = fstat_error {
        eprintln!("cat: standard output: {}", gnu_io_error(&e));
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
    let mut out = io::BufWriter::with_capacity(IO_SIZE, RawStdout);
    let mut state = CatState {
        newlines: 0,
        line_num: 0,
        pending_cr: false,
    };
    let mut exit_code = 0;
    // GNU refuses to copy a non-empty regular file onto itself.
    // GNU's `input file is output file` check needs standard output's
    // identity when it is a regular file.
    let out_id = (st.st_mode & libc::S_IFMT == libc::S_IFREG)
        .then_some((st.st_dev as u64, st.st_ino as u64));
    let mut buf = vec![0u8; IO_SIZE];

    for src in &sources {
        let res = open_input(src, out_id).and_then(|input| {
            let Some(mut file) = input else {
                let _ = out.flush();
                eprintln!("cat: {}: input file is output file", qf(src.display_name()));
                return Ok(false);
            };
            let mut stdin = stdin_file();
            let input: &mut File = match file {
                Some(ref mut f) => f,
                None => &mut stdin,
            };
            if plain {
                out.flush()?;
                cat_plain(input, out_id.is_some(), &mut buf, ctx)?;
            } else {
                cat_decorated(input, &mut buf, &opts, &mut state, &mut out, ctx)?;
            }
            Ok(true)
        });
        match res {
            Ok(true) => {}
            Ok(false) => exit_code = 1,
            // A failed write ends the run with GNU's `write error`.
            Err(e) if stdout::is_write_error(&e) => return Err(e.into()),
            Err(e) => {
                let _ = out.flush();
                eprintln!("cat: {}: {}", qf(src.display_name()), gnu_io_error(&e));
                exit_code = 1;
            }
        }
    }
    if state.pending_cr {
        out.write_all(b"\r")?;
    }
    out.flush()?;
    Ok(exit_code)
}

/// Read and write size: large enough that syscalls are not the cost.
const IO_SIZE: usize = 128 * 1024;

/// Opens one input: `Some(None)` is standard input, `None` the regular file
/// standard output writes to while unread data remains (GNU's `input file
/// is output file`).
fn open_input(src: &InputSource, out_id: Option<(u64, u64)>) -> io::Result<Option<Option<File>>> {
    let is_output = |file: &mut File| -> io::Result<bool> {
        if out_id.is_none() {
            return Ok(false);
        }
        let meta = file.metadata()?;
        Ok(out_id == Some((meta.dev(), meta.ino()))
            && file.stream_position().unwrap_or(0) < meta.len())
    };
    match src {
        InputSource::Stdin => Ok((!is_output(&mut stdin_file())?).then_some(None)),
        InputSource::File(path) => {
            let mut file = File::open(path)?;
            Ok((!is_output(&mut file)?).then_some(Some(file)))
        }
    }
}

/// Plain copy (GNU's `copy_cat` / `simple_cat`): `copy_file_range` from a
/// regular file to a regular file, else large reads and writes.
fn cat_plain(
    input: &mut File,
    out_is_regular: bool,
    buf: &mut [u8],
    ctx: &DogfishContext,
) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    if out_is_regular && input.metadata().is_ok_and(|m| m.is_file()) {
        let fd = input.as_raw_fd();
        loop {
            // SAFETY: descriptor only: `fd` is owned by the live `input`
            // and 1 is standard output; null offsets use (and advance) the
            // file offsets, so no pointer is read or written.
            let n = unsafe {
                libc::copy_file_range(
                    fd,
                    std::ptr::null_mut(),
                    1,
                    std::ptr::null_mut(),
                    1 << 30,
                    0,
                )
            };
            match n {
                0 => return Ok(()),
                n if n > 0 => {
                    ctx.telemetry.add_read(n as u64);
                    ctx.telemetry.add_written(n as u64);
                }
                // Not supported here (another file system, ...): copy below
                // from the current offsets.
                _ => break,
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = out_is_regular;
    let mut out = RawStdout;
    loop {
        let n = safe_read(input, buf)?;
        if n == 0 {
            return Ok(());
        }
        out.write_all(&buf[..n])?;
        ctx.telemetry.add_read(n as u64);
        ctx.telemetry.add_written(n as u64);
    }
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

/// Decorates one input (GNU's `cat`). When nothing more can be read
/// right away, the output so far is written before the read blocks, as
/// GNU does (so `cat -v` in a pipeline is not held back).
fn cat_decorated<W: Write>(
    input: &mut File,
    buf: &mut [u8],
    o: &CatOpts,
    state: &mut CatState,
    out: &mut W,
    ctx: &DogfishContext,
) -> io::Result<()> {
    let mut obuf: Vec<u8> = Vec::with_capacity(4 * IO_SIZE + 1024);
    let fd = input.as_raw_fd();
    let mut use_fionread = true;
    // Only -n, -b and -s: runs of text are copied as they are.
    let simple = !(o.show_nonprinting || o.show_tabs || o.show_ends);
    loop {
        if use_fionread {
            match bytes_available(fd) {
                Some(0) => {
                    out.write_all(&obuf)?;
                    ctx.telemetry.add_written(obuf.len() as u64);
                    obuf.clear();
                    out.flush()?;
                }
                Some(_) => {}
                None => use_fionread = false,
            }
        }
        let n = safe_read(input, buf)?;
        if n == 0 {
            break;
        }
        ctx.telemetry.add_read(n as u64);
        let data = &buf[..n];
        let mut i = 0;
        while i < n {
            let b = data[i];
            if b == b'\n' {
                i += 1;
                // GNU 9.x `-E`: a CR immediately before LF is shown as ^M.
                if state.pending_cr {
                    state.pending_cr = false;
                    obuf.extend_from_slice(b"^M");
                }
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
                continue;
            }
            if state.newlines >= 0 {
                if o.number {
                    cat_push_number(&mut obuf, state);
                }
                state.newlines = -1;
            }
            let end = memchr(b'\n', &data[i..]).map_or(n, |p| i + p);
            let run = &data[i..end];
            i = end;
            if simple {
                obuf.extend_from_slice(run);
                continue;
            }
            for &c in run {
                if state.pending_cr {
                    state.pending_cr = false;
                    obuf.push(b'\r');
                }
                if c == b'\r' && o.show_ends && !o.show_nonprinting {
                    state.pending_cr = true;
                } else {
                    cat_push_visible(&mut obuf, c, o);
                }
            }
        }
        if obuf.len() >= IO_SIZE {
            out.write_all(&obuf)?;
            ctx.telemetry.add_written(obuf.len() as u64);
            obuf.clear();
        }
    }
    out.write_all(&obuf)?;
    ctx.telemetry.add_written(obuf.len() as u64);
    Ok(())
}
