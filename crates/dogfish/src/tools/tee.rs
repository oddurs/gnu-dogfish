//! tee — copy standard input to standard output and files

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};

use anyhow::Result;
use dogfish_core::{DEFAULT_BUF_SIZE, DogfishContext, gnu_io_error, gnu_quote_file};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, argmatch, getopt_error, lossy_all, print_help, print_version,
};

const TEE_HELP: &str = "\
Copy standard input to each FILE, and also to standard output

Usage: tee [OPTIONS] [FILES]...

Arguments:
  [FILES]...  Output file paths

Options:
  -a, --append                 Append to the given FILEs, do not overwrite
  -i, --ignore-interrupts      Ignore interrupt signals
  -p                           Operate in a more appropriate MODE with pipes (same as
                               --output-error=warn-nopipe)
      --output-error[=<MODE>]  Set behavior on write error: warn, warn-nopipe, exit, exit-nopipe
      --help                   Display this help and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum TeeOpt {
    Append,
    IgnoreInterrupts,
    NoPipe,
    OutputError,
    Help,
    Version,
}

/// GNU's `long_options` and `"aip"`.
const SPECS: &[OptSpec<TeeOpt>] = &[
    OptSpec::both(b'a', "append", HasArg::No, TeeOpt::Append),
    OptSpec::both(
        b'i',
        "ignore-interrupts",
        HasArg::No,
        TeeOpt::IgnoreInterrupts,
    ),
    OptSpec::long("output-error", HasArg::Optional, TeeOpt::OutputError),
    OptSpec::long("help", HasArg::No, TeeOpt::Help),
    OptSpec::long("version", HasArg::No, TeeOpt::Version),
    OptSpec::short(b'p', HasArg::No, TeeOpt::NoPipe),
];

struct TeeArgs {
    append: bool,
    ignore_interrupts: bool,
    mode: OutputError,
    files: Vec<String>,
}

/// Parses the command line; `Err` is the exit status after `--help`,
/// `--version` or a diagnostic. As in GNU, the last of `-p` and
/// `--output-error` decides the mode.
fn parse_tee_args(args: Vec<OsString>) -> std::result::Result<TeeArgs, i32> {
    let mut p = TeeArgs {
        append: false,
        ignore_interrupts: false,
        mode: OutputError::Sigpipe,
        files: Vec::new(),
    };
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    while let Some(opt) = opts.next() {
        let (o, v) = opt.map_err(|e| getopt_error("tee", &e))?;
        match o {
            TeeOpt::Append => p.append = true,
            TeeOpt::IgnoreInterrupts => p.ignore_interrupts = true,
            TeeOpt::NoPipe => p.mode = OutputError::WarnNopipe,
            TeeOpt::OutputError => {
                p.mode = match v {
                    None => OutputError::WarnNopipe,
                    Some(m) => argmatch(
                        "tee",
                        "--output-error",
                        m,
                        &[
                            ("warn", OutputError::Warn),
                            ("warn-nopipe", OutputError::WarnNopipe),
                            ("exit", OutputError::Exit),
                            ("exit-nopipe", OutputError::ExitNopipe),
                        ],
                    )?,
                };
            }
            TeeOpt::Help => return Err(print_help(TEE_HELP)),
            TeeOpt::Version => return Err(print_version("tee")),
        }
    }
    p.files = lossy_all(opts.operands());
    Ok(p)
}

/// GNU tee's `--output-error` modes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputError {
    /// No option: SIGPIPE kills tee, other errors are diagnosed.
    Sigpipe,
    Warn,
    WarnNopipe,
    Exit,
    ExitNopipe,
}

/// One destination of `tee`; `fd` is `None` once it has failed and been
/// dropped. `_file` keeps a named output open.
struct TeeOutput {
    name: String,
    fd: Option<RawFd>,
    /// Whether to watch this output for a broken pipe while input is idle.
    pollable: bool,
    _file: Option<File>,
}

fn fstat_mode(fd: RawFd) -> Option<(libc::mode_t, u64)> {
    // SAFETY: `st` is a valid out-pointer for fstat.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        // `nlink_t` is narrower than u64 on some targets.
        #[allow(clippy::useless_conversion)]
        (libc::fstat(fd, &mut st) == 0).then(|| (st.st_mode & libc::S_IFMT, u64::from(st.st_nlink)))
    }
}

/// gnulib's `isapipe`: a FIFO with a pipe's link count (1 on Linux).
fn is_pipe(fd: RawFd) -> bool {
    fstat_mode(fd).is_some_and(|(fmt, nlink)| fmt == libc::S_IFIFO && nlink <= 1)
}

/// gnulib's `iopoll_input_ok`: input that is not always ready for reading
/// (anything but a regular file or a block device).
fn input_may_block(fd: RawFd) -> bool {
    !fstat_mode(fd).is_some_and(|(fmt, _)| fmt == libc::S_IFREG || fmt == libc::S_IFBLK)
}

/// What gnulib's `iopoll` saw: input to read, or a broken output pipe.
enum PollEvent {
    Input,
    BrokenOutput,
}

