//! shuf — GNU `shuf.c`: random permutations of lines (`-e`, files), of a
//! numeric range (`-i`, any size: sparse partial Fisher-Yates), `-n`,
//! `-r`, `-z`, `-o`, `--random-source`, + NDJSON shuffling on terminals.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;

use anyhow::Result;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, emit_objects_or_table, gnu_io_error, read_ndjson_stdin,
};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy, print_help, print_version, try_help,
};
use crate::common::input::{RawStdout, stdin_file};
use crate::common::lines::{lossy_lines, split_records};
use crate::common::quote::{qf, quote_locale};
use crate::common::xstrtol::{StrtolError, xstrtoumax};

const SHUF_HELP: &str = "\
Usage: shuf [OPTION]... [FILE]
  or:  shuf -e [OPTION]... [ARG]...
  or:  shuf -i LO-HI [OPTION]...
Write a random permutation of the input lines to standard output.

With no FILE, or when FILE is -, read standard input.

Mandatory arguments to long options are mandatory for short options too.
  -e, --echo                treat each ARG as an input line
  -i, --input-range=LO-HI   treat each number LO through HI as an input line
  -n, --head-count=COUNT    output at most COUNT lines
  -o, --output=FILE         write result to FILE instead of standard output
      --random-source=FILE  get random bytes from FILE
  -r, --repeat              output lines can be repeated
  -z, --zero-terminated     line delimiter is NUL, not newline
  -J, --json                print NDJSON objects as JSON
      --help        display this help and exit
      --version     output version information and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum ShufOpt {
    Echo,
    InputRange,
    HeadCount,
    Output,
    RandomSource,
    Repeat,
    ZeroTerminated,
    Json,
    Help,
    Version,
}

/// GNU's `long_opts` and `"ei:n:o:rz"`, plus the Dogfish `-J, --json`.
const SPECS: &[OptSpec<ShufOpt>] = &[
    OptSpec::both(b'e', "echo", HasArg::No, ShufOpt::Echo),
    OptSpec::both(b'i', "input-range", HasArg::Required, ShufOpt::InputRange),
    OptSpec::both(b'n', "head-count", HasArg::Required, ShufOpt::HeadCount),
    OptSpec::both(b'o', "output", HasArg::Required, ShufOpt::Output),
    OptSpec::long("random-source", HasArg::Required, ShufOpt::RandomSource),
    OptSpec::both(b'r', "repeat", HasArg::No, ShufOpt::Repeat),
    OptSpec::both(b'z', "zero-terminated", HasArg::No, ShufOpt::ZeroTerminated),
    OptSpec::long("help", HasArg::No, ShufOpt::Help),
    OptSpec::long("version", HasArg::No, ShufOpt::Version),
    OptSpec::both(b'J', "json", HasArg::No, ShufOpt::Json).exact(),
];

/// What shuf was asked to do.
struct ShufConfig {
    echo: bool,
    /// `-i LO-HI`.
    range: Option<(u64, u64)>,
    head_lines: u64,
    outfile: Option<String>,
    random_source: Option<String>,
    repeat: bool,
    eol: u8,
    json: bool,
    operands: Vec<OsString>,
}

