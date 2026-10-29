//! uname — With `--json` and visual card mode

use anyhow::Result;
use clap::{ArgAction, Parser};
use dogfish_core::DogfishContext;

use crate::common::uname_info::{UNKNOWN, get_uname_info};

#[derive(Parser)]
#[command(
    name = "uname",
    about = "Print certain system information.  With no OPTION, same as -s.",
    disable_help_flag = true,
    infer_long_args = true
)]
struct UnameArgs {
    /// Print all information, in the following order, except omit -p and -i if unknown
    #[arg(short = 'a', long = "all")]
    all: bool,
    /// Print the kernel name
    #[arg(short = 's', long = "kernel-name")]
    sysname: bool,
    /// Print the network node hostname
    #[arg(short = 'n', long = "nodename")]
    nodename: bool,
    /// Print the kernel release
    #[arg(short = 'r', long = "kernel-release")]
    release: bool,
    /// Print the kernel version
    #[arg(short = 'v', long = "kernel-version")]
    version: bool,
    /// Print the machine hardware name
    #[arg(short = 'm', long = "machine")]
    machine: bool,
    /// Print the processor type (non-portable)
    #[arg(short = 'p', long = "processor")]
    processor: bool,
    /// Print the hardware platform (non-portable)
    #[arg(short = 'i', long = "hardware-platform")]
    platform: bool,
    /// Print the operating system
    #[arg(short = 'o', long = "operating-system")]
    os: bool,
    /// Emit structured JSON
    #[arg(short = 'J', long = "json")]
    json: bool,
    /// Display this help and exit
    #[arg(long, action = ArgAction::Help)]
    help: Option<bool>,
}

pub fn run_uname(args: Vec<String>, _ctx: &DogfishContext) -> Result<i32> {
    let parsed = UnameArgs::try_parse_from(args)?;
    let info = get_uname_info()?;

    if parsed.json {
        println!("{}", serde_json::to_string_pretty(&info)?);
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
    println!("{}", parts.join(" "));
    Ok(0)
}
