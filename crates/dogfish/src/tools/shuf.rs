//! shuf — Fast Fisher-Yates shuffler for lines, numeric ranges (-i), or NDJSON!

use std::io::{self, BufRead, Write};

use std::ffi::OsString;

use anyhow::{Result, anyhow};
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, emit_objects_or_table, gnu_io_error,
    io::ignore_broken_pipe, read_ndjson_stdin, stdout,
};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy, lossy_all, print_help, print_version,
    unsupported, usage_error,
};
use crate::common::quote::quote_locale;

const SHUF_HELP: &str = "\
Generate random permutations of lines, ranges, or NDJSON objects

Usage: shuf [OPTIONS] [ARGS]...

Arguments:
  [ARGS]...  

Options:
  -e, --echo                       Treat each ARG as an input line
  -i, --input-range <INPUT_RANGE>  Act as if input came from a file containing the range LO-HI
  -n, --head-count <HEAD_COUNT>    Output at most COUNT lines/objects
  -J, --json                       
      --help                       Print help
";

#[derive(Default)]
struct ShufArgs {
    echo: bool,
    input_range: Option<String>,
    head_count: Option<usize>,
    json: bool,
    args: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ShufOpt {
    Echo,
    InputRange,
    HeadCount,
    Json,
    /// A GNU option Dogfish does not implement yet.
    Unsupported,
    Help,
    Version,
}

/// GNU's `long_opts` and `"ei:n:o:rz"`, plus the Dogfish `-J, --json`.
const SPECS: &[OptSpec<ShufOpt>] = &[
    OptSpec::both(b'e', "echo", HasArg::No, ShufOpt::Echo),
    OptSpec::both(b'i', "input-range", HasArg::Required, ShufOpt::InputRange),
    OptSpec::both(b'n', "head-count", HasArg::Required, ShufOpt::HeadCount),
    OptSpec::both(b'o', "output", HasArg::Required, ShufOpt::Unsupported),
    OptSpec::long("random-source", HasArg::Required, ShufOpt::Unsupported),
    OptSpec::both(b'r', "repeat", HasArg::No, ShufOpt::Unsupported),
    OptSpec::both(b'z', "zero-terminated", HasArg::No, ShufOpt::Unsupported),
    OptSpec::long("help", HasArg::No, ShufOpt::Help),
    OptSpec::long("version", HasArg::No, ShufOpt::Version),
    OptSpec::both(b'J', "json", HasArg::No, ShufOpt::Json),
];

/// gnulib `xstrtoumax` on a prefix of `s`: the value (`None` on overflow)
/// and the rest, or `Err` when `s` does not start with a number.
fn leading_umax(s: &str) -> std::result::Result<(Option<u64>, &str), ()> {
    let t = s.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    let t = t.strip_prefix('+').unwrap_or(t);
    let len = t.bytes().take_while(u8::is_ascii_digit).count();
    if len == 0 {
        return Err(());
    }
    Ok((t[..len].parse().ok(), &t[len..]))
}

/// Parses the command line; `Err` is the exit status after `--help`,
/// `--version` or a diagnostic.
fn parse_shuf_args(args: Vec<OsString>) -> std::result::Result<ShufArgs, i32> {
    let mut p = ShufArgs::default();
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    while let Some(opt) = opts.next() {
        let (o, v) = opt.map_err(|e| getopt_error("shuf", &e))?;
        let v = v.as_deref().map(lossy).unwrap_or_default();
        match o {
            ShufOpt::Echo => p.echo = true,
            ShufOpt::InputRange => {
                if p.input_range.is_some() {
                    eprintln!("shuf: multiple -i options specified");
                    return Err(1);
                }
                // GNU: LO-HI, with HI + 1 == LO an empty range.
                let range = match leading_umax(&v) {
                    Ok((lo, rest)) => match rest.strip_prefix('-').map(leading_umax) {
                        Some(Ok((hi, ""))) => Ok((lo, hi)),
                        _ => Err(false),
                    },
                    Err(()) => Err(false),
                };
                let valid = match range {
                    Ok((Some(lo), Some(hi))) => {
                        let n_lines = hi.wrapping_sub(lo).wrapping_add(1);
                        if (lo <= hi) == (n_lines == 0) {
                            Err(false)
                        } else {
                            Ok(())
                        }
                    }
                    Ok(_) => Err(true),
                    Err(overflow) => Err(overflow),
                };
                match valid {
                    Ok(()) => p.input_range = Some(v),
                    Err(overflow) => {
                        let why = if overflow {
                            ": Value too large for defined data type"
                        } else {
                            ""
                        };
                        eprintln!("shuf: invalid input range: {}{why}", quote_locale(&v));
                        return Err(1);
                    }
                }
            }
            // Like GNU, the smallest of several counts wins.
            ShufOpt::HeadCount => match leading_umax(&v) {
                Ok((n, "")) => {
                    let n = n.map_or(usize::MAX, |n| usize::try_from(n).unwrap_or(usize::MAX));
                    p.head_count = Some(p.head_count.map_or(n, |h| h.min(n)));
                }
                _ => {
                    eprintln!("shuf: invalid line count: {}", quote_locale(&v));
                    return Err(1);
                }
            },
            ShufOpt::Json => p.json = true,
            ShufOpt::Unsupported => return Err(unsupported("shuf", &opts.last_spelling())),
            ShufOpt::Help => return Err(print_help(SHUF_HELP)),
            ShufOpt::Version => return Err(print_version("shuf")),
        }
    }
    p.args = lossy_all(opts.operands());
    if p.echo && p.input_range.is_some() {
        return Err(usage_error("shuf", "cannot combine -e and -i options"));
    }
    // GNU: -i takes no operands, and without -e there is at most one FILE.
    let extra = if p.input_range.is_some() {
        p.args.first()
    } else if !p.echo {
        p.args.get(1)
    } else {
        None
    };
    if let Some(extra) = extra {
        return Err(usage_error(
            "shuf",
            format!("extra operand {}", quote_locale(extra)),
        ));
    }
    Ok(p)
}

pub fn run_shuf(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_shuf_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    ctx.telemetry
        .set_strategy("xorshift64* Fisher-Yates shuffle");

    // Seed from system time + pid
    let mut rng_state = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0xdeadbeef_cafebabe)
        ^ (std::process::id() as u64).wrapping_mul(0x9e3779b97f4a7c15);
    if rng_state == 0 {
        rng_state = 1;
    }
    let mut next_u64 = || -> u64 {
        rng_state ^= rng_state >> 12;
        rng_state ^= rng_state << 25;
        rng_state ^= rng_state >> 27;
        rng_state.wrapping_mul(0x2545F4914F6CDD1D)
    };

