//! uniq — Deduplicate lines or NDJSON objects (`--by <field>`)

use std::cmp::Ordering;
use std::io::{self, BufRead, Read, Write};

use std::ffi::OsString;

use anyhow::Result;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, emit_objects_or_table, gnu_io_error,
    io::ignore_broken_pipe, read_ndjson_stdin, stdout, table::compare_json_field,
};
use serde_json::Value;

use crate::common::getopt::{
    Arg, Getopt, HasArg, OptSpec, Order, getopt_error, lossy, print_help, print_version,
    unsupported, usage_error,
};
use crate::common::lines::{chomp, is_blank, looks_like_json, lossy_lines};
use crate::common::quote::quote_locale;

const UNIQ_HELP: &str = "\
Report or omit repeated lines or NDJSON objects

Usage: uniq [OPTIONS] [INPUT] [OUTPUT]

Arguments:
  [INPUT]   
  [OUTPUT]  

Options:
  -c, --count                      Prefix lines by the number of occurrences
  -d, --repeated                   Only print duplicate lines, one for each group
  -f, --skip-fields <SKIP_FIELDS>  Avoid comparing the first N fields [default: 0]
  -i, --ignore-case                Ignore differences in case when comparing
  -s, --skip-chars <SKIP_CHARS>    Avoid comparing the first N characters [default: 0]
  -u, --unique                     Only print unique lines
  -w, --check-chars <CHECK_CHARS>  Compare no more than N characters in lines
      --by <BY_FIELD>              Deduplicate NDJSON objects by named field
  -J, --json                       
      --help                       Print help
";

