//! whoami

use anyhow::Result;
use clap::{ArgAction, Parser};
use dogfish_core::DogfishContext;

use crate::common::users::get_username_by_uid;

#[derive(Parser)]
#[command(
    name = "whoami",
    about = "Print the user name associated with the current effective user ID",
    disable_help_flag = true,
    infer_long_args = true
)]
struct WhoamiArgs {
    /// Display this help and exit
    #[arg(long, action = ArgAction::Help)]
    help: Option<bool>,

    #[arg(short = 'J', long = "json")]
    json: bool,
}

pub fn run_whoami(args: Vec<String>, _ctx: &DogfishContext) -> Result<i32> {
    let parsed = WhoamiArgs::try_parse_from(args)?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    let Some(username) = get_username_by_uid(uid) else {
        eprintln!("whoami: cannot find name for user ID {uid}");
        return Ok(1);
    };
    if parsed.json {
        println!(
            "{}",
            serde_json::json!({ "uid": uid, "username": username })
        );
    } else {
        println!("{}", username);
    }
    Ok(0)
}