/// GNU's `-i` parsing: `LO-HI`, both plain numbers, with at least one line.
fn parse_range(arg: &str) -> std::result::Result<(u64, u64), String> {
    let bad = |overflow: bool| {
        let mut msg = format!("invalid input range: {}", quote_locale(arg));
        if overflow {
            msg.push_str(": Value too large for defined data type");
        }
        msg
    };
    let digits_end = {
        let b = arg.as_bytes();
        let mut i = b
            .iter()
            .take_while(|c| matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
            .count();
        if b.get(i) == Some(&b'+') {
            i += 1;
        }
        i + b[i..].iter().take_while(|c| c.is_ascii_digit()).count()
    };
    let (lo, hi) = arg.split_at(digits_end);
    let lo = match xstrtoumax(lo, "") {
        (v, None) => v,
        (_, Some(StrtolError::Overflow)) => return Err(bad(true)),
        _ => return Err(bad(false)),
    };
    let Some(hi) = hi.strip_prefix('-') else {
        return Err(bad(false));
    };
    let hi = match xstrtoumax(hi, "") {
        (v, None) => v,
        (_, Some(StrtolError::Overflow)) => return Err(bad(true)),
        _ => return Err(bad(false)),
    };
    let n_lines = hi.wrapping_sub(lo).wrapping_add(1);
    if (lo <= hi) == (n_lines == 0) {
        return Err(bad(false));
    }
    Ok((lo, hi))
}

/// GNU's option loop; `Err` is the exit status after a diagnostic.
fn parse_args(args: Vec<OsString>) -> std::result::Result<ShufConfig, i32> {
    let mut cfg = ShufConfig {
        echo: false,
        range: None,
        head_lines: u64::MAX,
        outfile: None,
        random_source: None,
        repeat: false,
        eol: b'\n',
        json: false,
        operands: Vec::new(),
    };
    let die = |msg: String| {
        eprintln!("shuf: {msg}");
        1
    };
    let mut g = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    while let Some(next) = g.next() {
        let (key, value) = next.map_err(|e| getopt_error("shuf", &e))?;
        let value = value.as_deref().map(lossy).unwrap_or_default();
        match key {
            ShufOpt::Echo => cfg.echo = true,
            ShufOpt::InputRange => {
                if cfg.range.is_some() {
                    return Err(die("multiple -i options specified".to_string()));
                }
                cfg.range = Some(parse_range(&value).map_err(die)?);
            }
            ShufOpt::HeadCount => match xstrtoumax(&value, "") {
                (n, None) => cfg.head_lines = cfg.head_lines.min(n),
                (_, Some(StrtolError::Overflow)) => {}
                _ => {
                    return Err(die(format!("invalid line count: {}", quote_locale(&value))));
                }
            },
            ShufOpt::Output => {
                if cfg.outfile.as_ref().is_some_and(|o| *o != value) {
                    return Err(die("multiple output files specified".to_string()));
                }
                cfg.outfile = Some(value);
            }
            ShufOpt::RandomSource => {
                if cfg.random_source.as_ref().is_some_and(|r| *r != value) {
                    return Err(die("multiple random sources specified".to_string()));
                }
                cfg.random_source = Some(value);
            }
            ShufOpt::Repeat => cfg.repeat = true,
            ShufOpt::ZeroTerminated => cfg.eol = 0,
            ShufOpt::Json => cfg.json = true,
            ShufOpt::Help => return Err(print_help(SHUF_HELP)),
            ShufOpt::Version => return Err(print_version("shuf")),
        }
    }
    cfg.operands = g.operands();
    if cfg.echo && cfg.range.is_some() {
        eprintln!("shuf: cannot combine -e and -i options");
        return Err(try_help("shuf"));
    }
    let extra = match (cfg.range.is_some(), cfg.echo) {
        (true, _) => cfg.operands.first(),
        (false, false) => cfg.operands.get(1),
        _ => None,
    };
    if let Some(extra) = extra {
        eprintln!("shuf: extra operand {}", quote_locale(extra.as_os_str()));
        return Err(try_help("shuf"));
    }
    Ok(cfg)
}

/// Random numbers: bytes from `--random-source`, else a xorshift64*
/// generator seeded from the system's entropy.
enum Rng {
    File {
        file: io::BufReader<File>,
        name: String,
    },
    Fast(u64),
}

impl Rng {
    fn new(source: Option<&str>) -> std::result::Result<Self, String> {
        match source {
            Some(name) => File::open(name)
                .map(|f| Rng::File {
                    file: io::BufReader::new(f),
                    name: name.to_string(),
                })
                .map_err(|e| format!("{}: {}", qf(name), gnu_io_error(&e))),
            None => {
                let mut seed = [0u8; 8];
                let ok = File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut seed));
                let mut s = u64::from_ne_bytes(seed);
                if ok.is_err() || s == 0 {
                    s = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos() as u64)
                        .unwrap_or(0x9e37_79b9_7f4a_7c15)
                        ^ u64::from(std::process::id()).wrapping_mul(0x9e37_79b9_7f4a_7c15)
                        | 1;
                }
                Ok(Rng::Fast(s))
            }
        }
    }

    fn next_u64(&mut self) -> u64 {
        match self {
            Rng::Fast(s) => {
                *s ^= *s >> 12;
                *s ^= *s << 25;
                *s ^= *s >> 27;
                s.wrapping_mul(0x2545_f491_4f6c_dd1d)
            }
            Rng::File { file, name } => {
                let mut b = [0u8; 8];
                if let Err(e) = file.read_exact(&mut b) {
                    let why = if e.kind() == io::ErrorKind::UnexpectedEof {
                        "end of file".to_string()
                    } else {
                        gnu_io_error(&e)
                    };
                    eprintln!("shuf: {}: {why}", qf(name.as_str()));
                    std::process::exit(1);
                }
                u64::from_le_bytes(b)
            }
        }
    }

    /// Uniform in `0..=max` (rejection sampling, no modulo bias).
    fn upto(&mut self, max: u64) -> u64 {
        if max == u64::MAX {
            return self.next_u64();
        }
        let n = max + 1;
        let zone = u64::MAX - (u64::MAX % n + 1) % n;
        loop {
            let v = self.next_u64();
            if v <= zone {
                return v % n;
            }
        }
    }
}

