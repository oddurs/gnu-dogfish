//! sleep — GNU intervals (`1.5`, `2m`, `1d`), plus human durations (`500ms`)

use std::thread;

use anyhow::Result;
use clap::{ArgAction, Parser};
use dogfish_core::{DogfishContext, gnu_quote};

use crate::common::getopt::try_help;
use crate::common::interval::{duration_from_secs, parse_interval};

#[derive(Parser)]
#[command(
    name = "sleep",
    about = "Pause for NUMBER seconds, where NUMBER may have a suffix of 's' (seconds), \
             'm' (minutes), 'h' (hours) or 'd' (days); several arguments are summed. \
             Compound durations such as 500ms or 1m30s are accepted too.",
    disable_help_flag = true,
    infer_long_args = true
)]
pub(crate) struct SleepArgs {
    /// Display this help and exit
    #[arg(long, action = ArgAction::Help)]
    pub(crate) help: Option<bool>,

    #[arg(required = true, value_name = "NUMBER")]
    durations: Vec<String>,
}

pub fn run_sleep(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = SleepArgs::try_parse_from(args)?;
    let mut total = 0f64;
    let mut ok = true;
    for d in &parsed.durations {
        match parse_interval(d) {
            Some(secs) => total += secs,
            None => {
                eprintln!("sleep: invalid time interval {}", gnu_quote(d));
                ok = false;
            }
        }
    }
    if !ok {
        return Ok(try_help("sleep", 1));
    }
    let total = duration_from_secs(total);
    ctx.telemetry.set_strategy(format!("sleep {:?}", total));
    thread::sleep(total);
    Ok(0)
}
