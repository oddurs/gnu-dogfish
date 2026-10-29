//! nproc — Print number of available processing units

use anyhow::Result;
use dogfish_core::{DogfishContext, gnu_quote, outln};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, Order, getopt_error, lossy, print_help, print_version, usage_error,
};
use crate::common::quote::quote_locale;

const NPROC_HELP: &str = "\
Print the number of processing units available to the current process, which may be less than the
number of online processors

Usage: nproc [OPTIONS]

Options:
      --all         Print the number of installed processors
      --ignore <N>  If possible, exclude N processing units
  -J, --json        
      --help        Display this help and exit
";

#[derive(Default)]
struct NprocArgs {
    all: bool,
    ignore: Option<String>,
    json: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NprocArgsOpt {
    All,
    Ignore,
    Json,
    Help,
    Version,
}

const SPECS: &[OptSpec<NprocArgsOpt>] = &[
    OptSpec::long("all", HasArg::No, NprocArgsOpt::All),
    OptSpec::long("ignore", HasArg::Required, NprocArgsOpt::Ignore),
    OptSpec::both(b'J', "json", HasArg::No, NprocArgsOpt::Json),
    OptSpec::long("help", HasArg::No, NprocArgsOpt::Help),
    OptSpec::long("version", HasArg::No, NprocArgsOpt::Version),
];

/// Parses the command line; `Err` is the exit status after `--help`,
/// `--version` or a usage diagnostic.
fn parse_nproc_args(args: Vec<std::ffi::OsString>) -> std::result::Result<NprocArgs, i32> {
    let mut parsed = NprocArgs::default();
    let mut opts = Getopt::with_order(args.into_iter().skip(1).collect(), SPECS, Order::Permute);
    while let Some(opt) = opts.next() {
        let (o, v) = opt.map_err(|e| getopt_error("nproc", &e))?;
        match o {
            NprocArgsOpt::All => parsed.all = true,
            NprocArgsOpt::Ignore => parsed.ignore = v.as_deref().map(lossy),
            NprocArgsOpt::Json => parsed.json = true,
            NprocArgsOpt::Help => return Err(print_help(NPROC_HELP)),
            NprocArgsOpt::Version => return Err(print_version("nproc")),
        }
    }
    let operands = opts.operands();
    if let Some(extra) = operands.first() {
        return Err(usage_error(
            "nproc",
            format!("extra operand {}", quote_locale(extra)),
        ));
    }
    Ok(parsed)
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

pub fn run_nproc(args: Vec<std::ffi::OsString>, _ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_nproc_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
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
        outln!(
            "{}",
            serde_json::json!({ "available_cpus": count, "total_cpus": online_processors(true) })
        );
    } else {
        outln!("{}", count);
    }
    Ok(0)
}
