//! uniq — GNU `uniq.c`: `-c/-d/-D/-u`, `--group`, `-f/-s/-w`, `-z`, the
//! obsolete `-N` / `+N` syntax, + NDJSON deduplication (`--by <field>`).

use std::cmp::Ordering;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};

use anyhow::Result;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, emit_objects_or_table, gnu_io_error, read_ndjson_stdin,
    table::compare_json_field,
};
use serde_json::Value;

use crate::common::getopt::{GetOpt, HasArg, OptSpec, argmatch, opt, print_help, usage_error};
use crate::common::gnu_getopt::print_tool_version;
use crate::common::input::RawStdout;
use crate::common::lines::{is_blank, looks_like_json, lossy_lines};
use crate::common::posixver::posix2_version;
use crate::common::quote::{qa, qf, quote_locale};
use crate::common::xstrtol::{StrtolError, xstrtoumax};

const UNIQ_HELP: &str = "\
Usage: uniq [OPTION]... [INPUT [OUTPUT]]
Filter adjacent matching lines from INPUT (or standard input),
writing to OUTPUT (or standard output).

With no options, matching lines are merged to the first occurrence.

Mandatory arguments to long options are mandatory for short options too.
  -c, --count           prefix lines by the number of occurrences
  -d, --repeated        only print duplicate lines, one for each group
  -D                    print all duplicate lines
      --all-repeated[=METHOD]  like -D, but allow separating groups
                                 with an empty line;
                                 METHOD={none(default),prepend,separate}
  -f, --skip-fields=N   avoid comparing the first N fields
      --group[=METHOD]  show all items, separating groups with an empty line;
                          METHOD={separate(default),prepend,append,both}
  -i, --ignore-case     ignore differences in case when comparing
  -s, --skip-chars=N    avoid comparing the first N characters
  -u, --unique          only print unique lines
  -z, --zero-terminated     line delimiter is NUL, not newline
  -w, --check-chars=N   compare no more than N characters in lines
      --by=FIELD        deduplicate NDJSON objects by FIELD (default: name)
  -J, --json            print NDJSON objects as JSON
      --help        display this help and exit
      --version     output version information and exit

A field is a run of blanks (usually spaces and/or TABs), then non-blank
characters.  Fields are skipped before chars.

Note: 'uniq' does not detect repeated lines unless they are adjacent.
You may want to sort the input first, or use 'sort -u' without 'uniq'.
";

static UNIQ_OPTS: [OptSpec; 25] = [
    opt(Some('0'), None, HasArg::No, "0"),
    opt(Some('1'), None, HasArg::No, "1"),
    opt(Some('2'), None, HasArg::No, "2"),
    opt(Some('3'), None, HasArg::No, "3"),
    opt(Some('4'), None, HasArg::No, "4"),
    opt(Some('5'), None, HasArg::No, "5"),
    opt(Some('6'), None, HasArg::No, "6"),
    opt(Some('7'), None, HasArg::No, "7"),
    opt(Some('8'), None, HasArg::No, "8"),
    opt(Some('9'), None, HasArg::No, "9"),
    opt(Some('c'), Some("count"), HasArg::No, "count"),
    opt(Some('d'), Some("repeated"), HasArg::No, "repeated"),
    opt(Some('D'), None, HasArg::No, "all-repeated"),
    opt(None, Some("all-repeated"), HasArg::Optional, "all-repeated"),
    opt(None, Some("group"), HasArg::Optional, "group"),
    opt(
        Some('f'),
        Some("skip-fields"),
        HasArg::Required,
        "skip-fields",
    ),
    opt(Some('i'), Some("ignore-case"), HasArg::No, "ignore-case"),
    opt(
        Some('s'),
        Some("skip-chars"),
        HasArg::Required,
        "skip-chars",
    ),
    opt(Some('u'), Some("unique"), HasArg::No, "unique"),
    opt(
        Some('w'),
        Some("check-chars"),
        HasArg::Required,
        "check-chars",
    ),
    opt(
        Some('z'),
        Some("zero-terminated"),
        HasArg::No,
        "zero-terminated",
    ),
    opt(None, Some("by"), HasArg::Required, "by"),
    opt(Some('J'), Some("json"), HasArg::No, "json"),
    opt(None, Some("help"), HasArg::No, "help"),
    opt(None, Some("version"), HasArg::No, "version"),
];

