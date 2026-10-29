//! cut — GNU `cut.c` byte and field selection (`-d ''` is NUL, `-d '\n'`,
//! `-z`) AND NDJSON `--field name,size` projection!

use std::ffi::OsString;
use std::io::{self, BufRead, Read, Write};
use std::os::unix::ffi::OsStringExt;

use anyhow::Result;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, emit_objects_or_table,
    eval_json_where, gnu_io_error, io::ignore_broken_pipe, read_ndjson_stdin, stdout,
};
use memchr::memchr2;
use serde_json::{Map, Value};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy_all, print_help, print_version, usage_error,
};
use crate::common::input::RawStdout;
use crate::common::lines::{line_end, looks_like_json, lossy_lines, split_records};
use crate::common::quote::{qf, quote_locale};

const CUT_HELP: &str = "\
Remove sections from each line of files or project fields from NDJSON

Usage: cut [OPTIONS] [FILES]...

Arguments:
  [FILES]...  

Options:
  -b, --bytes <BYTES>
          Select only these bytes (e.g. 1-5, 2,4)
  -c, --characters <CHARS>
          Select only these characters (bytes, as in GNU coreutils)
  -n
          (ignored)
  -f, --fields <FIELDS>
          Select only these fields (1-indexed numbers or NDJSON field names like `name,size`)
          [alias: --field]
  -d, --delimiter <DELIMITER>
          Use DELIM instead of TAB for field delimiter
      --complement
          Complement the set of selected bytes, characters or fields
  -s, --only-delimited
          Do not print lines not containing delimiters
  -z, --zero-terminated
          Line delimiter is NUL, not newline
      --output-delimiter <OUTPUT_DELIMITER>
          Output delimiter (defaults to input delimiter)
  -w, --where <WHERE_EXPR>
          Filter NDJSON objects before projecting (e.g., `--where \"size>1K\"`)
  -J, --json
          
      --help
          Print help
";

