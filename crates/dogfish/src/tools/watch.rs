//! watch — Execute a program periodically, showing fullscreen output with diff/timer

use std::io::{self, Write};
use std::process::Command;
use std::thread;

use anyhow::{Result, anyhow};
use clap::Parser;
use dogfish_core::{DogfishContext, gnu_quote};

use crate::common::date_format::{Stamp, format_time, local_time};
use crate::common::interval::{duration_from_secs, parse_interval};

#[derive(Parser)]
#[command(
    name = "watch",
    about = "Execute a program periodically, showing output fullscreen"
)]
struct WatchArgs {
    /// Specify update interval (e.g. 2s, 500ms, or 1.5)
    #[arg(short = 'n', long = "interval", default_value = "2s")]
    interval: String,

    ///Iterations limit (useful for scripts/testing; 0 = infinite)
    #[arg(short = 'c', long = "count", default_value_t = 0)]
    count: usize,

    #[arg(required = true, trailing_var_arg = true)]
    command: Vec<String>,
}

pub fn run_watch(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = WatchArgs::try_parse_from(args)?;
    let interval = parse_interval(&parsed.interval)
        .map(duration_from_secs)
        .ok_or_else(|| anyhow!("invalid interval {}", gnu_quote(&parsed.interval)))?;
    let cmd_str = parsed.command.join(" ");
    let st = ctx.style();
    let mut iter = 0usize;

    loop {
        iter += 1;
        let start = std::time::Instant::now();
        let output = Command::new("sh").arg("-c").arg(&cmd_str).output()?;
        let elapsed = start.elapsed();

        if ctx.stdout_is_tty {
            print!("\x1b[2J\x1b[H"); // Clear screen & home cursor
        }
        let now = Stamp::now();
        let clock = local_time(now.sec)
            .map(|lt| format_time(b"%H:%M:%S", &lt, now.ns, now.sec, false))
            .unwrap_or_default();
        println!(
            "{} {}   {}   {}",
            st.bold_cyan(&format!("Every {:?}:", interval)),
            st.bold(&cmd_str),
            st.dim(&format!(
                "[last run: {}]",
                dogfish_core::format_duration_human(elapsed)
            )),
            st.yellow(&String::from_utf8_lossy(&clock))
        );
        println!("{}", st.dim(&"─".repeat(72)));
        io::stdout().write_all(&output.stdout)?;
        io::stderr().write_all(&output.stderr)?;
        io::stdout().flush()?;

        if parsed.count > 0 && iter >= parsed.count {
            break;
        }
        thread::sleep(interval);
    }
    Ok(0)
}