    let mut lines: Vec<String> = if parsed.echo {
        parsed.args.clone()
    } else if let Some(ref range_str) = parsed.input_range {
        let (lo_s, hi_s) = range_str
            .split_once('-')
            .ok_or_else(|| anyhow!("shuf: invalid input range '{}'", range_str))?;
        let lo: i64 = lo_s.parse()?;
        let hi: i64 = hi_s.parse()?;
        // `HI + 1 == LO` is the empty range (the parser rejected the rest).
        if lo > hi.saturating_add(1) {
            return Err(anyhow!("shuf: invalid input range '{}'", range_str));
        }
        let mut nums: Vec<i64> = (lo..=hi).collect();
        let len = nums.len();
        let count = parsed.head_count.unwrap_or(len).min(len);
        for i in 0..count {
            let j = i + (next_u64() as usize) % (len - i);
            nums.swap(i, j);
        }
        let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout::stdout());
        let mut itoa_buf = itoa::Buffer::new();
        for &n in &nums[..count] {
            ignore_broken_pipe(out.write_all(itoa_buf.format(n).as_bytes()))?;
            ignore_broken_pipe(out.write_all(b"\n"))?;
        }
        ctx.telemetry.add_items(count as u64);
        ignore_broken_pipe(out.flush())?;
        return Ok(0);
    } else {
        let sources = InputSource::from_args(&parsed.args);
        let mut all = Vec::new();
        for s in &sources {
            let r = FastReader::open(s)
                .map_err(|e| anyhow!("shuf: {}: {}", s.display_name(), gnu_io_error(&e)))?
                .into_bufread();
            for l in r.lines() {
                all.push(l?);
            }
        }
        all
    };

    if !parsed.echo
        && parsed.input_range.is_none()
        && let Some(mut objs) = read_ndjson_stdin(&lines)
    {
        let len = objs.len();
        for i in (1..len).rev() {
            let j = (next_u64() as usize) % (i + 1);
            objs.swap(i, j);
        }
        if let Some(n) = parsed.head_count {
            objs.truncate(n);
        }
        ctx.telemetry.add_items(objs.len() as u64);
        emit_objects_or_table(&objs, ctx, parsed.json, !parsed.json)?;
        return Ok(0);
    }

    let len = lines.len();
    let count = parsed.head_count.unwrap_or(len).min(len);
    for i in 0..count {
        let j = i + (next_u64() as usize) % (len - i);
        lines.swap(i, j);
    }

    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout::stdout());
    for line in &lines[..count] {
        ignore_broken_pipe(writeln!(out, "{}", line))?;
    }
    ctx.telemetry.add_items(count as u64);
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}
