use std::collections::VecDeque;
use std::io::{self, BufRead, Write};
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use clap::Parser;
use dogfish_core::{
    ColumnAlign, DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, Table,
    emit_objects_or_table, eval_json_where,
    io::ignore_broken_pipe,
    parse_size_str, read_ndjson_stdin, sparkline_bar,
    table::{compare_json_field, read_all_stdin_lines},
};
use memchr::{memchr_iter, memrchr_iter};
use rayon::prelude::*;
use serde::Serialize;
use serde_json::{Map, Value};

// ---------------------------------------------------------------------------
// 1. cat — Zero-copy mmap + line numbering / squeeze-blank / show-ends
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "cat", about = "Concatenate FILE(s) to standard output")]
struct CatArgs {
    /// Number all output lines
    #[arg(short = 'n', long = "number")]
    number: bool,

    /// Number nonempty output lines, overrides -n
    #[arg(short = 'b', long = "number-nonblank")]
    number_nonblank: bool,

    /// Suppress repeated empty output lines
    #[arg(short = 's', long = "squeeze-blank")]
    squeeze_blank: bool,

    /// Display $ at end of each line
    #[arg(short = 'E', long = "show-ends")]
    show_ends: bool,

    /// Display TAB characters as ^I
    #[arg(short = 'T', long = "show-tabs")]
    show_tabs: bool,

    /// Files to concatenate
    files: Vec<String>,
}

pub fn run_cat(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = CatArgs::try_parse_from(args)?;
    let sources = InputSource::from_args(&parsed.files);
    let plain_fast_path = !parsed.number
        && !parsed.number_nonblank
        && !parsed.squeeze_blank
        && !parsed.show_ends
        && !parsed.show_tabs;

    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());
    let mut line_num = 1_usize;
    let mut prev_blank = false;
    let mut exit_code = 0;

    ctx.telemetry.set_strategy(if plain_fast_path {
        "zero-copy mmap slice write"
    } else {
        "line-decorated stream"
    });

    for src in &sources {
        let mut reader = match FastReader::open(src) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("cat: {}: {}", src.display_name(), e);
                exit_code = 1;
                continue;
            }
        };

        if plain_fast_path {
            reader.with_bytes(|bytes| -> io::Result<()> {
                ctx.telemetry.add_read(bytes.len() as u64);
                ignore_broken_pipe(out.write_all(bytes))?;
                ctx.telemetry.add_written(bytes.len() as u64);
                Ok(())
            })??;
        } else {
            let buf_reader = reader.into_bufread();
            for line_res in buf_reader.lines() {
                let line = line_res?;
                ctx.telemetry.add_read(line.len() as u64 + 1);
                let is_blank = line.is_empty();
                if parsed.squeeze_blank && is_blank && prev_blank {
                    continue;
                }
                prev_blank = is_blank;

                if parsed.number_nonblank {
                    if !is_blank {
                        write!(out, "{:6}\t", line_num)?;
                        line_num += 1;
                    }
                } else if parsed.number {
                    write!(out, "{:6}\t", line_num)?;
                    line_num += 1;
                }

                if parsed.show_tabs {
                    write!(out, "{}", line.replace('\t', "^I"))?;
                } else {
                    write!(out, "{}", line)?;
                }

                if parsed.show_ends {
                    writeln!(out, "$")?;
                } else {
                    writeln!(out)?;
                }
            }
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(exit_code)
}

// ---------------------------------------------------------------------------
// 2. nl — Number lines of files
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "nl", about = "Write each FILE to standard output, with line numbers added")]
struct NlArgs {
    /// Use NUMBER columns for line numbers
    #[arg(short = 'w', long = "number-width", default_value_t = 6)]
    width: usize,

    /// Add STRING after (possible) line number
    #[arg(short = 's', long = "number-separator", default_value = "\t")]
    separator: String,

    /// Number all lines (including empty ones; equivalent to -ba)
    #[arg(short = 'a', long = "all")]
    all: bool,

    files: Vec<String>,
}

pub fn run_nl(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = NlArgs::try_parse_from(args)?;
    let sources = InputSource::from_args(&parsed.files);
    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());
    let mut line_no = 1_usize;
    let st = ctx.style();

    for src in &sources {
        let reader = FastReader::open(src)?.into_bufread();
        for line in reader.lines() {
            let line = line?;
            ctx.telemetry.add_read(line.len() as u64 + 1);
            if line.is_empty() && !parsed.all {
                writeln!(out)?;
            } else {
                let num_str = format!("{:>width$}", line_no, width = parsed.width);
                writeln!(out, "{}{}{}", st.dim(&num_str), parsed.separator, line)?;
                line_no += 1;
            }
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// 3. head — SIMD newline scan + NDJSON auto-table rendering!
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "head", about = "Output the first part of files (or NDJSON object streams)")]
struct HeadArgs {
    /// Print the first NUM lines/objects instead of the first 10
    #[arg(short = 'n', long = "lines", default_value_t = 10)]
    lines: usize,

    /// Print the first NUM bytes of each file
    #[arg(short = 'c', long = "bytes")]
    bytes: Option<String>,

    /// Never print headers giving file names
    #[arg(short = 'q', long = "quiet", visible_alias = "silent")]
    quiet: bool,

    /// Always print headers giving file names
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    /// Force JSON output when processing NDJSON
    #[arg(short = 'J', long = "json")]
    json: bool,

    files: Vec<String>,
}