/// The first `k` entries of a random permutation of `0..n` (partial
/// Fisher-Yates): over an array when `n` is moderate, else with the moved
/// entries kept in a map, so `n` may be huge.
fn partial_permutation(rng: &mut Rng, k: u64, n: u64) -> Vec<u64> {
    const DENSE_MAX: u64 = 1 << 26;
    if n <= DENSE_MAX
        && let Ok(len) = usize::try_from(n)
    {
        let mut v: Vec<u64> = (0..n).collect();
        let k = usize::try_from(k).unwrap_or(len).min(len);
        for i in 0..k {
            let j = i + rng.upto((len - 1 - i) as u64) as usize;
            v.swap(i, j);
        }
        v.truncate(k);
        return v;
    }
    let mut moved: HashMap<u64, u64> = HashMap::new();
    let mut out = Vec::with_capacity(usize::try_from(k).unwrap_or(0).min(1 << 24));
    for i in 0..k {
        let j = i + rng.upto(n - 1 - i);
        let vi = *moved.get(&i).unwrap_or(&i);
        let vj = *moved.get(&j).unwrap_or(&j);
        moved.insert(j, vi);
        moved.remove(&i);
        out.push(vj);
    }
    out
}

/// Records of `data` ending in `eol` (a missing final terminator is added
/// on output, as GNU's `read_input` does).
fn read_records(data: &[u8], eol: u8) -> Vec<&[u8]> {
    split_records(data, eol).collect()
}

