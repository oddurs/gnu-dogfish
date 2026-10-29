//! yes — Page-aligned 64 KiB vector I/O

use std::ffi::OsString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;

use anyhow::Result;
use dogfish_core::{DEFAULT_BUF_SIZE, DogfishContext, gnu_io_error, stdout};

use crate::common::getopt::{Getopt, HasArg, OptSpec, getopt_error, print_help, print_version};

const YES_HELP: &str = "\
Repeatedly output a line with all specified STRING(s), or 'y'

Usage: yes [STRINGS]...

Arguments:
  [STRINGS]...  String(s) to repeat

Options:
      --help  Display this help and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum YesOpt {
    Help,
    Version,
}

/// gnulib's `parse_gnu_standard_options_only`: `--help` and `--version`
/// anywhere (abbreviable), any other option an error.
const SPECS: &[OptSpec<YesOpt>] = &[
    OptSpec::long("help", HasArg::No, YesOpt::Help),
    OptSpec::long("version", HasArg::No, YesOpt::Version),
];

pub fn run_yes(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    // Every option ends the parse: help, version or an error.
    if let Some(opt) = opts.next() {
        return Ok(match opt {
            Ok((YesOpt::Help, _)) => print_help(YES_HELP),
            Ok((YesOpt::Version, _)) => print_version("yes"),
            Err(e) => getopt_error("yes", &e),
        });
    }
    let operands = opts.operands();
    let mut line = Vec::new();
    for (i, s) in operands.iter().enumerate() {
        if i > 0 {
            line.push(b' ');
        }
        line.extend_from_slice(s.as_bytes());
    }
    if operands.is_empty() {
        line.push(b'y');
    }
    line.push(b'\n');

    ctx.telemetry.set_strategy("64KiB page-aligned buffer");
    let mut buf = Vec::with_capacity(DEFAULT_BUF_SIZE + line.len());
    while buf.len() < DEFAULT_BUF_SIZE {
        buf.extend_from_slice(&line);
    }

    // Repeat until a write fails, then fail with GNU's single diagnostic.
    let mut out = stdout::stdout();
    loop {
        match out.write_all(&buf) {
            Ok(()) => ctx.telemetry.add_written(buf.len() as u64),
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(0),
            Err(e) => {
                stdout::mark_reported();
                eprintln!("yes: standard output: {}", gnu_io_error(&e));
                return Ok(1);
            }
        }
    }
}

/// yes's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("yes", YES_HELP, SPECS)
}
