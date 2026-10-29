//! shuf — Fast Fisher-Yates shuffler for lines, numeric ranges (-i), or NDJSON!

use std::io::{self, BufRead, Write};

use anyhow::{Result, anyhow};
use clap::Parser;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, emit_objects_or_table, gnu_io_error,
    io::ignore_broken_pipe, read_ndjson_stdin, stdout,
};

#[derive(Parser)]
#[command(
    name = "shuf",
    about = "Generate random permutations of lines, ranges, or NDJSON objects"
)]
struct ShufArgs {
    /// Treat each ARG as an input line
    #[arg(short = 'e', long = "echo")]
    echo: bool,

    /// Act as if input came from a file containing the range LO-HI
    #[arg(short = 'i', long = "input-range")]
    input_range: Option<String>,

    /// Output at most COUNT lines/objects
    #[arg(short = 'n', long = "head-count")]
    head_count: Option<usize>,

    #[arg(short = 'J', long = "json")]
    json: bool,

    args: Vec<String>,
}

pub fn run_shuf(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = ShufArgs::try_parse_from(args)?;
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
        if lo > hi {
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