/// `--all-repeated=METHOD`: where blank lines separate groups of duplicates.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Delimit {
    None,
    Prepend,
    Separate,
}

/// `--group=METHOD`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Grouping {
    None,
    Separate,
    Prepend,
    Append,
    Both,
}

/// Everything `uniq` was asked to do (GNU's globals).
struct UniqConfig {
    skip_fields: u64,
    skip_chars: u64,
    check_chars: u64,
    ignore_case: bool,
    output_unique: bool,
    output_first_repeated: bool,
    output_later_repeated: bool,
    count: bool,
    delimit: Delimit,
    grouping: Grouping,
    delimiter: u8,
    input: String,
    output: String,
    by_field: Option<String>,
    json: bool,
}

/// GNU's `size_opt`: a count that may overflow (it saturates) but must be a
/// plain number.
fn size_opt(arg: &str, what: &str) -> std::result::Result<u64, i32> {
    match xstrtoumax(arg, "") {
        (n, None | Some(StrtolError::Overflow)) => Ok(n),
        _ => {
            eprintln!("uniq: {arg}: {what}");
            Err(1)
        }
    }
}

/// Parses the command line like GNU uniq's `getopt_long` loop with
/// `"-0123456789Dcdf:is:uw:z"`: operands are seen in order, so `+N` (skip
/// N characters) and digit options (`-3`, obsolete `-f 3`) work as in GNU.
/// `Err` is the exit status after a diagnostic (or `--help`).
fn parse_args(args: Vec<String>) -> std::result::Result<UniqConfig, i32> {
    let posixly_correct = std::env::var_os("POSIXLY_CORRECT").is_some();
    let strict_posix2 = (200112..200809).contains(&posix2_version());
    let mut cfg = UniqConfig {
        skip_fields: 0,
        skip_chars: 0,
        check_chars: u64::MAX,
        ignore_case: false,
        output_unique: true,
        output_first_repeated: true,
        output_later_repeated: false,
        count: false,
        delimit: Delimit::None,
        grouping: Grouping::None,
        delimiter: b'\n',
        input: "-".to_string(),
        output: "-".to_string(),
        by_field: None,
        json: false,
    };
    let mut files: Vec<String> = Vec::new();
    let mut fields_from_digits = false;
    let mut output_option_used = false;
    let mut rest: Vec<String> = args.into_iter().skip(1).collect();
    let add_file = |files: &mut Vec<String>, arg: String| {
        if files.len() == 2 {
            eprintln!("uniq: extra operand {}", quote_locale(&arg));
            return Err(usage_try());
        }
        files.push(arg);
        Ok(())
    };
    'parse: loop {
        let mut g = GetOpt::new(std::mem::take(&mut rest), &UNIQ_OPTS, false);
        loop {
            if posixly_correct && !files.is_empty() {
                for arg in g.into_operands() {
                    add_file(&mut files, arg)?;
                }
                break 'parse;
            }
            let end_of_options = g.peek() == Some("--");
            let (key, value) = match g.next_opt() {
                Some(Ok(kv)) => kv,
                Some(Err(msg)) => return Err(usage_error("uniq", msg, 1)),
                None => {
                    let mut operands = g.into_operands().into_iter();
                    if end_of_options {
                        for arg in operands {
                            add_file(&mut files, arg)?;
                        }
                        break 'parse;
                    }
                    let Some(arg) = operands.next() else {
                        break 'parse;
                    };
                    rest = operands.collect();
                    // GNU's "-" optstring: an operand is seen in place, and
                    // `+N` means skip N characters.
                    match xstrtoumax(&arg, "") {
                        (n, None) if arg.starts_with('+') && !strict_posix2 => cfg.skip_chars = n,
                        _ => add_file(&mut files, arg)?,
                    }
                    continue 'parse;
                }
            };
            let value = value.unwrap_or_default();
            match key {
                d if d.len() == 1 && d.as_bytes()[0].is_ascii_digit() => {
                    if !fields_from_digits {
                        cfg.skip_fields = 0;
                    }
                    let digit = u64::from(d.as_bytes()[0] - b'0');
                    cfg.skip_fields = cfg
                        .skip_fields
                        .checked_mul(10)
                        .and_then(|v| v.checked_add(digit))
                        .unwrap_or(u64::MAX);
                    fields_from_digits = true;
                }
                "count" => {
                    cfg.count = true;
                    output_option_used = true;
                }
                "repeated" => {
                    cfg.output_unique = false;
                    output_option_used = true;
                }
                "all-repeated" => {
                    cfg.output_unique = false;
                    cfg.output_later_repeated = true;
                    cfg.delimit = if value.is_empty() {
                        Delimit::None
                    } else {
                        let valid = ["none", "prepend", "separate"];
                        let i = argmatch("uniq", "--all-repeated", &value, &valid, 1)?;
                        [Delimit::None, Delimit::Prepend, Delimit::Separate][i]
                    };
                    output_option_used = true;
                }
                "group" => {
                    cfg.grouping = if value.is_empty() {
                        Grouping::Separate
                    } else {
                        let valid = ["prepend", "append", "separate", "both"];
                        let i = argmatch("uniq", "--group", &value, &valid, 1)?;
                        [
                            Grouping::Prepend,
                            Grouping::Append,
                            Grouping::Separate,
                            Grouping::Both,
                        ][i]
                    };
                }
                "skip-fields" => {
                    fields_from_digits = false;
                    cfg.skip_fields = size_opt(&value, "invalid number of fields to skip")?;
                }
                "ignore-case" => cfg.ignore_case = true,
                "skip-chars" => {
                    cfg.skip_chars = size_opt(&value, "invalid number of bytes to skip")?;
                }
                "unique" => {
                    cfg.output_first_repeated = false;
                    output_option_used = true;
                }
                "check-chars" => {
                    cfg.check_chars = size_opt(&value, "invalid number of bytes to compare")?;
                }
                "zero-terminated" => cfg.delimiter = 0,
                "by" => cfg.by_field = Some(value),
                "json" => cfg.json = true,
                "help" => {
                    let _ = print_help(UNIQ_HELP);
                    return Err(0);
                }
                "version" => {
                    print_tool_version("uniq");
                    return Err(0);
                }
                _ => {}
            }
        }
    }
    if cfg.grouping != Grouping::None && output_option_used {
        eprintln!("uniq: --group is mutually exclusive with -c/-d/-D/-u");
        return Err(usage_try());
    }
    if cfg.grouping != Grouping::None && cfg.count {
        eprintln!("uniq: grouping and printing repeat counts is meaningless");
        return Err(usage_try());
    }
    if cfg.count && cfg.output_later_repeated {
        eprintln!("uniq: printing all duplicated lines and repeat counts is meaningless");
        return Err(usage_try());
    }
    let mut files = files.into_iter();
    if let Some(f) = files.next() {
        cfg.input = f;
    }
    if let Some(f) = files.next() {
        cfg.output = f;
    }
    Ok(cfg)
}