pub fn run_head(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = HeadArgs::try_parse_from(normalize_legacy_num_arg(args));
    let parsed = parsed?;
    let sources = InputSource::from_args(&parsed.files);

    // Adaptive NDJSON detection when reading from piped stdin!
    if sources.len() == 1 && matches!(sources[0], InputSource::Stdin) && parsed.bytes.is_none() && !ctx.stdin_is_tty {
        let all_lines = read_all_stdin_lines()?;
        if let Some(objs) = read_ndjson_stdin(&all_lines) {
            ctx.telemetry.set_strategy("NDJSON object stream + auto-table");
            let slice = &objs[..objs.len().min(parsed.lines)];
            ctx.telemetry.add_items(slice.len() as u64);
            emit_objects_or_table(slice, ctx, parsed.json, !parsed.json)?;
            return Ok(0);
        }
        // Plain text fallback for already-read lines
        let stdout = io::stdout();
        let mut out = io::BufWriter::new(stdout.lock());
        for line in all_lines.into_iter().take(parsed.lines) {
            ignore_broken_pipe(writeln!(out, "{}", line))?;
        }
        ignore_broken_pipe(out.flush())?;
        return Ok(0);
    }

    let byte_limit = match parsed.bytes {
        Some(ref b) => Some(parse_size_str(b).map_err(|e| anyhow!(e))? as usize),
        None => None,
    };

    ctx.telemetry.set_strategy("64KiB chunk + memchr SIMD");
    let show_headers = parsed.verbose || (!parsed.quiet && sources.len() > 1);
    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());

    for (idx, src) in sources.iter().enumerate() {
        if show_headers {
            if idx > 0 {
                writeln!(out)?;
            }
            writeln!(out, "==> {} <==", src.display_name())?;
        }
        match src {
            InputSource::File(p) => {
                use std::io::Read;
                let mut f = std::fs::File::open(p)?;
                let mut buf = [0u8; DEFAULT_BUF_SIZE];
                let mut lines_left = parsed.lines;
                let mut bytes_left = byte_limit.unwrap_or(usize::MAX);
                while lines_left > 0 && bytes_left > 0 {
                    let n = f.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    let slice = &buf[..n.min(bytes_left)];
                    let mut end = slice.len();
                    if byte_limit.is_none() {
                        for pos in memchr_iter(b'\n', slice) {
                            lines_left -= 1;
                            if lines_left == 0 {
                                end = pos + 1;
                                break;
                            }
                        }
                    }
                    ctx.telemetry.add_read(end as u64);
                    ignore_broken_pipe(out.write_all(&slice[..end]))?;
                    ctx.telemetry.add_written(end as u64);
                    bytes_left = bytes_left.saturating_sub(end);
                    if end < slice.len() {
                        break;
                    }
                }
            }
            InputSource::Stdin => {
                let mut reader = FastReader::open(src)?;
                reader.with_bytes(|bytes| -> io::Result<()> {
                    let end = if let Some(b_lim) = byte_limit {
                        bytes.len().min(b_lim)
                    } else if parsed.lines == 0 {
                        0
                    } else {
                        memchr_iter(b'\n', bytes)
                            .nth(parsed.lines - 1)
                            .map(|pos| pos + 1)
                            .unwrap_or(bytes.len())
                    };
                    ctx.telemetry.add_read(end as u64);
                    ignore_broken_pipe(out.write_all(&bytes[..end]))?;
                    ctx.telemetry.add_written(end as u64);
                    Ok(())
                })??;
            }
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// 4. tail — SIMD reverse newline scan (`memrchr`) + `-f` follow + NDJSON support
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "tail", about = "Output the last part of files (or NDJSON object streams)")]
struct TailArgs {
    /// Output the last NUM lines/objects instead of the last 10
    #[arg(short = 'n', long = "lines", default_value_t = 10)]
    lines: usize,

    /// Output the last NUM bytes
    #[arg(short = 'c', long = "bytes")]
    bytes: Option<String>,

    /// Output appended data as the file grows
    #[arg(short = 'f', long = "follow")]
    follow: bool,

    #[arg(short = 'q', long = "quiet")]
    quiet: bool,

    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    #[arg(short = 'J', long = "json")]
    json: bool,

    files: Vec<String>,
}