/// Waits until `fdin` is readable (or at EOF) or `fdout` becomes a broken
/// pipe, like gnulib's `iopoll (fdin, fdout, true)`.
fn iopoll(fdin: RawFd, fdout: RawFd) -> io::Result<PollEvent> {
    let mut pfds = [
        libc::pollfd {
            fd: fdin,
            events: libc::POLLIN | libc::POLLRDBAND,
            revents: 0,
        },
        libc::pollfd {
            fd: fdout,
            events: libc::POLLRDBAND,
            revents: 0,
        },
    ];
    loop {
        // SAFETY: `pfds` is a valid array of two pollfd structs.
        if unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) } < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if pfds[0].revents != 0 {
            return Ok(PollEvent::Input);
        }
        if pfds[1].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Ok(PollEvent::BrokenOutput);
        }
    }
}

/// Writes all of `buf` to `fd`; on a non-blocking descriptor, waits for it to
/// become writable instead of failing with EAGAIN (GNU's `fwrite_wait`).
fn write_wait(fd: RawFd, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        // SAFETY: `buf` is valid for `buf.len()` bytes.
        let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
        if n >= 0 {
            buf = &buf[n as usize..];
            continue;
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINTR) => {}
            Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK => {
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                // SAFETY: `pfd` is a valid pollfd.
                while unsafe { libc::poll(&mut pfd, 1, -1) } < 0 {
                    if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                        return Err(e);
                    }
                }
            }
            _ => return Err(e),
        }
    }
    Ok(())
}

pub fn run_tee(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_tee_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let mode = parsed.mode;
    // SAFETY: changing this process's signal dispositions before any I/O.
    unsafe {
        if parsed.ignore_interrupts {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
        }
        if mode != OutputError::Sigpipe {
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        }
    }
    let exits = matches!(mode, OutputError::Exit | OutputError::ExitNopipe);
    let nopipe = matches!(mode, OutputError::WarnNopipe | OutputError::ExitNopipe);
    // Whether to notice a broken output pipe while waiting for input; there
    // is no need when the input is always ready.
    let pipe_check = nopipe && input_may_block(libc::STDIN_FILENO);

    let mut status = 0;
    let mut outputs = vec![TeeOutput {
        name: "standard output".to_string(),
        fd: Some(libc::STDOUT_FILENO),
        pollable: pipe_check && is_pipe(libc::STDOUT_FILENO),
        _file: None,
    }];
    for path in &parsed.files {
        let opened = OpenOptions::new()
            .create(true)
            .write(true)
            .append(parsed.append)
            .truncate(!parsed.append)
            .open(path);
        match opened {
            Ok(f) => outputs.push(TeeOutput {
                name: path.clone(),
                fd: Some(f.as_raw_fd()),
                pollable: pipe_check && is_pipe(f.as_raw_fd()),
                _file: Some(f),
            }),
            Err(e) => {
                eprintln!("tee: {}: {}", gnu_quote_file(path), gnu_io_error(&e));
                if exits {
                    return Ok(1);
                }
                status = 1;
            }
        }
    }

    // GNU's `fail_output`: drops `out` after error `e` (silently for EPIPE in
    // the -nopipe modes) and returns whether tee must exit now.
    let fail_output = |out: &mut TeeOutput, e: &io::Error, status: &mut i32| -> bool {
        out.fd = None;
        if nopipe && e.raw_os_error() == Some(libc::EPIPE) {
            return false;
        }
        eprintln!("tee: {}: {}", gnu_quote_file(&out.name), gnu_io_error(e));
        *status = 1;
        exits
    };

    let mut buf = vec![0u8; DEFAULT_BUF_SIZE];
    while let Some(first) = outputs.iter().position(|o| o.fd.is_some()) {
        if let (true, Some(fd)) = (outputs[first].pollable, outputs[first].fd) {
            match iopoll(libc::STDIN_FILENO, fd) {
                Ok(PollEvent::Input) => {}
                Ok(PollEvent::BrokenOutput) => {
                    let e = io::Error::from_raw_os_error(libc::EPIPE);
                    if fail_output(&mut outputs[first], &e, &mut status) {
                        return Ok(1);
                    }
                    continue;
                }
                Err(e) => {
                    eprintln!("tee: iopoll error: {}", gnu_io_error(&e));
                    status = 1;
                }
            }
        }
        // SAFETY: `buf` is valid for `buf.len()` bytes.
        let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            eprintln!("tee: read error: {}", gnu_io_error(&e));
            status = 1;
            break;
        }
        if n == 0 {
            break;
        }
        let chunk = &buf[..n as usize];
        ctx.telemetry.add_read(chunk.len() as u64);
        for out in &mut outputs {
            let Some(fd) = out.fd else {
                continue;
            };
            // Unbuffered like GNU: every chunk reaches every output at once.
            match write_wait(fd, chunk) {
                Ok(()) => ctx.telemetry.add_written(chunk.len() as u64),
                Err(e) => {
                    if fail_output(out, &e, &mut status) {
                        return Ok(1);
                    }
                }
            }
        }
    }
    Ok(status)
}
