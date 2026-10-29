//! true & false

use anyhow::Result;
use dogfish_core::DogfishContext;

use crate::common::getopt::print_help;

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

/// GNU `true`/`false` ignore every argument except a lone `--help`, which
/// still exits with the tool's own status.
fn run_trivial(args: &[String], status: i32, help: &str) -> Result<i32> {
    if args.len() == 2 && args[1] == "--help" {
        print_help(help)?;
    }
    Ok(status)
}

pub fn run_true(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    ctx.telemetry.set_strategy("immediate exit(0)");
    run_trivial(&args, 0, TRUE_HELP)
}

pub fn run_false(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    ctx.telemetry.set_strategy("immediate exit(1)");
    run_trivial(&args, 1, FALSE_HELP)
}
