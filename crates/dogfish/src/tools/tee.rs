//! tee — copy standard input to standard output and files

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};

use anyhow::Result;
use dogfish_core::{DEFAULT_BUF_SIZE, DogfishContext, gnu_io_error, gnu_quote_file, stdout};

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

/// One destination of `tee`; `None` once it has failed and been dropped.
struct TeeOutput {
    name: String,
    sink: Option<Box<dyn Write>>,
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

    let mut status = 0;
    let mut outputs = vec![TeeOutput {
        name: "standard output".to_string(),
        sink: Some(Box::new(stdout::stdout())),
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
                sink: Some(Box::new(f)),
            }),
            Err(e) => {
                eprintln!("tee: {}: {}", gnu_quote_file(path), gnu_io_error(&e));
                status = 1;
            }
        }
    }

    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut buf = vec![0u8; DEFAULT_BUF_SIZE];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("tee: read error: {}", gnu_io_error(&e));
                status = 1;
                break;
            }
        };
        ctx.telemetry.add_read(n as u64);
        for out in &mut outputs {
            let Some(sink) = out.sink.as_mut() else {
                continue;
            };
            // Unbuffered like GNU: every chunk reaches every output at once.
            let res = sink.write_all(&buf[..n]).and_then(|()| sink.flush());
            match res {
                Ok(()) => ctx.telemetry.add_written(n as u64),
                Err(e) => {
                    out.sink = None;
                    // Reported here, not again at exit.
                    if stdout::is_write_error(&e) {
                        stdout::mark_reported();
                    }
                    let pipe = e.kind() == io::ErrorKind::BrokenPipe;
                    let quiet =
                        pipe && matches!(mode, OutputError::WarnNopipe | OutputError::ExitNopipe);
                    if !quiet {
                        eprintln!("tee: {}: {}", gnu_quote_file(&out.name), gnu_io_error(&e));
                        status = 1;
                        if matches!(mode, OutputError::Exit | OutputError::ExitNopipe) {
                            return Ok(1);
                        }
                    }
                }
            }
        }
        if outputs.iter().all(|o| o.sink.is_none()) {
            break;
        }
    }
    Ok(status)
}
