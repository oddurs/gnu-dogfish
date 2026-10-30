//! watch — Execute a program periodically, showing fullscreen output with diff/timer

use std::ffi::OsString;
use std::io::{self, Write};
use std::process::Command;
use std::thread;

use anyhow::{Result, anyhow};
use dogfish_core::{DogfishContext, gnu_quote, out, outln, stdout};

use crate::common::date_format::{Stamp, format_time, local_time};
use crate::common::getopt::{
    Getopt, HasArg, OptSpec, Order, getopt_error, lossy, lossy_all, print_help, print_version,
    usage_error,
};
use crate::common::interval::{duration_from_secs, parse_interval};
use crate::common::signals::keep_ignored_sigpipe;

const WATCH_HELP: &str = "\
Execute a program periodically, showing output fullscreen

Usage: watch [OPTIONS] <COMMAND>...

Arguments:
  <COMMAND>...  

Options:
  -n, --interval <INTERVAL>  Specify update interval (e.g. 2s, 500ms, or 1.5) [default: 2s]
  -c, --count <COUNT>        Iterations limit (useful for scripts/testing; 0 = infinite) [default:
                             0]
  -h, --help                 Print help
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum WatchOpt {
    Interval,
    Count,
    Help,
    Version,
}

/// procps `watch`'s `-n, --interval`, plus the Dogfish `-c, --count`.
/// Options end at the COMMAND.
const SPECS: &[OptSpec<WatchOpt>] = &[
    OptSpec::both(b'n', "interval", HasArg::Required, WatchOpt::Interval),
    OptSpec::both(b'c', "count", HasArg::Required, WatchOpt::Count),
    OptSpec::both(b'h', "help", HasArg::No, WatchOpt::Help),
    OptSpec::long("version", HasArg::No, WatchOpt::Version),
];

struct WatchArgs {
    interval: String,
    count: usize,
    command: Vec<String>,
}

/// Parses the command line; `Err` is the exit status after `--help`,
/// `--version` or a usage diagnostic.
fn parse_watch_args(args: Vec<OsString>) -> std::result::Result<WatchArgs, i32> {
    let mut p = WatchArgs {
        interval: "2s".to_string(),
        count: 0,
        command: Vec::new(),
    };
    let mut opts = Getopt::with_order(
        args.into_iter().skip(1).collect(),
        SPECS,
        Order::StopAtOperand,
    );
    while let Some(opt) = opts.next() {
        let (o, v) = opt.map_err(|e| getopt_error("watch", &e))?;
        let v = v.as_deref().map(lossy).unwrap_or_default();
        match o {
            WatchOpt::Interval => p.interval = v,
            WatchOpt::Count => match v.parse() {
                Ok(n) => p.count = n,
                Err(_) => {
                    return Err(usage_error(
                        "watch",
                        format!("invalid argument {} for '--count'", gnu_quote(&v)),
                    ));
                }
            },
            WatchOpt::Help => return Err(print_help(WATCH_HELP)),
            WatchOpt::Version => return Err(print_version("watch")),
        }
    }
    p.command = lossy_all(opts.operands());
    if p.command.is_empty() {
        return Err(usage_error("watch", "missing operand"));
    }
    Ok(p)
}

pub fn run_watch(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    // As procps' `close_stdout`: a broken pipe at exit is not reported.
    stdout::ignore_epipe();
    let parsed = match parse_watch_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let interval = parse_interval(&parsed.interval)
        .map(duration_from_secs)
        .ok_or_else(|| anyhow!("invalid interval {}", gnu_quote(&parsed.interval)))?;
    let cmd_str = parsed.command.join(" ");
    let st = ctx.style();
    let mut iter = 0usize;

    loop {
        iter += 1;
        let start = std::time::Instant::now();
        let mut sh = Command::new("sh");
        sh.arg("-c").arg(&cmd_str);
        keep_ignored_sigpipe(&mut sh);
        let output = sh.output()?;
        let elapsed = start.elapsed();

        if ctx.stdout_is_tty() {
            out!("\x1b[2J\x1b[H"); // Clear screen & home cursor
        }
        let now = Stamp::now();
        let clock = local_time(now.sec)
            .map(|lt| format_time(b"%H:%M:%S", &lt, now.ns, false))
            .unwrap_or_default();
        outln!(
            "{} {}   {}   {}",
            st.bold_cyan(&format!("Every {:?}:", interval)),
            st.bold(&cmd_str),
            st.dim(&format!(
                "[last run: {}]",
                dogfish_core::format_duration_human(elapsed)
            )),
            st.yellow(&String::from_utf8_lossy(&clock))
        );
        outln!("{}", st.dim(&"─".repeat(72)));
        stdout::stdout().write_all(&output.stdout)?;
        io::stderr().write_all(&output.stderr)?;
        stdout::flush()?;

        if parsed.count > 0 && iter >= parsed.count {
            break;
        }
        thread::sleep(interval);
    }
    Ok(0)
}

/// watch's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("watch", WATCH_HELP, SPECS)
}