fn usage_try() -> i32 {
    crate::common::getopt::try_help("uniq", 1)
}

/// The part of `line` (without its delimiter) that is compared: GNU's
/// `find_field`, then at most `check_chars` bytes.
fn compared_part<'a>(line: &'a [u8], cfg: &UniqConfig) -> &'a [u8] {
    let field_sep = |c: u8| is_blank(c) || c == b'\n';
    let size = line.len();
    let mut i = 0;
    let mut count = 0;
    while count < cfg.skip_fields && i < size {
        while i < size && field_sep(line[i]) {
            i += 1;
        }
        while i < size && !field_sep(line[i]) {
            i += 1;
        }
        count += 1;
    }
    i += usize::try_from(cfg.skip_chars)
        .unwrap_or(usize::MAX)
        .min(size - i);
    let len = usize::try_from(cfg.check_chars)
        .unwrap_or(usize::MAX)
        .min(size - i);
    &line[i..i + len]
}

fn different(a: &[u8], b: &[u8], cfg: &UniqConfig) -> bool {
    if cfg.ignore_case {
        !a.eq_ignore_ascii_case(b)
    } else {
        a != b
    }
}

/// Why filtering stopped: reading the input or writing the output failed.
enum UniqError {
    Read(io::Error),
    Write(io::Error),
}

