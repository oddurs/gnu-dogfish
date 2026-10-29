//! sleep — GNU intervals (`1.5`, `2m`, `1d`), plus human durations (`500ms`)

use std::ffi::OsString;
use std::thread;

use anyhow::Result;
use dogfish_core::{DogfishContext, gnu_quote};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy_all, print_help, print_version, try_help,
    usage_error,
};
use crate::common::interval::{duration_from_secs, parse_interval};

const SLEEP_HELP: &str = "\
Pause for NUMBER seconds, where NUMBER may have a suffix of 's' (seconds), 'm' (minutes), 'h'
(hours) or 'd' (days); several arguments are summed. Compound durations such as 500ms or 1m30s are
accepted too.

Usage: sleep <NUMBER>...

Arguments:
  <NUMBER>...  

Options:
      --help  Display this help and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum SleepOpt {
    Help,
    Version,
}

/// gnulib's `parse_gnu_standard_options_only`.
const SPECS: &[OptSpec<SleepOpt>] = &[
    OptSpec::long("help", HasArg::No, SleepOpt::Help),
    OptSpec::long("version", HasArg::No, SleepOpt::Version),
];

pub fn run_sleep(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    // Every option ends the parse: help, version or an error.
    if let Some(opt) = opts.next() {
        return Ok(match opt {
            Ok((SleepOpt::Help, _)) => print_help(SLEEP_HELP),
            Ok((SleepOpt::Version, _)) => print_version("sleep"),
            Err(e) => getopt_error("sleep", &e),
        });
    }
    let durations = lossy_all(opts.operands());
    if durations.is_empty() {
        return Ok(usage_error("sleep", "missing operand"));
    }
    let mut total = 0f64;
    let mut ok = true;
    for d in &durations {
        match parse_interval(d) {
            Some(secs) => total += secs,
            None => {
                eprintln!("sleep: invalid time interval {}", gnu_quote(d));
                ok = false;
            }
        }
    }
    if !ok {
        return Ok(try_help("sleep"));
    }
    let total = duration_from_secs(total);
    ctx.telemetry.set_strategy(format!("sleep {:?}", total));
    thread::sleep(total);
    Ok(0)
}

/// sleep's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("sleep", SLEEP_HELP, SPECS)
}
