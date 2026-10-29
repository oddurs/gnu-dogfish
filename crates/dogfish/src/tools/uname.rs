//! uname — With `--json` and visual card mode

use anyhow::Result;
use dogfish_core::{DogfishContext, outln};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, Order, getopt_error, print_help, print_version, usage_error,
};
use crate::common::quote::quote_locale;
use crate::common::uname_info::{UNKNOWN, get_uname_info};

const UNAME_HELP: &str = "\
Print certain system information.  With no OPTION, same as -s.

Usage: uname [OPTIONS]

Options:
  -a, --all                Print all information, in the following order, except omit -p and -i if
                           unknown
  -s, --kernel-name        Print the kernel name
  -n, --nodename           Print the network node hostname
  -r, --kernel-release     Print the kernel release
  -v, --kernel-version     Print the kernel version
  -m, --machine            Print the machine hardware name
  -p, --processor          Print the processor type (non-portable)
  -i, --hardware-platform  Print the hardware platform (non-portable)
  -o, --operating-system   Print the operating system
  -J, --json               Emit structured JSON
      --help               Display this help and exit
";

#[derive(Default)]
struct UnameArgs {
    all: bool,
    sysname: bool,
    nodename: bool,
    release: bool,
    version: bool,
    machine: bool,
    processor: bool,
    platform: bool,
    os: bool,
    json: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UnameArgsOpt {
    All,
    Sysname,
    Nodename,
    Release,
    KernelVersion,
    Machine,
    Processor,
    Platform,
    Os,
    Json,
    Help,
    Version,
}

const SPECS: &[OptSpec<UnameArgsOpt>] = &[
    OptSpec::both(b'a', "all", HasArg::No, UnameArgsOpt::All),
    OptSpec::both(b's', "kernel-name", HasArg::No, UnameArgsOpt::Sysname),
    OptSpec::long("sysname", HasArg::No, UnameArgsOpt::Sysname),
    OptSpec::both(b'n', "nodename", HasArg::No, UnameArgsOpt::Nodename),
    OptSpec::both(b'r', "kernel-release", HasArg::No, UnameArgsOpt::Release),
    OptSpec::long("release", HasArg::No, UnameArgsOpt::Release),
    OptSpec::both(
        b'v',
        "kernel-version",
        HasArg::No,
        UnameArgsOpt::KernelVersion,
    ),
    OptSpec::both(b'm', "machine", HasArg::No, UnameArgsOpt::Machine),
    OptSpec::both(b'p', "processor", HasArg::No, UnameArgsOpt::Processor),
    OptSpec::both(
        b'i',
        "hardware-platform",
        HasArg::No,
        UnameArgsOpt::Platform,
    ),
    OptSpec::both(b'o', "operating-system", HasArg::No, UnameArgsOpt::Os),
    OptSpec::both(b'J', "json", HasArg::No, UnameArgsOpt::Json),
    OptSpec::long("help", HasArg::No, UnameArgsOpt::Help),
    OptSpec::long("version", HasArg::No, UnameArgsOpt::Version),
];

/// Parses the command line; `Err` is the exit status after `--help`,
/// `--version` or a usage diagnostic.
fn parse_uname_args(args: Vec<std::ffi::OsString>) -> std::result::Result<UnameArgs, i32> {
    let mut parsed = UnameArgs::default();
    let mut opts = Getopt::with_order(args.into_iter().skip(1).collect(), SPECS, Order::Permute);
    while let Some(opt) = opts.next() {
        let (o, _) = opt.map_err(|e| getopt_error("uname", &e))?;
        match o {
            UnameArgsOpt::All => parsed.all = true,
            UnameArgsOpt::Sysname => parsed.sysname = true,
            UnameArgsOpt::Nodename => parsed.nodename = true,
            UnameArgsOpt::Release => parsed.release = true,
            UnameArgsOpt::KernelVersion => parsed.version = true,
            UnameArgsOpt::Machine => parsed.machine = true,
            UnameArgsOpt::Processor => parsed.processor = true,
            UnameArgsOpt::Platform => parsed.platform = true,
            UnameArgsOpt::Os => parsed.os = true,
            UnameArgsOpt::Json => parsed.json = true,
            UnameArgsOpt::Help => return Err(print_help(UNAME_HELP)),
            UnameArgsOpt::Version => return Err(print_version("uname")),
        }
    }
    let operands = opts.operands();
    if let Some(extra) = operands.first() {
        return Err(usage_error(
            "uname",
            format!("extra operand {}", quote_locale(extra)),
        ));
    }
    Ok(parsed)
}

pub fn run_uname(args: Vec<std::ffi::OsString>, _ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_uname_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let info = get_uname_info()?;

    if parsed.json {
        outln!("{}", serde_json::to_string_pretty(&info)?);
        return Ok(0);
    }

    let any_specific = parsed.sysname
        || parsed.nodename
        || parsed.release
        || parsed.version
        || parsed.machine
        || parsed.processor
        || parsed.platform
        || parsed.os;
    let all = parsed.all;
    let fields = [
        (
            all || parsed.sysname || !(all || any_specific),
            &info.sysname,
        ),
        (all || parsed.nodename, &info.nodename),
        (all || parsed.release, &info.release),
        (all || parsed.version, &info.version),
        (all || parsed.machine, &info.machine),
        (
            parsed.processor || (all && info.processor != UNKNOWN),
            &info.processor,
        ),
        (
            parsed.platform || (all && info.hardware_platform != UNKNOWN),
            &info.hardware_platform,
        ),
        (all || parsed.os, &info.operating_system),
    ];
    let parts: Vec<&str> = fields
        .iter()
        .filter(|(wanted, _)| *wanted)
        .map(|(_, v)| v.as_str())
        .collect();
    outln!("{}", parts.join(" "));
    Ok(0)
}

/// uname's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("uname", UNAME_HELP, SPECS)
}
