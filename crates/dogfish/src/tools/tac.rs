//! tac — Concatenate and print files in reverse (SIMD memrchr_iter)

use std::io::{self, Write};

use anyhow::Result;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, gnu_io_error,
    io::ignore_broken_pipe, stdout,
};
use memchr::memrchr_iter;

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, Order, getopt_error, lossy_all, print_help, print_version, unsupported,
};

const TAC_HELP: &str = "\
Concatenate and print files in reverse (last line first)

Usage: tac [FILES]...

Arguments:
  [FILES]...  

Options:
      --help  Print help
";

#[derive(Default)]
struct TacArgs {
    files: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TacArgsOpt {
    /// A GNU option Dogfish does not implement yet.
    Unsupported,
    Help,
    Version,
}

/// GNU's `longopts` and `"brs:"`.
const SPECS: &[OptSpec<TacArgsOpt>] = &[
    OptSpec::both(b'b', "before", HasArg::No, TacArgsOpt::Unsupported),
    OptSpec::both(b'r', "regex", HasArg::No, TacArgsOpt::Unsupported),
    OptSpec::both(b's', "separator", HasArg::Required, TacArgsOpt::Unsupported),
    OptSpec::long("help", HasArg::No, TacArgsOpt::Help),
    OptSpec::long("version", HasArg::No, TacArgsOpt::Version),
];

/// Parses the command line; `Err` is the exit status after `--help`,
/// `--version` or a usage diagnostic.
fn parse_tac_args(args: Vec<std::ffi::OsString>) -> std::result::Result<TacArgs, i32> {
    let mut parsed = TacArgs::default();
    let mut opts = Getopt::with_order(args.into_iter().skip(1).collect(), SPECS, Order::Permute);
    // Every option ends the parse: help, version or an error.
    if let Some(opt) = opts.next() {
        let (o, _) = opt.map_err(|e| getopt_error("tac", &e))?;
        match o {
            TacArgsOpt::Unsupported => return Err(unsupported("tac", &opts.last_spelling())),
            TacArgsOpt::Help => return Err(print_help(TAC_HELP)),
            TacArgsOpt::Version => return Err(print_version("tac")),
        }
    }
    parsed.files = lossy_all(opts.operands());
    Ok(parsed)
}

pub fn run_tac(args: Vec<std::ffi::OsString>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_tac_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let sources = InputSource::from_args(&parsed.files);
    ctx.telemetry
        .set_strategy("mmap + memrchr reverse SIMD line scan");

    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout::stdout());
    let mut exit_code = 0;

    for src in &sources {
        let res = FastReader::open(src).and_then(|mut reader| {
            reader
                .with_bytes(|bytes| -> io::Result<()> {
                    ctx.telemetry.add_read(bytes.len() as u64);
                    // Records end with '\n'; an unterminated final record is
                    // emitted as-is (GNU tac does not add a separator).
                    let mut end = bytes.len();
                    let search_end = if bytes.last() == Some(&b'\n') {
                        end - 1
                    } else {
                        end
                    };
                    for pos in memrchr_iter(b'\n', &bytes[..search_end]) {
                        out.write_all(&bytes[pos + 1..end])?;
                        end = pos + 1;
                    }
                    out.write_all(&bytes[..end])?;
                    ctx.telemetry.add_written(bytes.len() as u64);
                    Ok(())
                })
                .and_then(|r| r)
        });
        match res {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(exit_code),
            // A failed write ends the run with GNU's `write error`.
            Err(e) if stdout::is_write_error(&e) => return Err(e.into()),
            Err(e) => {
                let _ = out.flush();
                eprintln!(
                    "tac: failed to open '{}' for reading: {}",
                    src.display_name(),
                    gnu_io_error(&e)
                );
                exit_code = 1;
            }
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(exit_code)
}