pub fn run_tail(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = TailArgs::try_parse_from(normalize_legacy_num_arg(args))?;
    let sources = InputSource::from_args(&parsed.files);

    if sources.len() == 1
        && matches!(sources[0], InputSource::Stdin)
        && parsed.bytes.is_none()
        && !parsed.follow
        && !ctx.stdin_is_tty
    {
        let all_lines = read_all_stdin_lines()?;
        if let Some(objs) = read_ndjson_stdin(&all_lines) {
            ctx.telemetry.set_strategy("NDJSON object stream + auto-table");
            let start = objs.len().saturating_sub(parsed.lines);
            let slice = &objs[start..];
            ctx.telemetry.add_items(slice.len() as u64);
            emit_objects_or_table(slice, ctx, parsed.json, !parsed.json)?;
            return Ok(0);
        }
        let stdout = io::stdout();
        let mut out = io::BufWriter::new(stdout.lock());
        let start = all_lines.len().saturating_sub(parsed.lines);
        for line in &all_lines[start..] {
            ignore_broken_pipe(writeln!(out, "{}", line))?;
        }
        ignore_broken_pipe(out.flush())?;
        return Ok(0);
    }

    let byte_limit = match parsed.bytes {
        Some(ref b) => Some(parse_size_str(b).map_err(|e| anyhow!(e))? as usize),
        None => None,
    };

    ctx.telemetry.set_strategy("reverse SeekFrom::End + memrchr SIMD");
    let show_headers = parsed.verbose || (!parsed.quiet && sources.len() > 1);
    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());

    for (idx, src) in sources.iter().enumerate() {
        if show_headers {
            if idx > 0 {
                writeln!(out)?;
            }
            writeln!(out, "==> {} <==", src.display_name())?;
        }
        match src {
            InputSource::File(p) => {
                use std::io::{Read, Seek, SeekFrom};
                let mut f = std::fs::File::open(p)?;
                let file_len = f.metadata()?.len() as usize;
                if file_len == 0 || parsed.lines == 0 {
                    continue;
                }
                if let Some(b_lim) = byte_limit {
                    let start = file_len.saturating_sub(b_lim);
                    f.seek(SeekFrom::Start(start as u64))?;
                    let mut buf = vec![0u8; file_len - start];
                    f.read_exact(&mut buf)?;
                    ctx.telemetry.add_read(buf.len() as u64);
                    ignore_broken_pipe(out.write_all(&buf))?;
                    ctx.telemetry.add_written(buf.len() as u64);
                } else {
                    // Read backwards in 64KiB blocks from EOF
                    let mut pos = file_len;
                    let mut newlines_found = 0usize;
                    let mut start_offset = 0usize;
                    let mut first_chunk = true;
                    let mut chunk = vec![0u8; DEFAULT_BUF_SIZE];

                    while pos > 0 {
                        let read_size = pos.min(DEFAULT_BUF_SIZE);
                        pos -= read_size;
                        f.seek(SeekFrom::Start(pos as u64))?;
                        f.read_exact(&mut chunk[..read_size])?;
                        let search_slice = if first_chunk && chunk[read_size - 1] == b'\n' {
                            &chunk[..read_size - 1]
                        } else {
                            &chunk[..read_size]
                        };
                        first_chunk = false;

                        let mut found_in_chunk = None;
                        for nl_idx in memrchr_iter(b'\n', search_slice) {
                            newlines_found += 1;
                            if newlines_found == parsed.lines {
                                found_in_chunk = Some(pos + nl_idx + 1);
                                break;
                            }
                        }
                        if let Some(off) = found_in_chunk {
                            start_offset = off;
                            break;
                        }
                    }
                    f.seek(SeekFrom::Start(start_offset as u64))?;
                    let tail_len = file_len - start_offset;
                    let mut tail_buf = vec![0u8; tail_len];
                    f.read_exact(&mut tail_buf)?;
                    ctx.telemetry.add_read(tail_len as u64);
                    ignore_broken_pipe(out.write_all(&tail_buf))?;
                    ctx.telemetry.add_written(tail_len as u64);
                }
            }
            InputSource::Stdin => {
                let mut reader = FastReader::open(src)?;
                reader.with_bytes(|bytes| -> io::Result<()> {
                    let start = if let Some(b_lim) = byte_limit {
                        bytes.len().saturating_sub(b_lim)
                    } else if parsed.lines == 0 || bytes.is_empty() {
                        bytes.len()
                    } else {
                        let search_slice = if bytes.last() == Some(&b'\n') {
                            &bytes[..bytes.len() - 1]
                        } else {
                            bytes
                        };
                        memrchr_iter(b'\n', search_slice)
                            .nth(parsed.lines - 1)
                            .map(|pos| pos + 1)
                            .unwrap_or(0)
                    };
                    let tail_slice = &bytes[start..];
                    ctx.telemetry.add_read(tail_slice.len() as u64);
                    ignore_broken_pipe(out.write_all(tail_slice))?;
                    ctx.telemetry.add_written(tail_slice.len() as u64);
                    Ok(())
                })??;
            }
        }
    }
    ignore_broken_pipe(out.flush())?;

    if parsed.follow {
        if let Some(InputSource::File(path)) = sources.first() {
            use std::io::{Read, Seek, SeekFrom};
            let mut file = std::fs::File::open(path)?;
            let mut pos = file.seek(SeekFrom::End(0))?;
            let mut buf = [0u8; 8192];
            loop {
                thread::sleep(Duration::from_millis(100));
                let len = file.metadata()?.len();
                if len > pos {
                    file.seek(SeekFrom::Start(pos))?;
                    let n = file.read(&mut buf)?;
                    if n > 0 {
                        out.write_all(&buf[..n])?;
                        out.flush()?;
                        pos += n as u64;
                    }
                }
            }
        }
    }

    Ok(0)
}

