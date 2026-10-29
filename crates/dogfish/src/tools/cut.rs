//! cut — Supports byte/char/field slicing AND NDJSON `--field name,size` projection!

use std::io::{self, Read, Write};

use anyhow::Result;
use clap::Parser;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, emit_objects_or_table,
    eval_json_where, gnu_io_error, io::ignore_broken_pipe, read_ndjson_stdin,
};
use memchr::memmem;
use serde_json::{Map, Value};

use crate::common::lines::{looks_like_json, lossy_lines, split_lines};

#[derive(Parser)]
#[command(
    name = "cut",
    about = "Remove sections from each line of files or project fields from NDJSON"
)]
pub(crate) struct CutArgs {
    /// Select only these bytes (e.g. 1-5, 2,4)
    #[arg(short = 'b', long = "bytes", allow_hyphen_values = true)]
    pub(crate) bytes: Option<String>,

    /// Select only these characters (bytes, as in GNU coreutils)
    #[arg(short = 'c', long = "characters", allow_hyphen_values = true)]
    pub(crate) chars: Option<String>,

    /// (ignored)
    #[arg(short = 'n')]
    no_split: bool,

    /// Select only these fields (1-indexed numbers or NDJSON field names like `name,size`)
    #[arg(
        short = 'f',
        long = "fields",
        visible_alias = "field",
        allow_hyphen_values = true
    )]
    fields: Option<String>,

    /// Use DELIM instead of TAB for field delimiter
    #[arg(short = 'd', long = "delimiter", allow_hyphen_values = true)]
    delimiter: Option<String>,

    /// Complement the set of selected bytes, characters or fields
    #[arg(long = "complement")]
    pub(crate) complement: bool,

    /// Do not print lines not containing delimiters
    #[arg(short = 's', long = "only-delimited")]
    only_delimited: bool,

    /// Output delimiter (defaults to input delimiter)
    #[arg(long = "output-delimiter")]
    output_delimiter: Option<String>,

    /// Filter NDJSON objects before projecting (e.g., `--where "size>1K"`)
    #[arg(short = 'w', long = "where")]
    pub(crate) where_expr: Option<String>,

    #[arg(short = 'J', long = "json")]
    pub(crate) json: bool,

    pub(crate) files: Vec<String>,
}

/// `-b` and `-c` are the same byte selection (as in GNU coreutils, which has
/// no multibyte `cut`); `-f` selects delimited fields.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum CutMode {
    Bytes,
    Fields,
}

pub(crate) struct CutSpec {
    mode: CutMode,
    /// Sorted, disjoint 1-based selected ranges (already complemented).
    ranges: Vec<(usize, usize)>,
    delim: Vec<u8>,
    out_delim: Vec<u8>,
    /// `--output-delimiter` was given: separate byte ranges with it.
    delimit_bytes: bool,
    only_delimited: bool,
}

impl CutSpec {
    fn selected(&self, pos: usize) -> bool {
        let idx = self.ranges.partition_point(|&(_, end)| end < pos);
        self.ranges.get(idx).is_some_and(|&(start, _)| start <= pos)
    }
}