#[derive(Default)]
struct UniqArgs {
    count: bool,
    repeated: bool,
    skip_fields: usize,
    ignore_case: bool,
    skip_chars: usize,
    unique: bool,
    check_chars: Option<usize>,
    by_field: Option<String>,
    json: bool,
    input: Option<String>,
    output: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UniqOpt {
    Count,
    Repeated,
    IgnoreCase,
    Unique,
    SkipFields,
    SkipChars,
    CheckChars,
    By,
    Json,
    /// GNU's obsolete `-N`: skip N fields.
    Digit(u8),
    /// A GNU option Dogfish does not implement yet.
    Unsupported,
    Help,
    Version,
}

const fn digit(c: u8) -> OptSpec<UniqOpt> {
    OptSpec::short(c, HasArg::No, UniqOpt::Digit(c))
}

/// GNU's `longopts` and `"-0123456789Dcdf:is:uw:z"`, plus the Dogfish `--by`
/// and `-J, --json`.
const SPECS: &[OptSpec<UniqOpt>] = &[
    OptSpec::both(b'c', "count", HasArg::No, UniqOpt::Count),
    OptSpec::both(b'd', "repeated", HasArg::No, UniqOpt::Repeated),
    OptSpec::long("all-repeated", HasArg::Optional, UniqOpt::Unsupported),
    OptSpec::long("group", HasArg::Optional, UniqOpt::Unsupported),
    OptSpec::both(b'i', "ignore-case", HasArg::No, UniqOpt::IgnoreCase),
    OptSpec::both(b'u', "unique", HasArg::No, UniqOpt::Unique),
    OptSpec::both(b'f', "skip-fields", HasArg::Required, UniqOpt::SkipFields),
    OptSpec::both(b's', "skip-chars", HasArg::Required, UniqOpt::SkipChars),
    OptSpec::both(b'w', "check-chars", HasArg::Required, UniqOpt::CheckChars),
    OptSpec::both(b'z', "zero-terminated", HasArg::No, UniqOpt::Unsupported),
    OptSpec::long("help", HasArg::No, UniqOpt::Help),
    OptSpec::long("version", HasArg::No, UniqOpt::Version),
    OptSpec::long("by", HasArg::Required, UniqOpt::By).exact(),
    OptSpec::both(b'J', "json", HasArg::No, UniqOpt::Json).exact(),
    digit(b'0'),
    digit(b'1'),
    digit(b'2'),
    digit(b'3'),
    digit(b'4'),
    digit(b'5'),
    digit(b'6'),
    digit(b'7'),
    digit(b'8'),
    digit(b'9'),
    OptSpec::short(b'D', HasArg::No, UniqOpt::Unsupported),
];

/// GNU `xstrtoumax` with no suffixes: a decimal count, too large meaning the
/// largest.
fn count_arg(s: &str) -> Option<usize> {
    let t = s.trim_start();
    let t = t.strip_prefix('+').unwrap_or(t);
    if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(t.parse().unwrap_or(usize::MAX))
}

/// Parses the command line; `Err` is the exit status after `--help`,
/// `--version` or a diagnostic.
fn parse_uniq_args(args: Vec<OsString>) -> std::result::Result<UniqArgs, i32> {
    let mut p = UniqArgs::default();
    let mut files: Vec<String> = Vec::new();
    // Whether the field count came from `-N` (digits accumulate) or `-f`.
    let mut obsolete_fields = false;
    let size_opt = |v: &str, what: &str| {
        count_arg(v).ok_or_else(|| {
            eprintln!("uniq: {v}: {what}");
            1
        })
    };
    let mut opts = Getopt::with_order(args.into_iter().skip(1).collect(), SPECS, Order::InPlace);
    let posixly_correct = std::env::var_os("POSIXLY_CORRECT").is_some();
    while let Some(item) = opts.next_in_order() {
        let (o, v) = match item.map_err(|e| getopt_error("uniq", &e))? {
            Arg::Operand(arg) => {
                let arg = lossy(&arg);
                // GNU's obsolete `+N`: skip N characters.
                if let Some(n) = arg.strip_prefix('+').and_then(count_arg) {
                    p.skip_chars = n;
                } else if files.len() == 2 {
                    return Err(usage_error(
                        "uniq",
                        format!("extra operand {}", quote_locale(&arg)),
                    ));
                } else {
                    files.push(arg);
                }
                // Under POSIXLY_CORRECT, options end with the first file.
                if posixly_correct && !files.is_empty() {
                    while let Some(rest) = opts.take_next() {
                        let rest = lossy(&rest);
                        if files.len() == 2 {
                            return Err(usage_error(
                                "uniq",
                                format!("extra operand {}", quote_locale(&rest)),
                            ));
                        }
                        files.push(rest);
                    }
                    break;
                }
                continue;
            }
            Arg::Opt(o, v) => (o, v.as_deref().map(lossy).unwrap_or_default()),
        };
        match o {
            UniqOpt::Count => p.count = true,
            UniqOpt::Repeated => p.repeated = true,
            UniqOpt::IgnoreCase => p.ignore_case = true,
            UniqOpt::Unique => p.unique = true,
            UniqOpt::Digit(c) => {
                if !obsolete_fields {
                    p.skip_fields = 0;
                }
                p.skip_fields = p
                    .skip_fields
                    .checked_mul(10)
                    .and_then(|n| n.checked_add(usize::from(c - b'0')))
                    .unwrap_or(usize::MAX);
                obsolete_fields = true;
            }
            UniqOpt::SkipFields => {
                obsolete_fields = false;
                p.skip_fields = size_opt(&v, "invalid number of fields to skip")?;
            }
            UniqOpt::SkipChars => p.skip_chars = size_opt(&v, "invalid number of bytes to skip")?,
            UniqOpt::CheckChars => {
                p.check_chars = Some(size_opt(&v, "invalid number of bytes to compare")?)
            }
            UniqOpt::By => p.by_field = Some(v),
            UniqOpt::Json => p.json = true,
            UniqOpt::Unsupported => return Err(unsupported("uniq", &opts.last_spelling())),
            UniqOpt::Help => return Err(print_help(UNIQ_HELP)),
            UniqOpt::Version => return Err(print_version("uniq")),
        }
    }
    let mut files = files.into_iter();
    p.input = files.next();
    p.output = files.next();
    Ok(p)
}

fn uniq_key<'a>(line: &'a [u8], p: &UniqArgs) -> &'a [u8] {
    let mut i = 0;
    for _ in 0..p.skip_fields {
        while i < line.len() && is_blank(line[i]) {
            i += 1;
        }
        while i < line.len() && !is_blank(line[i]) {
            i += 1;
        }
    }
    let start = (i + p.skip_chars).min(line.len());
    let end = match p.check_chars {
        Some(n) => (start + n).min(line.len()),
        None => line.len(),
    };
    &line[start..end]
}