/// Converts `-5` shorthand to `-n 5` for `head` and `tail`.
fn normalize_legacy_num_arg(args: Vec<String>) -> Vec<String> {
    args.into_iter()
        .map(|a| {
            if a.starts_with('-') && a.len() > 1 && a[1..].chars().all(|c| c.is_ascii_digit()) {
                format!("-n{}", &a[1..])
            } else {
                a
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 5. wc — SIMD `bytecount` + `--json` + visual table mode
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "wc", about = "Print newline, word, and byte counts for each FILE")]
struct WcArgs {
    /// Print the byte counts
    #[arg(short = 'c', long = "bytes")]
    bytes: bool,

    /// Print the character counts
    #[arg(short = 'm', long = "chars")]
    chars: bool,

    /// Print the newline counts
    #[arg(short = 'l', long = "lines")]
    lines: bool,

    /// Print the word counts
    #[arg(short = 'w', long = "words")]
    words: bool,

    /// Print the maximum display width
    #[arg(short = 'L', long = "max-line-length")]
    max_line_length: bool,

    /// Emit structured JSON / NDJSON
    #[arg(short = 'J', long = "json")]
    json: bool,

    /// Render a rich visual breakdown table with sparklines
    #[arg(short = 't', long = "table")]
    table: bool,

    files: Vec<String>,
}

#[derive(Serialize, Clone, Default)]
struct WcRecord {
    file: String,
    lines: u64,
    words: u64,
    chars: u64,
    bytes: u64,
    max_line_length: u64,
}

pub fn run_wc(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = WcArgs::try_parse_from(args)?;
    let sources = InputSource::from_args(&parsed.files);

    let default_cols =
        !parsed.bytes && !parsed.chars && !parsed.lines && !parsed.words && !parsed.max_line_length;
    let need_words = default_cols || parsed.words || parsed.json || parsed.table;
    let need_chars = parsed.chars || parsed.json || parsed.table;
    let need_max_len = parsed.max_line_length || parsed.json || parsed.table;

    ctx.telemetry.set_strategy(if !need_words && !need_chars && !need_max_len {
        "SIMD bytecount::count (newlines + len only)"
    } else {
        "SIMD bytecount + single-pass word scanner"
    });

    let can_parallel = sources.len() > 1 && sources.iter().all(|s| matches!(s, InputSource::File(_)));
    let records: Vec<WcRecord> = if can_parallel {
        sources
            .par_iter()
            .filter_map(|s| count_source(s, need_words, need_chars, need_max_len, ctx).ok())
            .collect()
    } else {
        let mut out = Vec::with_capacity(sources.len());
        for s in &sources {
            out.push(count_source(s, need_words, need_chars, need_max_len, ctx)?);
        }
        out
    };

    if parsed.json {
        emit_objects_or_table(&records, ctx, true, false)?;
        return Ok(0);
    }

    if parsed.table {
        let max_bytes = records.iter().map(|r| r.bytes).max().unwrap_or(1).max(1);
        let st = ctx.style();
        let mut tbl = Table::new(vec![
            ("FILE", ColumnAlign::Left),
            ("LINES", ColumnAlign::Right),
            ("WORDS", ColumnAlign::Right),
            ("CHARS", ColumnAlign::Right),
            ("SIZE", ColumnAlign::Right),
            ("PROPORTION", ColumnAlign::Left),
        ]);
        for r in &records {
            let ratio = r.bytes as f64 / max_bytes as f64;
            tbl.add_row(vec![
                st.bold(&r.file),
                r.lines.to_string(),
                r.words.to_string(),
                r.chars.to_string(),
                st.green(&dogfish_core::format_size(r.bytes, true)),
                sparkline_bar(ratio, 10, st),
            ]);
        }
        let stdout = io::stdout();
        let mut out = stdout.lock();
        tbl.render(&mut out, st)?;
        return Ok(0);
    }

    let mut total = WcRecord {
        file: "total".to_string(),
        ..Default::default()
    };

    for r in &records {
        total.lines += r.lines;
        total.words += r.words;
        total.chars += r.chars;
        total.bytes += r.bytes;
        total.max_line_length = total.max_line_length.max(r.max_line_length);
        print_wc_row(r, &parsed, default_cols, sources.len() == 1 && parsed.files.is_empty());
    }

    if records.len() > 1 {
        print_wc_row(&total, &parsed, default_cols, false);
    }
    Ok(0)
}

fn count_source(
    src: &InputSource,
    need_words: bool,
    need_chars: bool,
    need_max_len: bool,
    ctx: &DogfishContext,
) -> Result<WcRecord> {
    let mut reader = FastReader::open(src)?;
    let rec = reader.with_bytes(|bytes| {
        ctx.telemetry.add_read(bytes.len() as u64);
        ctx.telemetry.add_items(1);
        let lines = bytecount::count(bytes, b'\n') as u64;
        let byte_count = bytes.len() as u64;
        let chars = if need_chars {
            bytecount::num_chars(bytes) as u64
        } else {
            0
        };

        let mut words = 0_u64;
        let mut max_line = 0_u64;
        if need_words || need_max_len {
            let mut in_word = false;
            let mut cur_line_len = 0_u64;
            for &b in bytes {
                if b == b'\n' {
                    if cur_line_len > max_line {
                        max_line = cur_line_len;
                    }
                    cur_line_len = 0;
                    in_word = false;
                } else {
                    cur_line_len += 1;
                    let is_ws = b.is_ascii_whitespace();
                    if !is_ws && !in_word {
                        words += 1;
                        in_word = true;
                    } else if is_ws {
                        in_word = false;
                    }
                }
            }
            if cur_line_len > max_line {
                max_line = cur_line_len;
            }
        }

        WcRecord {
            file: src.display_name().to_string(),
            lines,
            words,
            chars,
            bytes: byte_count,
            max_line_length: max_line,
        }
    })?;
    Ok(rec)
}

fn print_wc_row(r: &WcRecord, args: &WcArgs, default_cols: bool, omit_filename: bool) {
    let mut cols = Vec::new();
    if default_cols || args.lines {
        cols.push(format!("{:>8}", r.lines));
    }
    if default_cols || args.words {
        cols.push(format!("{:>8}", r.words));
    }
    if args.chars {
        cols.push(format!("{:>8}", r.chars));
    }
    if default_cols || args.bytes {
        cols.push(format!("{:>8}", r.bytes));
    }
    if args.max_line_length {
        cols.push(format!("{:>8}", r.max_line_length));
    }
    if omit_filename {
        println!("{}", cols.join(""));
    } else {
        println!("{} {}", cols.join(""), r.file);
    }
}

// ---------------------------------------------------------------------------
// 6. sort — Parallel TimSort + NDJSON `--by <field>` + numeric/human/reverse/unique
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "sort", about = "Sort lines of text files or NDJSON object streams")]
struct SortArgs {
    /// Reverse the result of comparisons
    #[arg(short = 'r', long = "reverse", visible_alias = "desc")]
    reverse: bool,

