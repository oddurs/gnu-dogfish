//! hostname — Print system network hostname

use std::ffi::OsString;

use anyhow::Result;
use dogfish_core::{DogfishContext, outln};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, print_help, print_version, usage_error,
};
use crate::common::quote::quote_locale;
use crate::common::uname_info::get_uname_info;

const HOSTNAME_HELP: &str = "\
Print the system's network host name

Usage: hostname [OPTIONS]

Options:
  -s, --short  Print the host name cut at the first dot
      --help   Display this help and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum HostnameOpt {
    Short,
    Help,
    Version,
}

/// GNU's standard `--help` and `--version`, plus `-s, --short`.
const SPECS: &[OptSpec<HostnameOpt>] = &[
    OptSpec::both(b's', "short", HasArg::No, HostnameOpt::Short),
    OptSpec::long("help", HasArg::No, HostnameOpt::Help),
    OptSpec::long("version", HasArg::No, HostnameOpt::Version),
];

pub fn run_hostname(args: Vec<OsString>, _ctx: &DogfishContext) -> Result<i32> {
    let mut short = false;
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    while let Some(opt) = opts.next() {
        match opt {
            Ok((HostnameOpt::Short, _)) => short = true,
            Ok((HostnameOpt::Help, _)) => return Ok(print_help(HOSTNAME_HELP)),
            Ok((HostnameOpt::Version, _)) => return Ok(print_version("hostname")),
            Err(e) => return Ok(getopt_error("hostname", &e)),
        }
    }
    // Setting the host name is not supported.
    if let Some(extra) = opts.operands().first() {
        return Ok(usage_error(
            "hostname",
            format!("extra operand {}", quote_locale(extra)),
        ));
    }
    let info = get_uname_info()?;
    let name = if short {
        info.nodename.split('.').next().unwrap_or_default()
    } else {
        &info.nodename
    };
    outln!("{}", name);
    Ok(0)
}

/// hostname's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("hostname", HOSTNAME_HELP, SPECS)
}
