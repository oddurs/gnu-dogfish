//! uniq — Deduplicate lines or NDJSON objects (`--by <field>`)

use std::cmp::Ordering;
use std::io::{self, BufRead, Read, Write};

use anyhow::Result;
use clap::Parser;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, emit_objects_or_table, gnu_io_error,
    io::ignore_broken_pipe, read_ndjson_stdin, table::compare_json_field,
};
use serde_json::Value;

use crate::common::lines::{chomp, is_blank, looks_like_json, lossy_lines};

#[derive(Parser)]
#[command(
    name = "uniq",
    about = "Report or omit repeated lines or NDJSON objects"
)]
pub(crate) struct UniqArgs {
    /// Prefix lines by the number of occurrences
    #[arg(short = 'c', long = "count")]
    pub(crate) count: bool,

    /// Only print duplicate lines, one for each group
    #[arg(short = 'd', long = "repeated")]
    repeated: bool,

    /// Avoid comparing the first N fields
    #[arg(short = 'f', long = "skip-fields", default_value_t = 0)]
    skip_fields: usize,

    /// Ignore differences in case when comparing
    #[arg(short = 'i', long = "ignore-case")]
    pub(crate) ignore_case: bool,

    /// Avoid comparing the first N characters
    #[arg(short = 's', long = "skip-chars", default_value_t = 0)]
    skip_chars: usize,

    /// Only print unique lines
    #[arg(short = 'u', long = "unique")]
    pub(crate) unique: bool,

    /// Compare no more than N characters in lines
    #[arg(short = 'w', long = "check-chars")]
    check_chars: Option<usize>,

    /// Deduplicate NDJSON objects by named field
    #[arg(long = "by")]
    pub(crate) by_field: Option<String>,

    #[arg(short = 'J', long = "json")]
    pub(crate) json: bool,

    input: Option<String>,

    pub(crate) output: Option<String>,
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

pub fn run_uniq(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = UniqArgs::try_parse_from(args)?;
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
        _ => Box::new(io::stdout().lock()),
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