pub fn run_shuf(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let cfg = match parse_args(args) {
        Ok(cfg) => cfg,
        Err(status) => return Ok(status),
    };
    ctx.telemetry.set_strategy("partial Fisher-Yates shuffle");

    // Input: operands, a range, or the lines of one file (not read at all
    // for -n 0).
    let mut data = Vec::new();
    if cfg.head_lines > 0 && !cfg.echo && cfg.range.is_none() {
        let res = match cfg.operands.first().map(OsString::as_os_str) {
            None => stdin_file().read_to_end(&mut data),
            Some(p) if p == "-" => stdin_file().read_to_end(&mut data),
            Some(path) => File::open(path).and_then(|mut f| f.read_to_end(&mut data)),
        };
        if let Err(e) = res {
            let name = cfg
                .operands
                .first()
                .map_or(OsStr::new("-"), OsString::as_os_str);
            let what = if cfg.operands.is_empty() {
                "read error".to_string()
            } else {
                qf(name)
            };
            eprintln!("shuf: {what}: {}", gnu_io_error(&e));
            return Ok(1);
        }
    }
    let lines: Vec<&[u8]> = if cfg.echo {
        cfg.operands.iter().map(|s| s.as_bytes()).collect()
    } else {
        read_records(&data, cfg.eol)
    };

    // Rich output: NDJSON objects are shuffled as objects on a terminal (or
    // with -J). Piped output is GNU's, byte for byte.
    if (ctx.stdout_is_tty() || cfg.json)
        && !cfg.echo
        && cfg.range.is_none()
        && !cfg.repeat
        && cfg.eol == b'\n'
        && cfg.outfile.is_none()
        && let Some(mut objs) = read_ndjson_stdin(&lossy_lines(&data))
    {
        let mut rng = match Rng::new(cfg.random_source.as_deref()) {
            Ok(r) => r,
            Err(msg) => {
                eprintln!("shuf: {msg}");
                return Ok(1);
            }
        };
        let n = objs.len() as u64;
        let k = cfg.head_lines.min(n);
        let order = partial_permutation(&mut rng, k, n);
        let picked: Vec<_> = order
            .into_iter()
            .map(|i| std::mem::take(&mut objs[i as usize]))
            .collect();
        ctx.telemetry.add_items(picked.len() as u64);
        emit_objects_or_table(&picked, ctx, cfg.json, !cfg.json)?;
        return Ok(0);
    }

    let n_lines: u64 = match cfg.range {
        Some((lo, hi)) => hi.wrapping_sub(lo).wrapping_add(1),
        None => lines.len() as u64,
    };
    let mut rng = match Rng::new(cfg.random_source.as_deref()) {
        Ok(r) => r,
        Err(msg) => {
            eprintln!("shuf: {msg}");
            return Ok(1);
        }
    };
    let sink: Box<dyn Write> = match &cfg.outfile {
        Some(path) => match File::create(path) {
            Ok(f) => Box::new(f),
            Err(e) => {
                eprintln!("shuf: {}: {}", qf(path), gnu_io_error(&e));
                return Ok(1);
            }
        },
        None => Box::new(RawStdout),
    };
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, sink);
    let mut itoa_buf = itoa::Buffer::new();
    let mut write_item = |out: &mut io::BufWriter<Box<dyn Write>>, idx: u64| -> io::Result<()> {
        match cfg.range {
            Some((lo, _)) => out.write_all(itoa_buf.format(lo + idx).as_bytes())?,
            None => out.write_all(lines[idx as usize])?,
        }
        out.write_all(&[cfg.eol])
    };

    let res: io::Result<()> = (|| {
        if cfg.repeat {
            if cfg.head_lines == 0 {
                return Ok(());
            }
            if n_lines == 0 {
                eprintln!("shuf: no lines to repeat");
                std::process::exit(1);
            }
            let mut left = cfg.head_lines;
            while left > 0 {
                let idx = rng.upto(n_lines - 1);
                write_item(&mut out, idx)?;
                // u64::MAX stands for "no -n": repeat forever.
                if cfg.head_lines != u64::MAX {
                    left -= 1;
                }
            }
            return Ok(());
        }
        let k = cfg.head_lines.min(n_lines);
        for idx in partial_permutation(&mut rng, k, n_lines) {
            write_item(&mut out, idx)?;
        }
        out.flush()
    })();
    res?;
    ctx.telemetry.add_items(n_lines.min(cfg.head_lines));
    out.flush()?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::{Rng, parse_range, partial_permutation};

    #[test]
    fn ranges() {
        assert_eq!(parse_range("1-100"), Ok((1, 100)));
        assert_eq!(parse_range("5-5"), Ok((5, 5)));
        assert!(parse_range("1").is_err());
        assert!(parse_range("A").is_err());
        assert!(parse_range("1-").is_err());
        assert!(parse_range("1-A").is_err());
        assert_eq!(parse_range("2-1"), Ok((2, 1)));
        assert!(parse_range("3-1").is_err());
        assert!(parse_range("0-18446744073709551615").is_err());
        assert!(parse_range("1-18446744073709551615").is_ok());
    }

    #[test]
    fn permutation_is_a_permutation() {
        let mut rng = Rng::Fast(42);
        let mut p = partial_permutation(&mut rng, 100, 100);
        p.sort_unstable();
        assert_eq!(p, (0..100).collect::<Vec<_>>());
        assert_eq!(partial_permutation(&mut rng, 2, u64::MAX).len(), 2);
    }
}

/// Entry points for the fuzz targets in `fuzz/` (only with the `fuzzing` feature).
#[cfg(feature = "fuzzing")]
#[allow(dead_code)]
pub mod fuzz {
    use super::*;

    pub fn args(args: Vec<OsString>) {
        let _ = parse_args(args);
    }
}