impl From<io::Error> for UniqError {
    fn from(e: io::Error) -> Self {
        Self::Write(e)
    }
}

/// Reads the next record (GNU's `readlinebuffer_delim`): the delimiter is
/// supplied when the last record lacks one. False at end of input.
fn read_record(r: &mut impl BufRead, buf: &mut Vec<u8>, delim: u8) -> Result<bool, UniqError> {
    buf.clear();
    if r.read_until(delim, buf).map_err(UniqError::Read)? == 0 {
        return Ok(false);
    }
    if buf.last() != Some(&delim) {
        buf.push(delim);
    }
    Ok(true)
}

/// GNU's `check_file`: filters `input` to `out`.
fn check_file(
    input: &mut impl BufRead,
    out: &mut impl Write,
    cfg: &UniqConfig,
) -> Result<(), UniqError> {
    let delim = cfg.delimiter;
    let key = |line: &[u8]| -> std::ops::Range<usize> {
        let body = &line[..line.len() - 1];
        let part = compared_part(body, cfg);
        let start = part.as_ptr() as usize - body.as_ptr() as usize;
        start..start + part.len()
    };
    let mut this = Vec::new();
    let mut prev = Vec::new();

    if cfg.output_unique && cfg.output_first_repeated && !cfg.count {
        // Every group's first line is printed right away.
        let mut have_prev = false;
        let mut prev_key = 0..0;
        let mut first_group_printed = false;
        while read_record(input, &mut this, delim)? {
            let this_key = key(&this);
            let new_group =
                !have_prev || different(&this[this_key.clone()], &prev[prev_key.clone()], cfg);
            let g = cfg.grouping;
            if new_group
                && g != Grouping::None
                && (matches!(g, Grouping::Prepend | Grouping::Both)
                    || (first_group_printed && matches!(g, Grouping::Append | Grouping::Separate)))
            {
                out.write_all(&[delim])?;
            }
            if new_group || g != Grouping::None {
                out.write_all(&this)?;
                std::mem::swap(&mut prev, &mut this);
                prev_key = this_key;
                have_prev = true;
                first_group_printed = true;
            }
        }
        if matches!(cfg.grouping, Grouping::Both | Grouping::Append) && first_group_printed {
            out.write_all(&[delim])?;
        }
        return Ok(());
    }

    if !read_record(input, &mut prev, delim)? {
        return Ok(());
    }
    let mut prev_key = key(&prev);
    let mut match_count: u64 = 0;
    let mut first_delimiter = true;
    let writeline = |out: &mut dyn Write, line: &[u8], matched: bool, n: u64| -> io::Result<()> {
        let shown = if n == 0 {
            cfg.output_unique
        } else if !matched {
            cfg.output_first_repeated
        } else {
            cfg.output_later_repeated
        };
        if !shown {
            return Ok(());
        }
        if cfg.count {
            write!(out, "{:7} ", n.saturating_add(1))?;
        }
        out.write_all(line)
    };
    while read_record(input, &mut this, delim)? {
        let this_key = key(&this);
        let matched = !different(&this[this_key.clone()], &prev[prev_key.clone()], cfg);
        match_count += u64::from(matched);
        if match_count == u64::MAX {
            if cfg.count {
                out.flush()?;
                eprintln!("uniq: too many repeated lines");
                std::process::exit(1);
            }
            match_count -= 1;
        }
        if cfg.delimit != Delimit::None {
            if !matched {
                if match_count > 0 {
                    first_delimiter = false;
                }
            } else if match_count == 1
                && (cfg.delimit == Delimit::Prepend
                    || (cfg.delimit == Delimit::Separate && !first_delimiter))
            {
                out.write_all(&[delim])?;
            }
        }
        if !matched || cfg.output_later_repeated {
            writeline(out, &prev, matched, match_count)?;
            std::mem::swap(&mut prev, &mut this);
            prev_key = this_key;
            if !matched {
                match_count = 0;
            }
        }
    }
    writeline(out, &prev, false, match_count)?;
    Ok(())
}

