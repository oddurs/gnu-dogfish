//! nproc — Print number of available processing units

use anyhow::Result;
use clap::{ArgAction, Parser};
use dogfish_core::{DogfishContext, gnu_quote};

#[derive(Parser)]
#[command(
    name = "nproc",
    about = "Print the number of processing units available to the current process, \
             which may be less than the number of online processors",
    disable_help_flag = true,
    infer_long_args = true
)]
pub(crate) struct NprocArgs {
    /// Print the number of installed processors
    #[arg(long = "all")]
    pub(crate) all: bool,

    /// If possible, exclude N processing units
    #[arg(long = "ignore", value_name = "N")]
    ignore: Option<String>,

    #[arg(short = 'J', long = "json")]
    pub(crate) json: bool,

    /// Display this help and exit
    #[arg(long, action = ArgAction::Help)]
    pub(crate) help: Option<bool>,
}

/// Parses an OpenMP thread count like gnulib: a decimal number, optionally
/// followed by a comma-separated list; anything else counts as unset (0).
fn parse_omp_threads(var: &str) -> u64 {
    let Ok(value) = std::env::var(var) else {
        return 0;
    };
    let s = value.trim_start();
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return 0;
    }
    let rest = s[digits..].trim_start();
    if rest.is_empty() || rest.starts_with(',') {
        s[..digits].parse().unwrap_or(u64::MAX)
    } else {
        0
    }
}

fn online_processors(all: bool) -> u64 {
    #[cfg(target_os = "linux")]
    if !all {
        // SAFETY: `set` is a properly sized, zeroed CPU set for the calling thread.
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) == 0 {
                let n = libc::CPU_COUNT(&set);
                if n > 0 {
                    return n as u64;
                }
            }
        }
    }
    let name = if all {
        libc::_SC_NPROCESSORS_CONF
    } else {
        libc::_SC_NPROCESSORS_ONLN
    };
    // SAFETY: sysconf has no preconditions.
    let n = unsafe { libc::sysconf(name) };
    if n > 0 { n as u64 } else { 1 }
}

pub fn run_nproc(args: Vec<String>, _ctx: &DogfishContext) -> Result<i32> {
    let parsed = NprocArgs::try_parse_from(args)?;
    let ignore = match parsed.ignore.as_deref() {
        None => 0,
        Some(s) => match s.trim_start().parse::<u64>() {
            Ok(n) => n,
            Err(e) => {
                let overflow = *e.kind() == std::num::IntErrorKind::PosOverflow;
                let detail = if overflow {
                    ": Value too large for defined data type"
                } else {
                    ""
                };
                eprintln!("nproc: invalid number: {}{detail}", gnu_quote(s));
                return Ok(1);
            }
        },
    };
    let total = online_processors(parsed.all);
    let available = if parsed.all {
        total
    } else {
        let mut limit = parse_omp_threads("OMP_THREAD_LIMIT");
        if limit == 0 {
            limit = u64::MAX;
        }
        match parse_omp_threads("OMP_NUM_THREADS") {
            0 => online_processors(false).min(limit),
            n => n.min(limit),
        }
    };
    let count = if ignore < available {
        available - ignore
    } else {
        1
    };
    if parsed.json {
        println!(
            "{}",
            serde_json::json!({ "available_cpus": count, "total_cpus": online_processors(true) })
        );
    } else {
        println!("{}", count);
    }
    Ok(0)
}