    /// Compare according to string numerical value
    #[arg(short = 'n', long = "numeric-sort")]
    numeric: bool,

    /// Compare human readable numbers (e.g., 2K, 1G)
    #[arg(short = 'h', long = "human-numeric-sort")]
    human_numeric: bool,

    /// Ignore leading blanks
    #[arg(short = 'b', long = "ignore-leading-blanks")]
    ignore_blanks: bool,

    /// Fold lower case to upper case characters
    #[arg(short = 'f', long = "ignore-case")]
    ignore_case: bool,

    /// Output only the first of an equal run
    #[arg(short = 'u', long = "unique")]
    unique: bool,

    /// Sort by column number (1-indexed) or field key
    #[arg(short = 'k', long = "key")]
    key: Option<usize>,

    /// Use SEP instead of non-blank to blank transition
    #[arg(short = 't', long = "field-separator")]
    separator: Option<char>,

    /// Modern Dogfish extension: Sort NDJSON objects by named field (e.g., `--by size`)
    #[arg(long = "by")]
    by_field: Option<String>,

    /// Filter NDJSON objects before sorting (e.g., `--where "size>10K"`)
    #[arg(short = 'w', long = "where")]
    where_expr: Option<String>,

    /// Emit JSON / NDJSON output
    #[arg(short = 'J', long = "json")]
    json: bool,

    files: Vec<String>,
}

pub fn run_sort(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = SortArgs::try_parse_from(args)?;
    let sources = InputSource::from_args(&parsed.files);

    let mut all_lines = Vec::new();
    for src in &sources {
        let reader = FastReader::open(src)?.into_bufread();
        for line in reader.lines() {
            let l = line?;
            ctx.telemetry.add_read(l.len() as u64 + 1);
            all_lines.push(l);
        }
    }

    // Check if input is an NDJSON object stream!
    if let Some(mut objs) = read_ndjson_stdin(&all_lines) {
        ctx.telemetry.set_strategy("NDJSON field-aware parallel sort");
        if let Some(ref expr) = parsed.where_expr {
            objs.retain(|o| eval_json_where(o, expr));
        }
        let field = parsed
            .by_field
            .clone()
            .unwrap_or_else(|| "name".to_string());
        objs.par_sort_by(|a, b| {
            let ord = compare_json_field(a, b, &field);
            if parsed.reverse { ord.reverse() } else { ord }
        });
        if parsed.unique {
            objs.dedup_by(|a, b| compare_json_field(a, b, &field) == std::cmp::Ordering::Equal);
        }
        ctx.telemetry.add_items(objs.len() as u64);
        emit_objects_or_table(&objs, ctx, parsed.json, !parsed.json)?;
        return Ok(0);
    }

    ctx.telemetry.set_strategy("rayon par_sort_by");
    all_lines.par_sort_by(|a, b| {
        let ka = extract_sort_key(a, parsed.key, parsed.separator, parsed.ignore_blanks);
        let kb = extract_sort_key(b, parsed.key, parsed.separator, parsed.ignore_blanks);

        let ord = if parsed.human_numeric {
            let na = parse_size_str(ka).unwrap_or(0);
            let nb = parse_size_str(kb).unwrap_or(0);
            na.cmp(&nb)
        } else if parsed.numeric {
            let na = ka.trim().parse::<f64>().unwrap_or(0.0);
            let nb = kb.trim().parse::<f64>().unwrap_or(0.0);
            na.partial_cmp(&nb).unwrap_or(std::cmp::Ordering::Equal)
        } else if parsed.ignore_case {
            ka.to_ascii_lowercase().cmp(&kb.to_ascii_lowercase())
        } else {
            ka.cmp(kb)
        };

        if parsed.reverse { ord.reverse() } else { ord }
    });

    if parsed.unique {
        all_lines.dedup();
    }

    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());
    for line in &all_lines {
        ignore_broken_pipe(writeln!(out, "{}", line))?;
        ctx.telemetry.add_written(line.len() as u64 + 1);
    }
    ctx.telemetry.add_items(all_lines.len() as u64);
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}

fn extract_sort_key(line: &str, key_idx: Option<usize>, sep: Option<char>, trim: bool) -> &str {
    let base = match key_idx {
        Some(k) if k >= 1 => {
            if let Some(delim) = sep {
                line.split(delim).nth(k - 1).unwrap_or("")
            } else {
                line.split_whitespace().nth(k - 1).unwrap_or("")
            }
        }
        _ => line,
    };
    if trim { base.trim_start() } else { base }
}

// ---------------------------------------------------------------------------
// 7. uniq — Deduplicate lines or NDJSON objects (`--by <field>`)
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "uniq", about = "Report or omit repeated lines or NDJSON objects")]
struct UniqArgs {
    /// Prefix lines by the number of occurrences
    #[arg(short = 'c', long = "count")]
    count: bool,

    /// Only print duplicate lines, one for each group
    #[arg(short = 'd', long = "repeated")]
    repeated: bool,

    /// Only print unique lines
    #[arg(short = 'u', long = "unique")]
    unique: bool,

    /// Ignore differences in case when comparing
    #[arg(short = 'i', long = "ignore-case")]
    ignore_case: bool,

    /// Deduplicate NDJSON objects by named field
    #[arg(long = "by")]
    by_field: Option<String>,

