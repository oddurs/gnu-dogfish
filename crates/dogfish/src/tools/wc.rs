//! wc — SIMD `bytecount` + parallel UTF-8 aware scanner + `--json` + table mode

use std::io::{self, Write};

use anyhow::Result;
use dogfish_core::{
    ColumnAlign, DogfishContext, FastReader, InputSource, Table, emit_objects_or_table,
    gnu_io_error, io::ignore_broken_pipe, sparkline_bar, stdout,
};
use rayon::prelude::*;
use serde::Serialize;
use unicode_width::UnicodeWidthChar;

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, Order, getopt_error, lossy_all, print_help, print_version, unsupported,
};

const WC_HELP: &str = "\
Print newline, word, and byte counts for each FILE

Usage: wc [OPTIONS] [FILES]...

Arguments:
  [FILES]...  

Options:
  -c, --bytes            Print the byte counts
  -m, --chars            Print the character counts
  -l, --lines            Print the newline counts
  -w, --words            Print the word counts
  -L, --max-line-length  Print the maximum display width
  -J, --json             Emit structured JSON / NDJSON
  -t, --table            Render a rich visual breakdown table with sparklines
      --help             Print help
";

#[derive(Default)]
struct WcArgs {
    bytes: bool,
    chars: bool,
    lines: bool,
    words: bool,
    max_line_length: bool,
    json: bool,
    table: bool,
    files: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WcArgsOpt {
    Bytes,
    Chars,
    Lines,
    Words,
    MaxLineLength,
    Json,
    Table,
    /// A GNU option Dogfish does not implement yet.
    Unsupported,
    Help,
    Version,
}

/// GNU's `longopts` and `"clLmw"`, plus the Dogfish `-J, --json` and
/// `-t, --table`.
const SPECS: &[OptSpec<WcArgsOpt>] = &[
    OptSpec::both(b'c', "bytes", HasArg::No, WcArgsOpt::Bytes),
    OptSpec::both(b'm', "chars", HasArg::No, WcArgsOpt::Chars),
    OptSpec::both(b'l', "lines", HasArg::No, WcArgsOpt::Lines),
    OptSpec::both(b'w', "words", HasArg::No, WcArgsOpt::Words),
    OptSpec::long("debug", HasArg::No, WcArgsOpt::Unsupported),
    OptSpec::long("files0-from", HasArg::Required, WcArgsOpt::Unsupported),
    OptSpec::both(
        b'L',
        "max-line-length",
        HasArg::No,
        WcArgsOpt::MaxLineLength,
    ),
    OptSpec::long("total", HasArg::Required, WcArgsOpt::Unsupported),
    OptSpec::both(b'J', "json", HasArg::No, WcArgsOpt::Json),
    OptSpec::both(b't', "table", HasArg::No, WcArgsOpt::Table),
    OptSpec::long("help", HasArg::No, WcArgsOpt::Help),
    OptSpec::long("version", HasArg::No, WcArgsOpt::Version),
];

/// Parses the command line; `Err` is the exit status after `--help`,
/// `--version` or a usage diagnostic.
fn parse_wc_args(args: Vec<std::ffi::OsString>) -> std::result::Result<WcArgs, i32> {
    let mut parsed = WcArgs::default();
    let mut opts = Getopt::with_order(args.into_iter().skip(1).collect(), SPECS, Order::Permute);
    while let Some(opt) = opts.next() {
        let (o, _) = opt.map_err(|e| getopt_error("wc", &e))?;
        match o {
            WcArgsOpt::Bytes => parsed.bytes = true,
            WcArgsOpt::Chars => parsed.chars = true,
            WcArgsOpt::Lines => parsed.lines = true,
            WcArgsOpt::Words => parsed.words = true,
            WcArgsOpt::MaxLineLength => parsed.max_line_length = true,
            WcArgsOpt::Json => parsed.json = true,
            WcArgsOpt::Table => parsed.table = true,
            WcArgsOpt::Unsupported => return Err(unsupported("wc", &opts.last_spelling())),
            WcArgsOpt::Help => return Err(print_help(WC_HELP)),
            WcArgsOpt::Version => return Err(print_version("wc")),
        }
    }
    parsed.files = lossy_all(opts.operands());
    Ok(parsed)
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

#[derive(Clone, Copy, Default)]
struct WcScan {
    lines: u64,
    words: u64,
    chars: u64,
    max_line: u64,
}

/// Unicode whitespace per glibc `iswspace`, plus the no-break spaces GNU wc
/// also treats as word separators.
fn is_unicode_space(c: char) -> bool {
    matches!(
        c as u32,
        0x1680 | 0x2000..=0x200A | 0x2028 | 0x2029 | 0x205F | 0x3000 | 0x00A0 | 0x202F | 0x2060
    )
}

/// Single-pass GNU-compatible (UTF-8 locale) word/char/display-width scanner.
/// The slice must start at a line boundary.
fn wc_scan(bytes: &[u8]) -> WcScan {
    let mut s = WcScan::default();
    let mut in_word = false;
    let mut linepos: u64 = 0;
    let mut i = 0;
    let len = bytes.len();
    while i < len {
        let b = bytes[i];
        if b < 0x80 {
            i += 1;
            s.chars += 1;
            match b {
                b'\n' | b'\r' | 0x0c => {
                    if b == b'\n' {
                        s.lines += 1;
                    }
                    s.max_line = s.max_line.max(linepos);
                    linepos = 0;
                    in_word = false;
                }
                b'\t' => {
                    linepos += 8 - (linepos % 8);
                    in_word = false;
                }
                b' ' => {
                    linepos += 1;
                    in_word = false;
                }
                0x0b => in_word = false,
                0x21..=0x7e => {
                    linepos += 1;
                    if !in_word {
                        s.words += 1;
                        in_word = true;
                    }
                }
                _ => {} // other control characters: neither word nor separator
            }
            continue;
        }
        let seq_len = match b {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => 0,
        };
        let decoded = (seq_len > 0 && i + seq_len <= len)
            .then(|| std::str::from_utf8(&bytes[i..i + seq_len]).ok())
            .flatten()
            .and_then(|st| st.chars().next());
        let Some(c) = decoded else {
            // Invalid / truncated sequence: not a character, zero width.
            i += 1;
            continue;
        };
        i += seq_len;
        s.chars += 1;
        let cp = c as u32;
        if (0x80..0xA0).contains(&cp) {
            continue; // C1 controls
        }
        linepos += c.width().unwrap_or(0) as u64;
        if is_unicode_space(c) {
            in_word = false;
        } else if !in_word {
            s.words += 1;
            in_word = true;
        }
    }
    s.max_line = s.max_line.max(linepos);
    s
}

/// Splits `bytes` into ~`target`-sized pieces that each start at a line boundary.
fn line_aligned_chunks(bytes: &[u8], target: usize) -> Vec<&[u8]> {
    let mut chunks = Vec::with_capacity(bytes.len() / target + 1);
    let mut start = 0;
    while start < bytes.len() {
        let nominal = (start + target).min(bytes.len());
        let end = match memchr::memchr(b'\n', &bytes[nominal..]) {
            Some(off) => nominal + off + 1,
            None => bytes.len(),
        };
        chunks.push(&bytes[start..end]);
        start = end;
    }
    chunks
}

fn wc_count_bytes(bytes: &[u8], need_scan: bool) -> WcScan {
    if !need_scan {
        // Fast path: only `-l` / `-c` requested -> pure SIMD `bytecount::count`
        let lines = if bytes.len() >= 512 * 1024 {
            bytes
                .par_chunks(256 * 1024)
                .map(|c| bytecount::count(c, b'\n') as u64)
                .sum()
        } else {
            bytecount::count(bytes, b'\n') as u64
        };
        return WcScan {
            lines,
            ..Default::default()
        };
    }
    if bytes.len() >= 512 * 1024 {
        // Parallel scan over line-aligned chunks: words never span a newline,
        // so each chunk is independent.
        return line_aligned_chunks(bytes, 256 * 1024)
            .par_iter()
            .map(|c| wc_scan(c))
            .reduce(WcScan::default, |a, b| WcScan {
                lines: a.lines + b.lines,
                words: a.words + b.words,
                chars: a.chars + b.chars,
                max_line: a.max_line.max(b.max_line),
            });
    }
    wc_scan(bytes)
}

fn count_source(src: &InputSource, need_scan: bool, ctx: &DogfishContext) -> io::Result<WcRecord> {
    let mut reader = FastReader::open(src)?;
    reader.with_bytes(|bytes| {
        ctx.telemetry.add_read(bytes.len() as u64);
        ctx.telemetry.add_items(1);
        let s = wc_count_bytes(bytes, need_scan);
        WcRecord {
            file: src.display_name().to_string(),
            lines: s.lines,
            words: s.words,
            chars: s.chars,
            bytes: bytes.len() as u64,
            max_line_length: s.max_line,
        }
    })
}

/// (is_regular_file, size) of standard input.
fn stdin_stat() -> Option<(bool, u64)> {
    // SAFETY: libc::stat is plain data, for which all-zero bytes are valid.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstat on fd 0 with a properly sized, writable stat buffer.
    if unsafe { libc::fstat(0, &mut st) } != 0 {
        return None;
    }
    Some((
        (st.st_mode & libc::S_IFMT) == libc::S_IFREG,
        st.st_size as u64,
    ))
}

/// GNU `compute_number_width`: digits of the summed regular-file sizes,
/// at least 7 when any input is not a regular file.
fn wc_number_width(sources: &[InputSource]) -> usize {
    let mut min_width = 1;
    let mut total: u64 = 0;
    for src in sources {
        let stat = match src {
            InputSource::Stdin => stdin_stat(),
            InputSource::File(p) => std::fs::metadata(p).ok().map(|m| (m.is_file(), m.len())),
        };
        match stat {
            Some((true, size)) => total = total.saturating_add(size),
            Some((false, _)) => min_width = 7,
            None => {}
        }
    }
    let mut width = 1;
    while total >= 10 {
        total /= 10;
        width += 1;
    }
    width.max(min_width)
}

pub fn run_wc(args: Vec<std::ffi::OsString>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_wc_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let sources = InputSource::from_args(&parsed.files);

    let default_cols =
        !parsed.bytes && !parsed.chars && !parsed.lines && !parsed.words && !parsed.max_line_length;
    let show_lines = default_cols || parsed.lines;
    let show_words = default_cols || parsed.words;
    let show_bytes = default_cols || parsed.bytes;
    let need_scan =
        show_words || parsed.chars || parsed.max_line_length || parsed.json || parsed.table;

    ctx.telemetry.set_strategy(if !need_scan {
        "SIMD bytecount::count (newlines + len only)"
    } else {
        "SIMD bytecount + parallel line-aligned UTF-8 scanner"
    });

    let can_parallel =
        sources.len() > 1 && sources.iter().all(|s| matches!(s, InputSource::File(_)));
    let results: Vec<io::Result<WcRecord>> = if can_parallel {
        sources
            .par_iter()
            .map(|s| count_source(s, need_scan, ctx))
            .collect()
    } else {
        sources
            .iter()
            .map(|s| count_source(s, need_scan, ctx))
            .collect()
    };

    let mut exit_code = 0;
    let mut records = Vec::with_capacity(results.len());
    let report = |src: &InputSource, e: &io::Error| {
        eprintln!("wc: {}: {}", src.display_name(), gnu_io_error(e));
    };

    if parsed.json || parsed.table {
        for (src, res) in sources.iter().zip(results) {
            match res {
                Ok(r) => records.push(r),
                Err(e) => {
                    report(src, &e);
                    exit_code = 1;
                }
            }
        }
        if parsed.json {
            emit_objects_or_table(&records, ctx, true, false)?;
            return Ok(exit_code);
        }
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
        let mut out = stdout::stdout();
        tbl.render(&mut out, st)?;
        return Ok(exit_code);
    }

    let ncols = [
        show_lines,
        show_words,
        parsed.chars,
        show_bytes,
        parsed.max_line_length,
    ]
    .iter()
    .filter(|&&b| b)
    .count();
    let width = if ncols == 1 && sources.len() == 1 {
        1
    } else {
        wc_number_width(&sources)
    };

    let mut out = io::BufWriter::new(stdout::stdout());
    let print_row = |out: &mut io::BufWriter<stdout::Stdout>,
                     r: &WcRecord,
                     name: Option<&str>|
     -> io::Result<()> {
        let mut row = String::new();
        let cols = [
            (show_lines, r.lines),
            (show_words, r.words),
            (parsed.chars, r.chars),
            (show_bytes, r.bytes),
            (parsed.max_line_length, r.max_line_length),
        ];
        for (shown, val) in cols {
            if shown {
                if !row.is_empty() {
                    row.push(' ');
                }
                row.push_str(&format!("{:>width$}", val));
            }
        }
        if let Some(n) = name {
            row.push(' ');
            row.push_str(n);
        }
        row.push('\n');
        ignore_broken_pipe(out.write_all(row.as_bytes()))
    };

    let mut total = WcRecord::default();
    for (src, res) in sources.iter().zip(results) {
        match res {
            Ok(r) => {
                total.lines += r.lines;
                total.words += r.words;
                total.chars += r.chars;
                total.bytes += r.bytes;
                total.max_line_length = total.max_line_length.max(r.max_line_length);
                let name = (!parsed.files.is_empty()).then_some(r.file.as_str());
                print_row(&mut out, &r, name)?;
            }
            Err(e) => {
                out.flush()?;
                report(src, &e);
                exit_code = 1;
            }
        }
    }
    if sources.len() > 1 {
        print_row(&mut out, &total, Some("total"))?;
    }
    ignore_broken_pipe(out.flush())?;
    Ok(exit_code)
}