pub fn run_uniq(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_uniq_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let src = match parsed.input {
        Some(ref f) if f != "-" => InputSource::File(f.clone()),
        _ => InputSource::Stdin,
    };

    let mut reader = match FastReader::open(&src) {
        Ok(r) => r.into_bufread(),
        Err(e) => {
            eprintln!("uniq: {}: {}", src.display_name(), gnu_io_error(&e));
            return Ok(1);
        }
    };
    let mut first_line = Vec::new();
    if reader.read_until(b'\n', &mut first_line)? == 0 {
        return Ok(0);
    }

    if looks_like_json(&first_line) {
        let mut all = first_line.clone();
        reader.read_to_end(&mut all)?;
        if let Some(objs) = read_ndjson_stdin(&lossy_lines(&all)) {
            ctx.telemetry.set_strategy("NDJSON object deduplication");
            let field = parsed.by_field.as_deref().unwrap_or("name");
            let mut groups: Vec<(Value, usize)> = Vec::new();
            for obj in objs {
                if let Some((last, cnt)) = groups.last_mut()
                    && compare_json_field(last, &obj, field) == Ordering::Equal
                {
                    *cnt += 1;
                    continue;
                }
                groups.push((obj, 1));
            }
            let filtered: Vec<Value> = groups
                .into_iter()
                .filter(|(_, c)| {
                    if parsed.repeated {
                        *c > 1
                    } else if parsed.unique {
                        *c == 1
                    } else {
                        true
                    }
                })
                .map(|(mut val, c)| {
                    if parsed.count
                        && let Some(map) = val.as_object_mut()
                    {
                        map.insert("count".to_string(), serde_json::json!(c));
                    }
                    val
                })
                .collect();
            emit_objects_or_table(&filtered, ctx, parsed.json, !parsed.json)?;
            return Ok(0);
        }
        // Not NDJSON after all: dedupe the buffered text.
        reader = Box::new(io::Cursor::new(all));
        first_line.clear();
        reader.read_until(b'\n', &mut first_line)?;
    }

    // O(1) memory single-pass streaming deduplicator!
    ctx.telemetry
        .set_strategy("O(1) memory streaming deduplicator");
    let sink: Box<dyn Write> = match parsed.output.as_deref() {
        Some(path) if path != "-" => Box::new(std::fs::File::create(path)?),
        _ => Box::new(stdout::stdout()),
    };
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, sink);
    let mut prev = chomp(&first_line).to_vec();
    let mut count = 1usize;
    let mut cur = Vec::with_capacity(256);

    let emit_group =
        |out: &mut io::BufWriter<Box<dyn Write>>, line: &[u8], cnt: usize| -> io::Result<()> {
            if (parsed.repeated && cnt <= 1) || (parsed.unique && cnt != 1) {
                return Ok(());
            }
            if parsed.count {
                ignore_broken_pipe(write!(out, "{:7} ", cnt))?;
            }
            ignore_broken_pipe(out.write_all(line))?;
            ignore_broken_pipe(out.write_all(b"\n"))
        };

    loop {
        cur.clear();
        if reader.read_until(b'\n', &mut cur)? == 0 {
            break;
        }
        let line = chomp(&cur);
        let (ka, kb) = (uniq_key(&prev, &parsed), uniq_key(line, &parsed));
        let is_same = if parsed.ignore_case {
            ka.eq_ignore_ascii_case(kb)
        } else {
            ka == kb
        };
        if is_same {
            count += 1;
        } else {
            emit_group(&mut out, &prev, count)?;
            prev.clear();
            prev.extend_from_slice(line);
            count = 1;
        }
    }
    emit_group(&mut out, &prev, count)?;
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}
