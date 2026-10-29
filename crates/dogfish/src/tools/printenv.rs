//! printenv — Print values of all or specified environment variables

use std::ffi::OsString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;

use anyhow::Result;
use dogfish_core::{DogfishContext, stdout};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, Order, getopt_error, lossy_all, print_help, print_version, try_help,
};

const PRINTENV_HELP: &str = "\
Print the values of the specified environment VARIABLE(s). If no VARIABLE is specified, print name
and value pairs for them all.

Usage: printenv [OPTIONS] [VARS]...

Arguments:
  [VARS]...  

Options:
  -0, --null  End each output line with NUL, not newline
  -J, --json  
      --help  Display this help and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum PrintenvOpt {
    Null,
    Json,
    /// `-i` and `-u NAME`: in GNU's option string (shared with `env`), but a
    /// usage error.
    EnvOnly,
    Help,
    Version,
}

/// GNU's `longopts` and `"+iu:0"`, plus the Dogfish `-J, --json`.
const SPECS: &[OptSpec<PrintenvOpt>] = &[
    OptSpec::both(b'0', "null", HasArg::No, PrintenvOpt::Null),
    OptSpec::long("help", HasArg::No, PrintenvOpt::Help),
    OptSpec::long("version", HasArg::No, PrintenvOpt::Version),
    OptSpec::both(b'J', "json", HasArg::No, PrintenvOpt::Json).exact(),
    OptSpec::short(b'i', HasArg::No, PrintenvOpt::EnvOnly),
    OptSpec::short(b'u', HasArg::Required, PrintenvOpt::EnvOnly),
];

struct PrintenvArgs {
    null: bool,
    json: bool,
    vars: Vec<String>,
}

/// Parses the command line; `Err` is the exit status after `--help`,
/// `--version` or a usage diagnostic.
fn parse_printenv_args(args: Vec<OsString>) -> std::result::Result<PrintenvArgs, i32> {
    let mut p = PrintenvArgs {
        null: false,
        json: false,
        vars: Vec::new(),
    };
    let mut opts = Getopt::with_order(
        args.into_iter().skip(1).collect(),
        SPECS,
        Order::StopAtOperand,
    );
    while let Some(opt) = opts.next() {
        match opt.map_err(|e| getopt_error("printenv", &e))?.0 {
            PrintenvOpt::Null => p.null = true,
            PrintenvOpt::Json => p.json = true,
            PrintenvOpt::EnvOnly => return Err(try_help("printenv")),
            PrintenvOpt::Help => return Err(print_help(PRINTENV_HELP)),
            PrintenvOpt::Version => return Err(print_version("printenv")),
        }
    }
    p.vars = lossy_all(opts.operands());
    Ok(p)
}

pub fn run_printenv(args: Vec<OsString>, _ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_printenv_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let term: &[u8] = if parsed.null { b"\0" } else { b"\n" };
    let mut out = io::BufWriter::new(stdout::stdout());

    if parsed.vars.is_empty() {
        if parsed.json {
            let map: std::collections::BTreeMap<String, String> = std::env::vars_os()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
                .collect();
            writeln!(out, "{}", serde_json::to_string_pretty(&map)?)?;
        } else {
            // Environment order, like GNU (which walks `environ`).
            for (k, v) in std::env::vars_os() {
                out.write_all(k.as_bytes())?;
                out.write_all(b"=")?;
                out.write_all(v.as_bytes())?;
                out.write_all(term)?;
            }
        }
        out.flush()?;
        return Ok(0);
    }

    let mut exit_code = 0;
    for name in &parsed.vars {
        let value = (!name.contains('='))
            .then(|| std::env::var_os(name))
            .flatten();
        match value {
            Some(val) => {
                out.write_all(val.as_bytes())?;
                out.write_all(term)?;
            }
            None => exit_code = 1,
        }
    }
    out.flush()?;
    Ok(exit_code)
}
