//! tee — copy standard input to standard output and files

use std::fs::OpenOptions;
use std::io::{self, Read, Write};

use anyhow::Result;
use clap::{ArgAction, Parser};
use dogfish_core::{DEFAULT_BUF_SIZE, DogfishContext, gnu_io_error, gnu_quote_file};

use crate::common::getopt::argmatch;

#[derive(Parser)]
#[command(
    name = "tee",
    about = "Copy standard input to each FILE, and also to standard output",
    disable_help_flag = true,
    infer_long_args = true
)]
struct TeeArgs {
    /// Append to the given FILEs, do not overwrite
    #[arg(short = 'a', long = "append")]
    append: bool,

    /// Ignore interrupt signals
    #[arg(short = 'i', long = "ignore-interrupts")]
    ignore_interrupts: bool,

    /// Operate in a more appropriate MODE with pipes (same as --output-error=warn-nopipe)
    #[arg(short = 'p')]
    nopipe: bool,

    /// Set behavior on write error: warn, warn-nopipe, exit, exit-nopipe
    #[arg(
        long = "output-error",
        value_name = "MODE",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "warn-nopipe"
    )]
    output_error: Option<String>,

    /// Display this help and exit
    #[arg(long, action = ArgAction::Help)]
    help: Option<bool>,

    /// Output file paths
    files: Vec<String>,
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

pub fn run_tee(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = TeeArgs::try_parse_from(args)?;
    let mut mode = if parsed.nopipe {
        OutputError::WarnNopipe
    } else {
        OutputError::Sigpipe
    };
    if let Some(m) = &parsed.output_error {
        let modes = ["warn", "warn-nopipe", "exit", "exit-nopipe"];
        mode = match argmatch("tee", "--output-error", m, &modes, 1) {
            Ok(0) => OutputError::Warn,
            Ok(1) => OutputError::WarnNopipe,
            Ok(2) => OutputError::Exit,
            Ok(_) => OutputError::ExitNopipe,
            Err(status) => return Ok(status),
        };
    }
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
        sink: Some(Box::new(io::stdout())),
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