    #[arg(short = 'J', long = "json")]
    json: bool,

    input: Option<String>,
}

pub fn run_uniq(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = UniqArgs::try_parse_from(args)?;
    let src = match parsed.input {
        Some(ref f) if f != "-" => InputSource::File(f.clone()),
        _ => InputSource::Stdin,
    };

    let reader = FastReader::open(&src)?.into_bufread();
    let lines: Vec<String> = reader.lines().collect::<io::Result<_>>()?;

    if let Some(objs) = read_ndjson_stdin(&lines) {
        ctx.telemetry.set_strategy("NDJSON object deduplication");
        let field = parsed.by_field.as_deref().unwrap_or("name");
        let mut groups: Vec<(Value, usize)> = Vec::new();
        for obj in objs {
            if let Some((last, cnt)) = groups.last_mut() {
                if compare_json_field(last, &obj, field) == std::cmp::Ordering::Equal {
                    *cnt += 1;
                    continue;
                }
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
                if parsed.count {
                    if let Some(map) = val.as_object_mut() {
                        map.insert("count".to_string(), serde_json::json!(c));
                    }
                }
                val
            })
            .collect();
        emit_objects_or_table(&filtered, ctx, parsed.json, !parsed.json)?;
        return Ok(0);
    }

    let mut groups: Vec<(String, usize)> = Vec::new();
    for line in lines {
        let matches_prev = groups.last().map_or(false, |(prev, _)| {
            if parsed.ignore_case {
                prev.eq_ignore_ascii_case(&line)
            } else {
                prev == &line
            }
        });
        if matches_prev {
            groups.last_mut().unwrap().1 += 1;
        } else {
            groups.push((line, 1));
        }
    }

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    for (line, count) in groups {
        if parsed.repeated && count <= 1 {
            continue;
        }
        if parsed.unique && count != 1 {
            continue;
        }
        if parsed.count {
            ignore_broken_pipe(writeln!(out, "{:7} {}", count, line))?;
        } else {
            ignore_broken_pipe(writeln!(out, "{}", line))?;
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// 8. cut — Supports byte/char/field slicing AND NDJSON `--field name,size` projection!
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "cut", about = "Remove sections from each line of files or project fields from NDJSON")]
struct CutArgs {
    /// Select only these bytes (e.g. 1-5, 2,4)
    #[arg(short = 'b', long = "bytes")]
    bytes: Option<String>,

    /// Select only these characters
    #[arg(short = 'c', long = "characters")]
    chars: Option<String>,

    /// Select only these fields (1-indexed numbers or NDJSON field names like `name,size`)
    #[arg(short = 'f', long = "fields", visible_alias = "field")]
    fields: Option<String>,

    /// Use DELIM instead of TAB for field delimiter
    #[arg(short = 'd', long = "delimiter", default_value = "\t")]
    delimiter: String,

    /// Do not print lines not containing delimiters
    #[arg(short = 's', long = "only-delimited")]
    only_delimited: bool,

    /// Output delimiter (defaults to input delimiter)
    #[arg(long = "output-delimiter")]
    output_delimiter: Option<String>,

    /// Filter NDJSON objects before projecting (e.g., `--where "size>1K"`)
    #[arg(short = 'w', long = "where")]
    where_expr: Option<String>,

    #[arg(short = 'J', long = "json")]
    json: bool,

    files: Vec<String>,
}

pub fn run_cut(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = CutArgs::try_parse_from(args)?;
    let sources = InputSource::from_args(&parsed.files);

    // Check for NDJSON named-field projection (e.g., `dogfish ls -J | dogfish cut -f name,size`)
    if sources.len() == 1 && matches!(sources[0], InputSource::Stdin) && !ctx.stdin_is_tty {
        let all_lines = read_all_stdin_lines()?;
        if let Some(mut objs) = read_ndjson_stdin(&all_lines) {
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
        return cut_lines(&all_lines, &parsed);
    }

    for src in &sources {
        let reader = FastReader::open(src)?.into_bufread();
        let lines: Vec<String> = reader.lines().collect::<io::Result<_>>()?;
        cut_lines(&lines, &parsed)?;
    }
    Ok(0)
}

fn cut_lines(lines: &[String], parsed: &CutArgs) -> Result<i32> {
    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());
    let out_delim = parsed
        .output_delimiter
        .as_deref()
        .unwrap_or(&parsed.delimiter);

    for line in lines {
        if let Some(ref b_spec) = parsed.bytes {
            let indices = parse_range_spec(b_spec, line.len());
            let selected: Vec<u8> = indices
                .into_iter()
                .filter_map(|i| line.as_bytes().get(i - 1).copied())
                .collect();
            ignore_broken_pipe(out.write_all(&selected))?;
            ignore_broken_pipe(out.write_all(b"\n"))?;
        } else if let Some(ref c_spec) = parsed.chars {
            let chars: Vec<char> = line.chars().collect();
            let indices = parse_range_spec(c_spec, chars.len());
            let selected: String = indices
                .into_iter()
                .filter_map(|i| chars.get(i - 1).copied())
                .collect();
            ignore_broken_pipe(writeln!(out, "{}", selected))?;
        } else if let Some(ref f_spec) = parsed.fields {
            if !line.contains(&parsed.delimiter) {
                if !parsed.only_delimited {
                    ignore_broken_pipe(writeln!(out, "{}", line))?;
                }
                continue;
            }
            let parts: Vec<&str> = line.split(&parsed.delimiter).collect();
            let indices = parse_range_spec(f_spec, parts.len());
            let selected: Vec<&str> = indices
                .into_iter()
                .filter_map(|i| parts.get(i - 1).copied())
                .collect();
            ignore_broken_pipe(writeln!(out, "{}", selected.join(out_delim)))?;
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}

/// Parses 1-indexed GNU range specifications (`1,3`, `2-5`, `-3`, `4-`).
fn parse_range_spec(spec: &str, max_len: usize) -> Vec<usize> {
    let mut out = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if let Some((start_s, end_s)) = part.split_once('-') {
            let start = if start_s.is_empty() {
                1
            } else {
                start_s.parse::<usize>().unwrap_or(1)
            };
            let end = if end_s.is_empty() {
                max_len
            } else {
                end_s.parse::<usize>().unwrap_or(max_len).min(max_len)
            };
            for i in start..=end {
                if i >= 1 {
                    out.push(i);
                }
            }
        } else if let Ok(idx) = part.parse::<usize>() {
            if idx >= 1 {
                out.push(idx);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 9. tr — Translate, squeeze, or delete characters with character class support
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "tr", about = "Translate, squeeze, and/or delete characters from standard input")]
struct TrArgs {
    /// Delete characters in SET1, do not translate
    #[arg(short = 'd', long = "delete")]
    delete: bool,

    /// Replace each sequence of a repeated character with a single occurrence
    #[arg(short = 's', long = "squeeze-repeats")]
    squeeze: bool,

    /// Use the complement of SET1
    #[arg(short = 'c', visible_short_alias = 'C', long = "complement")]
    complement: bool,

    sets: Vec<String>,
}

pub fn run_tr(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = TrArgs::try_parse_from(args)?;
    let Some(set1_raw) = parsed.sets.first() else {
        return Err(anyhow!("tr: missing operand"));
    };
    let mut set1 = expand_tr_set(set1_raw);
    if parsed.complement {
        let mut comp = Vec::new();
        for b in 0..=127u8 {
            let ch = b as char;
            if !set1.contains(&ch) {
                comp.push(ch);
            }
        }
        set1 = comp;
    }
    let set2 = parsed.sets.get(1).map(|s| expand_tr_set(s)).unwrap_or_default();

    let mut input = String::new();
    use std::io::Read;
    io::stdin().lock().read_to_string(&mut input)?;
    ctx.telemetry.add_read(input.len() as u64);

    let mut result = String::with_capacity(input.len());
    let mut last_emitted: Option<char> = None;

    for ch in input.chars() {
        if parsed.delete {
            if set1.contains(&ch) {
                continue;
            }
            if parsed.squeeze && set2.contains(&ch) && last_emitted == Some(ch) {
                continue;
            }
            result.push(ch);
            last_emitted = Some(ch);
        } else if !set2.is_empty() {
            let mapped = if let Some(pos) = set1.iter().position(|&c| c == ch) {
                *set2.get(pos).unwrap_or_else(|| set2.last().unwrap())
            } else {
                ch
            };
            if parsed.squeeze && set2.contains(&mapped) && last_emitted == Some(mapped) {
                continue;
            }
            result.push(mapped);
            last_emitted = Some(mapped);
        } else if parsed.squeeze {
            if set1.contains(&ch) && last_emitted == Some(ch) {
                continue;
            }
            result.push(ch);
            last_emitted = Some(ch);
        } else {
            result.push(ch);
        }
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    ignore_broken_pipe(out.write_all(result.as_bytes()))?;
    ctx.telemetry.add_written(result.len() as u64);
    Ok(0)
}

fn expand_tr_set(raw: &str) -> Vec<char> {
    let s = raw
        .replace("[:lower:]", "a-z")
        .replace("[:upper:]", "A-Z")
        .replace("[:digit:]", "0-9")
        .replace("[:alpha:]", "a-zA-Z")
        .replace("[:alnum:]", "a-zA-Z0-9")
        .replace("[:space:]", " \t\n\r\x0b\x0c")
        .replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\r", "\r");

    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if i + 2 < chars.len() && chars[i + 1] == '-' {
            let start = chars[i] as u32;
            let end = chars[i + 2] as u32;
            if start <= end {
                for code in start..=end {
                    if let Some(c) = char::from_u32(code) {
                        out.push(c);
                    }
                }
                i += 3;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// 10. paste — Merge lines of files side-by-side or serially
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "paste", about = "Merge lines of files")]
struct PasteArgs {
    /// Use characters from LIST instead of TABs
    #[arg(short = 'd', long = "delimiters", default_value = "\t")]
    delimiters: String,

    /// Paste one file at a time instead of in parallel
    #[arg(short = 's', long = "serial")]
    serial: bool,

    files: Vec<String>,
}

pub fn run_paste(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = PasteArgs::try_parse_from(args)?;
    let sources = InputSource::from_args(&parsed.files);
    let delims: Vec<char> = parsed
        .delimiters
        .replace("\\n", "\n")
        .replace("\\t", "\t")
        .chars()
        .collect();
    let delims = if delims.is_empty() { vec!['\t'] } else { delims };

    let mut file_lines: Vec<VecDeque<String>> = Vec::with_capacity(sources.len());
    for src in &sources {
        let reader = FastReader::open(src)?.into_bufread();
        let q: VecDeque<String> = reader.lines().collect::<io::Result<_>>()?;
        file_lines.push(q);
    }

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    if parsed.serial {
        for q in &file_lines {
            let mut row = String::new();
            for (i, line) in q.iter().enumerate() {
                if i > 0 {
                    row.push(delims[(i - 1) % delims.len()]);
                }
                row.push_str(line);
            }
            ignore_broken_pipe(writeln!(out, "{}", row))?;
            ctx.telemetry.add_written(row.len() as u64 + 1);
        }
    } else {
        loop {
            if file_lines.iter().all(|q| q.is_empty()) {
                break;
            }
            let mut row = String::new();
            for (i, q) in file_lines.iter_mut().enumerate() {
                if i > 0 {
                    row.push(delims[(i - 1) % delims.len()]);
                }
                if let Some(line) = q.pop_front() {
                    row.push_str(&line);
                }
            }
            ignore_broken_pipe(writeln!(out, "{}", row))?;
            ctx.telemetry.add_written(row.len() as u64 + 1);
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// 11. tac — Concatenate and print files in reverse (SIMD memrchr_iter)
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "tac", about = "Concatenate and print files in reverse (last line first)")]
struct TacArgs {
    files: Vec<String>,
}

pub fn run_tac(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = TacArgs::try_parse_from(args)?;
    let sources = InputSource::from_args(&parsed.files);
    ctx.telemetry.set_strategy("mmap + memrchr reverse SIMD line scan");

    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());

    for src in &sources {
        let mut reader = FastReader::open(src)?;
        reader.with_bytes(|bytes| -> io::Result<()> {
            ctx.telemetry.add_read(bytes.len() as u64);
            let mut end = bytes.len();
            for pos in memrchr_iter(b'\n', bytes) {
                if pos + 1 < end {
                    ignore_broken_pipe(out.write_all(&bytes[pos + 1..end]))?;
                    if bytes[end - 1] != b'\n' {
                        ignore_broken_pipe(out.write_all(b"\n"))?;
                    }
                }
                end = pos + 1;
            }
            if end > 0 {
                ignore_broken_pipe(out.write_all(&bytes[..end]))?;
                if bytes[end - 1] != b'\n' {
                    ignore_broken_pipe(out.write_all(b"\n"))?;
                }
            }
            ctx.telemetry.add_written(bytes.len() as u64);
            Ok(())
        })??;
    }
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// 12. shuf — Fast Fisher-Yates shuffler for lines, numeric ranges (-i), or NDJSON!
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "shuf", about = "Generate random permutations of lines, ranges, or NDJSON objects")]
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
    ctx.telemetry.set_strategy("xorshift64* Fisher-Yates shuffle");

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
        (lo..=hi).map(|n| n.to_string()).collect()
    } else {
        let sources = InputSource::from_args(&parsed.args);
        let mut all = Vec::new();
        for s in &sources {
            let r = FastReader::open(s)?.into_bufread();
            for l in r.lines() {
                all.push(l?);
            }
        }
        all
    };

    if !parsed.echo && parsed.input_range.is_none() {
        if let Some(mut objs) = read_ndjson_stdin(&lines) {
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
    }

    let len = lines.len();
    for i in (1..len).rev() {
        let j = (next_u64() as usize) % (i + 1);
        lines.swap(i, j);
    }
    if let Some(n) = parsed.head_count {
        lines.truncate(n);
    }

    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());
    for line in &lines {
        ignore_broken_pipe(writeln!(out, "{}", line))?;
    }
    ctx.telemetry.add_items(lines.len() as u64);
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// 13. query / filter — Dedicated NDJSON SQL-like pipeline tool (`where`, `sort`, `select`, `limit`)
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "query", visible_alias = "filter", about = "Filter, sort, and project NDJSON object streams (`dogfish ls -J | dogfish query -w \"size>1K\" -s size -d -f name,size`)")]
struct QueryArgs {
    /// Filter expression (`size>10K`, `kind=file`, `name~rs`, `lines>=100`)
    #[arg(short = 'w', long = "where")]
    where_expr: Vec<String>,

    /// Sort by field name
    #[arg(short = 's', long = "sort", visible_alias = "by")]
    sort_by: Option<String>,

    /// Sort descending
    #[arg(short = 'd', long = "desc", visible_short_alias = 'r')]
    desc: bool,

    /// Comma-separated fields to select/project (`name,size,kind`)
    #[arg(short = 'f', long = "select", visible_alias = "fields")]
    select: Option<String>,

    /// Limit number of output rows
    #[arg(short = 'n', long = "limit")]
    limit: Option<usize>,

    /// Emit raw JSON / NDJSON instead of auto-table on TTY
    #[arg(short = 'J', long = "json")]
    json: bool,
}

pub fn run_query(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = QueryArgs::try_parse_from(args)?;
    ctx.telemetry.set_strategy("NDJSON query engine (where + sort + select)");

    let all_lines = read_all_stdin_lines()?;
    let Some(mut objs) = read_ndjson_stdin(&all_lines) else {
        return Err(anyhow!(
            "query: stdin is not an NDJSON object stream. Tip: pass `-J` to the upstream command (e.g. `dogfish ls -J | dogfish query ...`)"
        ));
    };

    for expr in &parsed.where_expr {
        objs.retain(|o| eval_json_where(o, expr));
    }

    if let Some(ref field) = parsed.sort_by {
        objs.par_sort_by(|a, b| {
            let ord = compare_json_field(a, b, field);
            if parsed.desc { ord.reverse() } else { ord }
        });
    }

    if let Some(lim) = parsed.limit {
        objs.truncate(lim);
    }

    if let Some(ref f_spec) = parsed.select {
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

    ctx.telemetry.add_items(objs.len() as u64);
    emit_objects_or_table(&objs, ctx, parsed.json, !parsed.json)?;
    Ok(0)
}
