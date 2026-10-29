//! hostname — Print system network hostname

use anyhow::Result;
use clap::{ArgAction, Parser};
use dogfish_core::DogfishContext;

use crate::common::uname_info::get_uname_info;

#[derive(Parser)]
#[command(
    name = "hostname",
    about = "Print the system's network host name",
    disable_help_flag = true,
    infer_long_args = true
)]
pub(crate) struct HostnameArgs {
    /// Print the host name cut at the first dot
    #[arg(short = 's', long = "short")]
    pub(crate) short: bool,

    /// Display this help and exit
    #[arg(long, action = ArgAction::Help)]
    pub(crate) help: Option<bool>,
}

pub fn run_hostname(args: Vec<String>, _ctx: &DogfishContext) -> Result<i32> {
    let parsed = HostnameArgs::try_parse_from(args)?;
    let info = get_uname_info()?;
    let name = if parsed.short {
        info.nodename.split('.').next().unwrap_or_default()
    } else {
        &info.nodename
    };
    println!("{}", name);
    Ok(0)
}