/// Parses a GNU list (`1,3`, `2-5`, `-3`, `4-`) into sorted 1-based ranges,
/// merging only overlapping ones like GNU, so that `1-2,3-4` stays two ranges
/// for `--output-delimiter`.
fn parse_cut_list(spec: &str, mode: CutMode) -> std::result::Result<Vec<(usize, usize)>, String> {
    let what = if mode == CutMode::Fields {
        "fields are"
    } else {
        "byte/character positions are"
    };
    let mut ranges = Vec::new();
    for part in spec.split([',', ' ']).filter(|p| !p.is_empty()) {
        let num = |s: &str| -> std::result::Result<usize, String> {
            let n: usize = s.parse().map_err(|_| {
                format!(
                    "invalid {} value '{}'",
                    if mode == CutMode::Fields {
                        "field"
                    } else {
                        "byte/character position"
                    },
                    part
                )
            })?;
            if n == 0 {
                return Err(format!("{} numbered from 1", what));
            }
            Ok(n)
        };
        let (start, end) = match part.split_once('-') {
            Some(("", "")) => return Err("invalid range with no endpoint: -".to_string()),
            Some((s, e)) => (
                if s.is_empty() { 1 } else { num(s)? },
                if e.is_empty() { usize::MAX } else { num(e)? },
            ),
            None => {
                let n = num(part)?;
                (n, n)
            }
        };
        if start > end {
            return Err("invalid decreasing range".to_string());
        }
        ranges.push((start, end));
    }
    if ranges.is_empty() {
        return Err(format!("{} numbered from 1", what));
    }
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (s, e) in ranges {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    Ok(merged)
}

/// GNU `complement_rp`: the positions not covered by sorted, disjoint `ranges`.
fn complement_cut_ranges(ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(ranges.len() + 1);
    let mut next = 1;
    for &(start, end) in ranges {
        if start > next {
            out.push((next, start - 1));
        }
        next = end.saturating_add(1);
    }
    if ranges.last().is_some_and(|&(_, end)| end < usize::MAX) {
        out.push((next, usize::MAX));
    }
    out
}

fn cut_line<W: Write>(line: &[u8], spec: &CutSpec, out: &mut W) -> io::Result<()> {
    match spec.mode {
        CutMode::Bytes => {
            let mut printed = false;
            for &(start, end) in &spec.ranges {
                if start > line.len() {
                    break;
                }
                if printed && spec.delimit_bytes {
                    out.write_all(&spec.out_delim)?;
                }
                out.write_all(&line[start - 1..end.min(line.len())])?;
                printed = true;
            }
        }
        CutMode::Fields => {
            if memmem::find(line, &spec.delim).is_none() {
                if !spec.only_delimited {
                    out.write_all(line)?;
                    out.write_all(b"\n")?;
                }
                return Ok(());
            }
            let mut first = true;
            let mut rest = line;
            let mut idx = 1;
            loop {
                let (field, next) = match memmem::find(rest, &spec.delim) {
                    Some(p) => (&rest[..p], Some(&rest[p + spec.delim.len()..])),
                    None => (rest, None),
                };
                if spec.selected(idx) {
                    if !first {
                        out.write_all(&spec.out_delim)?;
                    }
                    first = false;
                    out.write_all(field)?;
                }
                match next {
                    Some(n) => rest = n,
                    None => break,
                }
                idx += 1;
            }
        }
    }
    out.write_all(b"\n")
}

fn cut_buffer<W: Write>(bytes: &[u8], spec: &CutSpec, out: &mut W) -> io::Result<()> {
    for line in split_lines(bytes) {
        cut_line(line, spec, out)?;
    }
    Ok(())
}

pub fn run_cut(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = CutArgs::try_parse_from(args)?;
    let sources = InputSource::from_args(&parsed.files);

    let mut all_stdin: Option<Vec<u8>> = None;
    // Check for NDJSON named-field projection (e.g., `dogfish ls -J | dogfish cut -f name,size`)
    if sources.len() == 1 && matches!(sources[0], InputSource::Stdin) && !ctx.stdin_is_tty {
        let mut all = Vec::new();
        io::stdin().lock().read_to_end(&mut all)?;
        if looks_like_json(&all)
            && let Some(mut objs) = read_ndjson_stdin(&lossy_lines(&all))
        {
            if let Some(ref expr) = parsed.where_expr {
                objs.retain(|o| eval_json_where(o, expr));
            }
            if let Some(ref f_spec) = parsed.fields {
                ctx.telemetry.set_strategy("NDJSON field projection");
                let requested: Vec<&str> = f_spec.split(',').map(|s| s.trim()).collect();
                let projected: Vec<Value> = objs
                    .into_iter()
                    .map(|obj| {
                        let mut m = Map::new();
                        if let Some(orig) = obj.as_object() {
                            for &k in &requested {
                                if let Some(v) = orig.get(k) {
                                    m.insert(k.to_string(), v.clone());
                                }
                            }
                        }
                        Value::Object(m)
                    })
                    .collect();
                emit_objects_or_table(&projected, ctx, parsed.json, !parsed.json)?;
                return Ok(0);
            } else if parsed.where_expr.is_some() {
                emit_objects_or_table(&objs, ctx, parsed.json, !parsed.json)?;
                return Ok(0);
            }
        }
        all_stdin = Some(all);
    }

    let usage_error = |msg: &str| {
        eprintln!("cut: {msg}\nTry 'cut --help' for more information.");
        Ok(1)
    };
    let delim = parsed.delimiter.clone().unwrap_or_else(|| "\t".to_string());
    if delim.chars().count() != 1 {
        return usage_error("the delimiter must be a single character");
    }
    let (mode, list) = match (&parsed.bytes, &parsed.chars, &parsed.fields) {
        (Some(l), None, None) | (None, Some(l), None) => (CutMode::Bytes, l),
        (None, None, Some(l)) => (CutMode::Fields, l),
        (None, None, None) => {
            return usage_error("you must specify a list of bytes, characters, or fields");
        }
        _ => return usage_error("only one type of list may be specified"),
    };
    if mode == CutMode::Bytes && parsed.delimiter.is_some() {
        return usage_error("an input delimiter may be specified only when operating on fields");
    }
    if mode == CutMode::Bytes && parsed.only_delimited {
        return usage_error(
            "suppressing non-delimited lines makes sense\n\tonly when operating on fields",
        );
    }
    let mut ranges = match parse_cut_list(list, mode) {
        Ok(r) => r,
        Err(msg) => return usage_error(&msg),
    };
    if parsed.complement {
        ranges = complement_cut_ranges(&ranges);
    }
    let spec = CutSpec {
        mode,
        ranges,
        delimit_bytes: parsed.output_delimiter.is_some(),
        out_delim: match parsed.output_delimiter.as_deref() {
            // GNU treats an empty `--output-delimiter` as a NUL byte.
            Some("") => vec![0],
            Some(d) => d.as_bytes().to_vec(),
            None => delim.as_bytes().to_vec(),
        },
        delim: delim.into_bytes(),
        only_delimited: parsed.only_delimited,
    };

    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());
    let mut exit_code = 0;
    if let Some(all) = all_stdin {
        ignore_broken_pipe(cut_buffer(&all, &spec, &mut out))?;
    } else {
        for src in &sources {
            let res = FastReader::open(src).and_then(|mut r| {
                r.with_bytes(|bytes| cut_buffer(bytes, &spec, &mut out))
                    .and_then(|r| r)
            });
            match res {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(exit_code),
                Err(e) => {
                    let _ = out.flush();
                    eprintln!("cut: {}: {}", src.display_name(), gnu_io_error(&e));
                    exit_code = 1;
                }
            }
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(exit_code)
}
