//! true & false

use std::ffi::OsString;

use anyhow::Result;
use dogfish_core::DogfishContext;

use crate::common::getopt::{print_help, print_version};

const TRUE_HELP: &str = "\
Usage: true [ignored command line arguments]
  or:  true OPTION
Exit with a status code indicating success.

      --help        display this help and exit
      --version     output version information and exit
";

const FALSE_HELP: &str = "\
Usage: false [ignored command line arguments]
  or:  false OPTION
Exit with a status code indicating failure.

      --help        display this help and exit
      --version     output version information and exit
";

/// GNU `true`/`false` ignore every argument except a lone `--help` or
/// `--version` (exactly, no abbreviations), and exit with the tool's own
/// status even then. A failure to write the help is reported at exit.
fn run_trivial(tool: &str, args: &[OsString], status: i32, help: &str) -> Result<i32> {
    if args.len() == 2 {
        if args[1] == "--help" {
            print_help(help);
        } else if args[1] == "--version" {
            print_version(tool);
        }
    }
    Ok(status)
}

pub fn run_true(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    ctx.telemetry.set_strategy("immediate exit(0)");
    run_trivial("true", &args, 0, TRUE_HELP)
}

pub fn run_false(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    ctx.telemetry.set_strategy("immediate exit(1)");
    run_trivial("false", &args, 1, FALSE_HELP)
}