#[derive(Default)]
struct CutArgs {
    bytes: Option<String>,
    chars: Option<String>,
    fields: Option<String>,
    /// One byte, as in GNU (`-d ''` is NUL).
    delimiter: Option<Vec<u8>>,
    complement: bool,
    only_delimited: bool,
    output_delimiter: Option<Vec<u8>>,
    zero_terminated: bool,
    where_expr: Option<String>,
    json: bool,
    files: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CutOpt {
    Bytes,
    Chars,
    Fields,
    Delimiter,
    OnlyDelimited,
    OutputDelimiter,
    Complement,
    NoSplit,
    ZeroTerminated,
    Where,
    Json,
    Help,
    Version,
}

/// GNU's `longopts` and `"b:c:d:f:nsz"`, plus the Dogfish `--field`,
/// `-w, --where` and `-J, --json`.
const SPECS: &[OptSpec<CutOpt>] = &[
    OptSpec::both(b'b', "bytes", HasArg::Required, CutOpt::Bytes),
    OptSpec::both(b'c', "characters", HasArg::Required, CutOpt::Chars),
    OptSpec::both(b'f', "fields", HasArg::Required, CutOpt::Fields),
    OptSpec::both(b'd', "delimiter", HasArg::Required, CutOpt::Delimiter),
    OptSpec::both(b's', "only-delimited", HasArg::No, CutOpt::OnlyDelimited),
    OptSpec::long(
        "output-delimiter",
        HasArg::Required,
        CutOpt::OutputDelimiter,
    ),
    OptSpec::long("complement", HasArg::No, CutOpt::Complement),
    OptSpec::both(b'z', "zero-terminated", HasArg::No, CutOpt::ZeroTerminated),
    OptSpec::long("help", HasArg::No, CutOpt::Help),
    OptSpec::long("version", HasArg::No, CutOpt::Version),
    OptSpec::long("field", HasArg::Required, CutOpt::Fields).exact(),
    OptSpec::both(b'w', "where", HasArg::Required, CutOpt::Where).exact(),
    OptSpec::both(b'J', "json", HasArg::No, CutOpt::Json).exact(),
    OptSpec::short(b'n', HasArg::No, CutOpt::NoSplit),
];

/// Parses the command line; `Err` is the exit status after `--help`,
/// `--version` or a diagnostic.
fn parse_cut_args(args: Vec<OsString>) -> std::result::Result<CutArgs, i32> {
    let mut p = CutArgs::default();
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    while let Some(opt) = opts.next() {
        let (o, v) = opt.map_err(|e| getopt_error("cut", &e))?;
        let raw = v.map(OsString::into_vec).unwrap_or_default();
        let text = String::from_utf8_lossy(&raw).into_owned();
        match o {
            CutOpt::Bytes | CutOpt::Chars | CutOpt::Fields => {
                // GNU checks this as it meets the second list.
                if p.bytes.is_some() || p.chars.is_some() || p.fields.is_some() {
                    return Err(usage_error("cut", "only one list may be specified"));
                }
                *match o {
                    CutOpt::Bytes => &mut p.bytes,
                    CutOpt::Chars => &mut p.chars,
                    _ => &mut p.fields,
                } = Some(text);
            }
            CutOpt::Delimiter => {
                if raw.len() > 1 {
                    return Err(usage_error(
                        "cut",
                        "the delimiter must be a single character",
                    ));
                }
                p.delimiter = Some(vec![raw.first().copied().unwrap_or(0)]);
            }
            CutOpt::OnlyDelimited => p.only_delimited = true,
            CutOpt::OutputDelimiter => p.output_delimiter = Some(raw),
            CutOpt::Complement => p.complement = true,
            CutOpt::NoSplit => {}
            CutOpt::Where => p.where_expr = Some(text),
            CutOpt::Json => p.json = true,
            CutOpt::ZeroTerminated => p.zero_terminated = true,
            CutOpt::Help => return Err(print_help(CUT_HELP)),
            CutOpt::Version => return Err(print_version("cut")),
        }
    }
    p.files = lossy_all(opts.operands());
    Ok(p)
}

/// What to cut, after option processing (GNU's globals).
struct CutSpec {
    /// `-b`/`-c` (bytes, as GNU has no multibyte `cut`) rather than `-f`.
    byte_mode: bool,
    /// Sorted, disjoint 1-based selected ranges (already complemented).
    ranges: Vec<(u64, u64)>,
    delim: u8,
    out_delim: Vec<u8>,
    /// `--output-delimiter` was given: separate byte ranges with it.
    delimit_bytes: bool,
    suppress_non_delimited: bool,
    line_delim: u8,
}

impl CutSpec {
    fn print_kth(&self, k: u64) -> bool {
        let idx = self.ranges.partition_point(|&(_, hi)| hi < k);
        self.ranges.get(idx).is_some_and(|&(lo, _)| lo <= k)
    }
}

/// A `set_fields` failure: GNU prints the message, then the `--help` hint.
type ListError = String;

/// GNU's `set_fields`: parses a list (`1,3`, `2-5`, `-3`, `4-`, blanks as
/// separators) into sorted ranges, merging only overlapping ones (so
/// `1-2,3-4` stays two ranges for `--output-delimiter`), then complements.
fn set_fields(spec: &str, byte_mode: bool, complement: bool) -> Result<Vec<(u64, u64)>, ListError> {
    let pos_msg = |pos: &str, fld: &str| {
        if byte_mode {
            pos.to_string()
        } else {
            fld.to_string()
        }
    };
    let numbered_from_1 = || {
        pos_msg(
            "byte/character positions are numbered from 1",
            "fields are numbered from 1",
        )
    };
    let bytes = spec.as_bytes();
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    let mut initial: u64 = 1;
    let mut value: u64 = 0;
    let mut lhs_specified = false;
    let mut rhs_specified = false;
    let mut dash_found = false;
    let mut in_digits = false;
    let mut num_start = 0;
    let mut i = 0;
    loop {
        let c = bytes.get(i).copied();
        match c {
            Some(b'-') => {
                in_digits = false;
                if dash_found {
                    return Err(pos_msg(
                        "invalid byte or character range",
                        "invalid field range",
                    ));
                }
                dash_found = true;
                i += 1;
                if lhs_specified && value == 0 {
                    return Err(numbered_from_1());
                }
                initial = if lhs_specified { value } else { 1 };
                value = 0;
            }
            None | Some(b',') | Some(b' ') | Some(b'\t') => {
                in_digits = false;
                if dash_found {
                    dash_found = false;
                    if !lhs_specified && !rhs_specified {
                        return Err("invalid range with no endpoint: -".to_string());
                    }
                    if !rhs_specified {
                        ranges.push((initial, u64::MAX));
                    } else {
                        if value < initial {
                            return Err("invalid decreasing range".to_string());
                        }
                        ranges.push((initial, value));
                    }
                    value = 0;
                } else {
                    if value == 0 {
                        return Err(numbered_from_1());
                    }
                    ranges.push((value, value));
                    value = 0;
                }
                if c.is_none() {
                    break;
                }
                i += 1;
                lhs_specified = false;
                rhs_specified = false;
            }
            Some(d) if d.is_ascii_digit() => {
                if !in_digits {
                    num_start = i;
                }
                in_digits = true;
                if dash_found {
                    rhs_specified = true;
                } else {
                    lhs_specified = true;
                }
                match value
                    .checked_mul(10)
                    .and_then(|v| v.checked_add(u64::from(d - b'0')))
                {
                    Some(v) if v != u64::MAX => value = v,
                    _ => {
                        let len = bytes[num_start..]
                            .iter()
                            .take_while(|b| b.is_ascii_digit())
                            .count();
                        let bad = quote_locale(&spec[num_start..num_start + len]);
                        return Err(if byte_mode {
                            format!("byte/character offset {bad} is too large")
                        } else {
                            format!("field number {bad} is too large")
                        });
                    }
                }
                i += 1;
            }
            Some(_) => {
                let rest = quote_locale(String::from_utf8_lossy(&bytes[i..]).as_ref());
                return Err(if byte_mode {
                    format!("invalid byte/character position {rest}")
                } else {
                    format!("invalid field value {rest}")
                });
            }
        }
    }
    if ranges.is_empty() {
        return Err(pos_msg(
            "missing list of byte/character positions",
            "missing list of fields",
        ));
    }
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (lo, hi) in ranges {
        match merged.last_mut() {
            Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    if complement {
        merged = complement_ranges(&merged);
    }
    Ok(merged)
}

/// GNU `complement_rp`: the positions not covered by sorted, disjoint `ranges`.
fn complement_ranges(ranges: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut out = Vec::with_capacity(ranges.len() + 1);
    if let Some(&(lo, _)) = ranges.first()
        && lo > 1
    {
        out.push((1, lo - 1));
    }
    for pair in ranges.windows(2) {
        if pair[0].1 + 1 != pair[1].0 {
            out.push((pair[0].1 + 1, pair[1].0 - 1));
        }
    }
    if let Some(&(_, hi)) = ranges.last()
        && hi < u64::MAX
    {
        out.push((hi + 1, u64::MAX));
    }
    out
}

/// GNU's `cut_bytes` over a whole input.
fn cut_bytes<W: Write>(data: &[u8], spec: &CutSpec, out: &mut W) -> io::Result<()> {
    for line in split_records(data, spec.line_delim) {
        let len = line.len() as u64;
        let mut printed = false;
        for &(lo, hi) in &spec.ranges {
            if lo > len {
                break;
            }
            if printed && spec.delimit_bytes {
                out.write_all(&spec.out_delim)?;
            }
            out.write_all(&line[(lo - 1) as usize..hi.min(len) as usize])?;
            printed = true;
        }
        out.write_all(&[spec.line_delim])?;
    }
    Ok(())
}

/// GNU's `cut_fields` over a whole input, `getc`/`ungetc` replaced by a
/// cursor. Keeps GNU's corner cases: a non-delimited line is printed whole
/// (unless `-s`), and with `-d '\n'` the final newline is not a delimiter.
fn cut_fields<W: Write>(data: &[u8], spec: &CutSpec, out: &mut W) -> io::Result<()> {
    const EOF: i32 = -1;
    let (delim, line_delim) = (spec.delim, spec.line_delim);
    if data.is_empty() {
        return Ok(());
    }
    let mut pos = 0usize;
    let mut field_idx: u64 = 1;
    let mut found_any_selected_field = false;
    let buffer_first_field = spec.suppress_non_delimited ^ !spec.print_kth(1);
    let mut c: i32 = 0;
    loop {
        if field_idx == 1 && buffer_first_field {
            if pos >= data.len() {
                break;
            }
            // getndelim2: up to and including the first delim or line_delim.
            let end = memchr2(delim, line_delim, &data[pos..]).map_or(data.len(), |p| pos + p + 1);
            let field = &data[pos..end];
            pos = end;
            c = 0;
            let last = field[field.len() - 1];
            if last != delim {
                // Not delimited: the whole line, unless -s.
                if !spec.suppress_non_delimited {
                    out.write_all(field)?;
                    if last != line_delim {
                        out.write_all(&[line_delim])?;
                    }
                    c = i32::from(line_delim);
                }
                continue;
            }
            if spec.print_kth(1) {
                out.write_all(&field[..field.len() - 1])?;
                // With -d$'\n' the last '\n' is not a delimiter.
                if delim != line_delim || pos < data.len() {
                    found_any_selected_field = true;
                }
            }
            field_idx += 1;
        }

        let mut prev_c = c;
        let selected = spec.print_kth(field_idx);
        if selected {
            if found_any_selected_field {
                out.write_all(&spec.out_delim)?;
            }
            found_any_selected_field = true;
        }
        let rest = &data[pos..];
        let run = memchr2(delim, line_delim, rest).unwrap_or(rest.len());
        if selected {
            out.write_all(&rest[..run])?;
        }
        if run > 0 {
            prev_c = i32::from(rest[run - 1]);
        }
        pos += run;
        c = match data.get(pos) {
            Some(&b) => {
                pos += 1;
                i32::from(b)
            }
            None => EOF,
        };

        if delim == line_delim && c == i32::from(delim) && pos >= data.len() {
            c = EOF;
        }
        if c == i32::from(delim) {
            field_idx += 1;
        } else if c == i32::from(line_delim) || c == EOF {
            if (found_any_selected_field || !(spec.suppress_non_delimited && field_idx == 1))
                && (c == i32::from(line_delim)
                    || prev_c != i32::from(line_delim)
                    || delim == line_delim)
            {
                out.write_all(&[line_delim])?;
            }
            if c == EOF {
                break;
            }
            field_idx = 1;
            found_any_selected_field = false;
        }
    }
    Ok(())
}

fn cut_buffer<W: Write>(bytes: &[u8], spec: &CutSpec, out: &mut W) -> io::Result<()> {
    if spec.byte_mode {
        cut_bytes(bytes, spec, out)
    } else {
        cut_fields(bytes, spec, out)
    }
}

/// [`cut_buffer`] over a stream, a run of whole records at a time, so that
/// an endless input (`cut -z -c1- /dev/zero`) is cut as it is read and a
/// write error stops it.
fn cut_stream<W: Write>(r: &mut impl BufRead, spec: &CutSpec, out: &mut W) -> io::Result<()> {
    let mut partial: Vec<u8> = Vec::new();
    // A field delimiter equal to the line delimiter makes the whole input
    // one record.
    if !spec.byte_mode && spec.delim == spec.line_delim {
        r.read_to_end(&mut partial)?;
        return cut_buffer(&partial, spec, out);
    }
    loop {
        let buf = match r.fill_buf() {
            Ok(buf) => buf,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if buf.is_empty() {
            break;
        }
        let n = buf.len();
        match memchr::memrchr(spec.line_delim, buf) {
            Some(last) if partial.is_empty() => {
                cut_buffer(&buf[..=last], spec, out)?;
                partial.extend_from_slice(&buf[last + 1..]);
            }
            Some(last) => {
                partial.extend_from_slice(&buf[..=last]);
                cut_buffer(&partial, spec, out)?;
                partial.clear();
                partial.extend_from_slice(&buf[last + 1..]);
            }
            None => partial.extend_from_slice(buf),
        }
        r.consume(n);
    }
    if partial.is_empty() {
        Ok(())
    } else {
        cut_buffer(&partial, spec, out)
    }
}

/// Option checks in GNU's order; `Err` is the message for a usage error.
fn cut_spec(parsed: &CutArgs) -> Result<CutSpec, ListError> {
    // A second list and a long delimiter were refused while parsing.
    let lists = [&parsed.bytes, &parsed.chars, &parsed.fields];
    let delim = parsed
        .delimiter
        .as_deref()
        .map(|d| d.first().copied().unwrap_or(0));
    let byte_mode = parsed.fields.is_none();
    let Some(list) = lists.iter().find_map(|l| l.as_deref()) else {
        return Err("you must specify a list of bytes, characters, or fields".to_string());
    };
    if byte_mode {
        if delim.is_some() {
            return Err(
                "an input delimiter may be specified only when operating on fields".to_string(),
            );
        }
        if parsed.only_delimited {
            return Err(
                "suppressing non-delimited lines makes sense\n\tonly when operating on fields"
                    .to_string(),
            );
        }
    }
    let ranges = set_fields(list, byte_mode, parsed.complement)?;
    let delim = delim.unwrap_or(b'\t');
    Ok(CutSpec {
        byte_mode,
        ranges,
        delim,
        out_delim: match parsed.output_delimiter.as_deref() {
            // GNU treats an empty `--output-delimiter` as a NUL byte.
            Some([]) => vec![0],
            Some(d) => d.to_vec(),
            None => vec![delim],
        },
        delimit_bytes: parsed.output_delimiter.is_some(),
        suppress_non_delimited: parsed.only_delimited,
        line_delim: line_end(parsed.zero_terminated),
    })
}

pub fn run_cut(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_cut_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let sources = InputSource::from_args(&parsed.files);
    let spec = cut_spec(&parsed);

    // NDJSON named-field projection (`dogfish ls -J | dogfish cut -f name,size`):
    // a field list GNU would reject, -w, or -J.
    let mut all_stdin: Option<Vec<u8>> = None;
    let json_fields = parsed.fields.is_some() && spec.is_err();
    if sources.len() == 1
        && matches!(sources[0], InputSource::Stdin)
        && !ctx.stdin_is_tty()
        && !parsed.zero_terminated
        && (json_fields || parsed.where_expr.is_some() || parsed.json)
    {
        let mut all = Vec::new();
        io::stdin().lock().read_to_end(&mut all)?;
        if looks_like_json(&all)
            && let Some(objs) = read_ndjson_stdin(&lossy_lines(&all))
        {
            return cut_objects(objs, &parsed, ctx);
        }
        all_stdin = Some(all);
    }

    let spec = match spec {
        Ok(spec) => spec,
        Err(msg) => {
            eprintln!("cut: {msg}\nTry 'cut --help' for more information.");
            return Ok(1);
        }
    };

    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, RawStdout);
    let mut exit_code = 0;
    if let Some(all) = all_stdin {
        ignore_broken_pipe(cut_buffer(&all, &spec, &mut out))?;
    } else {
        for src in &sources {
            let res = FastReader::open(src).and_then(|r| match r {
                FastReader::Mapped(map) => cut_buffer(&map, &spec, &mut out),
                FastReader::Buffered(mut r) => cut_stream(&mut r, &spec, &mut out),
            });
            match res {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(exit_code),
                Err(e) if stdout::is_write_error(&e) => return Err(e.into()),
                Err(e) => {
                    let _ = out.flush();
                    eprintln!("cut: {}: {}", qf(src.display_name()), gnu_io_error(&e));
                    exit_code = 1;
                }
            }
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(exit_code)
}

/// `-f name,size` / `--where` over NDJSON objects.
fn cut_objects(mut objs: Vec<Value>, parsed: &CutArgs, ctx: &DogfishContext) -> Result<i32> {
    if let Some(ref expr) = parsed.where_expr {
        objs.retain(|o| eval_json_where(o, expr));
    }
    if let Some(ref f_spec) = parsed.fields {
        ctx.telemetry.set_strategy("NDJSON field projection");
        let requested: Vec<&str> = f_spec.split(',').map(|s| s.trim()).collect();
        objs = objs
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
    }
    emit_objects_or_table(&objs, ctx, parsed.json, !parsed.json)?;
    Ok(0)
}

/// cut's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("cut", CUT_HELP, SPECS)
}

#[cfg(test)]
mod tests {
    use super::set_fields;
    use crate::common::quote::quote_locale;

    #[test]
    fn lists() {
        assert_eq!(set_fields("1,3-4", false, false), Ok(vec![(1, 1), (3, 4)]));
        assert_eq!(set_fields("2-,1-2", false, false), Ok(vec![(1, u64::MAX)]));
        assert_eq!(set_fields("1-2,3", false, true), Ok(vec![(4, u64::MAX)]));
        assert_eq!(
            set_fields("-", true, false),
            Err("invalid range with no endpoint: -".to_string())
        );
        assert_eq!(
            set_fields("1--2", true, false),
            Err("invalid byte or character range".to_string())
        );
        assert_eq!(
            set_fields("18446744073709551615", true, false),
            Err(format!(
                "byte/character offset {} is too large",
                quote_locale("18446744073709551615")
            ))
        );
        assert!(set_fields("18446744073709551614-", true, false).is_ok());
    }
}

/// Entry points for the fuzz targets in `fuzz/` (only with the `fuzzing` feature).
#[cfg(feature = "fuzzing")]
#[allow(dead_code)]
pub mod fuzz {
    use super::*;

    /// `cut -b`/`-f LIST` (with and without `--complement`): ranges come out
    /// sorted, 1-based and non-empty.
    pub fn list(spec: &str) {
        for byte_mode in [false, true] {
            for complement in [false, true] {
                if let Ok(ranges) = set_fields(spec, byte_mode, complement) {
                    for w in ranges.windows(2) {
                        assert!(w[0].1 < w[1].0, "{spec:?}: {ranges:?}");
                    }
                    for &(lo, hi) in &ranges {
                        assert!(1 <= lo && lo <= hi, "{spec:?}: {ranges:?}");
                    }
                    let _ = complement_ranges(&ranges);
                }
            }
        }
    }

    pub fn args(args: Vec<OsString>) {
        let _ = parse_cut_args(args);
    }
}