pub fn run_uniq(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let cfg = match parse_args(args) {
        Ok(cfg) => cfg,
        Err(status) => return Ok(status),
    };
    let raw: Box<dyn Read> = if cfg.input == "-" {
        Box::new(io::stdin())
    } else {
        match File::open(&cfg.input) {
            Ok(f) => Box::new(f),
            Err(e) => {
                eprintln!("uniq: {}: {}", qf(&cfg.input), gnu_io_error(&e));
                return Ok(1);
            }
        }
    };
    let mut input = BufReader::with_capacity(DEFAULT_BUF_SIZE, raw);

    // Rich output: NDJSON objects are deduplicated by field on a terminal
    // (or with -J). Piped output is GNU's, byte for byte.
    if (ctx.stdout_is_tty || cfg.json) && cfg.delimiter == b'\n' && cfg.output == "-" {
        let peek = input.fill_buf()?;
        if looks_like_json(peek) {
            let mut all = Vec::new();
            input.read_to_end(&mut all)?;
            if let Some(objs) = read_ndjson_stdin(&lossy_lines(&all)) {
                return uniq_objects(objs, &cfg, ctx);
            }
            input = BufReader::with_capacity(DEFAULT_BUF_SIZE, Box::new(io::Cursor::new(all)));
        }
    }

    ctx.telemetry
        .set_strategy("O(1) memory streaming deduplicator");
    let sink: Box<dyn Write> = if cfg.output == "-" {
        Box::new(RawStdout)
    } else {
        match File::create(&cfg.output) {
            Ok(f) => Box::new(f),
            Err(e) => {
                eprintln!("uniq: {}: {}", qf(&cfg.output), gnu_io_error(&e));
                return Ok(1);
            }
        }
    };
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, sink);
    match check_file(&mut input, &mut out, &cfg) {
        Ok(()) => {}
        Err(UniqError::Write(e)) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(0),
        Err(UniqError::Write(e)) => return Err(e.into()),
        Err(UniqError::Read(e)) => {
            let _ = out.flush();
            eprintln!(
                "uniq: error reading {}: {}",
                qa(&cfg.input),
                gnu_io_error(&e)
            );
            return Ok(1);
        }
    }
    match out.flush() {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(0),
        res => res.map(|()| 0).map_err(Into::into),
    }
}

/// `--by FIELD`: groups adjacent NDJSON objects with equal FIELD.
fn uniq_objects(objs: Vec<Value>, cfg: &UniqConfig, ctx: &DogfishContext) -> Result<i32> {
    ctx.telemetry.set_strategy("NDJSON object deduplication");
    let field = cfg.by_field.as_deref().unwrap_or("name");
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
            if !cfg.output_unique {
                *c > 1
            } else if !cfg.output_first_repeated {
                *c == 1
            } else {
                true
            }
        })
        .map(|(mut val, c)| {
            if cfg.count
                && let Some(map) = val.as_object_mut()
            {
                map.insert("count".to_string(), serde_json::json!(c));
            }
            val
        })
        .collect();
    emit_objects_or_table(&filtered, ctx, cfg.json, !cfg.json)?;
    Ok(0)
}
