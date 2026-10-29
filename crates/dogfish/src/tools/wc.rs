//! wc — SIMD `bytecount` + parallel UTF-8 aware scanner + `--json` + table mode

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;

use anyhow::Result;
use dogfish_core::{
    ColumnAlign, DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, Table,
    emit_objects_or_table, gnu_io_error, sparkline_bar, stdout,
};
use rayon::prelude::*;
use serde::Serialize;
use unicode_width::UnicodeWidthChar;

use crate::common::files0::{Files0From, extra_operand_error};
use crate::common::getopt::{
    Getopt, HasArg, OptSpec, Order, argmatch, getopt_error, lossy, lossy_all, print_help,
    print_version, unsupported,
};
use crate::common::input::{close_stdin, safe_read, stdin_file, stdin_is_closed};
use crate::common::locale::utf8_locale;
use crate::common::quote::{qa, qf};

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
      --files0-from=F    Read input from the files specified by NUL-terminated
                         names in file F; if F is - then read names from
                         standard input
      --total=WHEN       When to print a line with total counts;
                         WHEN can be: auto, always, only, never
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
    files0_from: Option<String>,
    total: TotalMode,
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
    Files0From,
    Total,
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
    OptSpec::long("files0-from", HasArg::Required, WcArgsOpt::Files0From),
    OptSpec::both(
        b'L',
        "max-line-length",
        HasArg::No,
        WcArgsOpt::MaxLineLength,
    ),
    OptSpec::long("total", HasArg::Required, WcArgsOpt::Total),
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
        let (o, v) = opt.map_err(|e| getopt_error("wc", &e))?;
        match o {
            WcArgsOpt::Bytes => parsed.bytes = true,
            WcArgsOpt::Chars => parsed.chars = true,
            WcArgsOpt::Lines => parsed.lines = true,
            WcArgsOpt::Words => parsed.words = true,
            WcArgsOpt::MaxLineLength => parsed.max_line_length = true,
            WcArgsOpt::Json => parsed.json = true,
            WcArgsOpt::Table => parsed.table = true,
            WcArgsOpt::Files0From => parsed.files0_from = v.as_deref().map(lossy),
            WcArgsOpt::Total => {
                parsed.total = argmatch(
                    "wc",
                    "--total",
                    v.unwrap_or_default(),
                    &[
                        ("auto", TotalMode::Auto),
                        ("always", TotalMode::Always),
                        ("only", TotalMode::Only),
                        ("never", TotalMode::Never),
                    ],
                )?;
            }
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
fn wc_scan(bytes: &[u8], utf8: bool) -> WcScan {
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
        if !utf8 {
            // C locale: a byte above 0x7f is a character that is neither
            // printable nor a separator.
            i += 1;
            s.chars += 1;
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

fn wc_count_bytes(bytes: &[u8], need_scan: bool, utf8: bool) -> WcScan {
    if !need_scan {
        // Fast path: only `-l` / `-c` requested -> SIMD `bytecount::count`,
        // on one thread (more threads cost more CPU than they save).
        let lines = bytecount::count(bytes, b'\n') as u64;
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
            .map(|c| wc_scan(c, utf8))
            .reduce(WcScan::default, |a, b| WcScan {
                lines: a.lines + b.lines,
                words: a.words + b.words,
                chars: a.chars + b.chars,
                max_line: a.max_line.max(b.max_line),
            });
    }
    wc_scan(bytes, utf8)
}

/// What to count and print (GNU's `print_*` flags).
#[derive(Clone, Copy)]
struct WcOpts {
    lines: bool,
    words: bool,
    chars: bool,
    bytes: bool,
    max_line_length: bool,
    /// Words, characters or widths are needed: run the full scanner.
    need_scan: bool,
    /// The locale's characters are UTF-8 sequences; else `-m` counts bytes.
    utf8: bool,
}

impl WcOpts {
    /// Only `-c` (or `-m` where characters are bytes): a regular file's
    /// size answers it without reading.
    fn bytes_only(&self) -> bool {
        (self.bytes || (self.chars && !self.utf8))
            && !(self.lines || self.words || (self.chars && self.utf8) || self.max_line_length)
    }

    fn columns(&self) -> usize {
        [
            self.lines,
            self.words,
            self.chars,
            self.bytes,
            self.max_line_length,
        ]
        .iter()
        .filter(|&&b| b)
        .count()
    }
}

/// `--total=WHEN`.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum TotalMode {
    #[default]
    Auto,
    Always,
    Only,
    Never,
}

/// One input's counts; `error` is a read error after which GNU still
/// prints what was counted.
struct Counted {
    rec: WcRecord,
    error: Option<io::Error>,
}

/// An input opened for counting.
enum WcInput {
    Stdin,
    File(File),
}

/// Opens `src`; `Err` is the open error (GNU then prints no counts).
fn open_input(src: &InputSource) -> io::Result<WcInput> {
    match src {
        InputSource::Stdin => Ok(WcInput::Stdin),
        InputSource::File(path) => File::open(path).map(WcInput::File),
    }
}

/// GNU's `-c` shortcut: a regular file (or one positioned inside) counts
/// its size from the current offset, and the offset moves to the end, as
/// if it had been read. Sizes that are a multiple of the page size may be
/// approximate (/proc, /sys), so all but the last block is skipped and the
/// rest really read.
fn count_bytes_only(file: &mut File) -> (u64, Option<io::Error>) {
    let mut bytes = 0u64;
    let mut skip_read = false;
    if let Ok(meta) = file.metadata()
        && meta.is_file()
    {
        // SAFETY: no preconditions: sysconf takes no pointers and cannot
        // fail in a way that matters (a -1 falls back to 4096).
        let page_size = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
            .unwrap_or(4096)
            .max(1);
        let end_pos = meta.len();
        let current_pos = file.stream_position().ok();
        if end_pos % page_size != 0 {
            bytes = end_pos.saturating_sub(current_pos.unwrap_or(0));
            if bytes > 0 && file.seek(SeekFrom::Current(bytes as i64)).is_ok() {
                skip_read = true;
            } else {
                bytes = 0;
            }
        } else {
            let hi_pos = end_pos - end_pos % (meta.blksize() + 1);
            if let Some(pos) = current_pos
                && pos < hi_pos
                && file.seek(SeekFrom::Current(hi_pos as i64)).is_ok()
            {
                bytes = hi_pos - pos;
            }
        }
    }
    if skip_read {
        return (bytes, None);
    }
    let mut buf = vec![0u8; DEFAULT_BUF_SIZE];
    loop {
        match safe_read(file, &mut buf) {
            Ok(0) => return (bytes, None),
            Ok(n) => bytes += n as u64,
            Err(e) => return (bytes, Some(e)),
        }
    }
}

/// Counts one opened input (GNU's `wc`).
fn count_input(input: WcInput, name: &str, opts: &WcOpts, ctx: &DogfishContext) -> Counted {
    let mut stdin = stdin_file();
    let mut file = match input {
        WcInput::Stdin => None,
        WcInput::File(f) => Some(f),
    };
    let handle: &mut File = match &mut file {
        Some(f) => f,
        None => &mut stdin,
    };
    let record = |bytes: u64, s: WcScan| WcRecord {
        file: name.to_string(),
        lines: s.lines,
        words: s.words,
        // GNU: where characters are bytes, -m reports the byte count.
        chars: if opts.utf8 { s.chars } else { bytes },
        bytes,
        max_line_length: s.max_line,
    };
    if opts.bytes_only() {
        let (bytes, error) = count_bytes_only(handle);
        ctx.telemetry.add_items(1);
        return Counted {
            rec: record(bytes, WcScan::default()),
            error,
        };
    }
    if !opts.need_scan {
        // Lines (and bytes): stream in large reads, as GNU's wc_lines does,
        // rather than mapping or buffering the whole input.
        let (mut lines, mut bytes) = (0u64, 0u64);
        let mut buf = vec![0u8; 64 * 1024];
        let error = loop {
            match safe_read(handle, &mut buf) {
                Ok(0) => break None,
                Ok(n) => {
                    lines += bytecount::count(&buf[..n], b'\n') as u64;
                    bytes += n as u64;
                }
                Err(e) => break Some(e),
            }
        };
        ctx.telemetry.add_read(bytes);
        ctx.telemetry.add_items(1);
        let scan = WcScan {
            lines,
            ..Default::default()
        };
        return Counted {
            rec: record(bytes, scan),
            error,
        };
    }
    let count = |data: &[u8]| {
        ctx.telemetry.add_read(data.len() as u64);
        ctx.telemetry.add_items(1);
        record(
            data.len() as u64,
            wc_count_bytes(data, opts.need_scan, opts.utf8),
        )
    };
    match file {
        // A file is mapped when possible; it was just opened, at offset 0.
        Some(f) => match FastReader::from_file(f) {
            Ok(mut reader) => match reader.with_bytes(count) {
                Ok(rec) => Counted { rec, error: None },
                Err(e) => Counted {
                    rec: count(&[]),
                    error: Some(e),
                },
            },
            Err(e) => Counted {
                rec: count(&[]),
                error: Some(e),
            },
        },
        // Standard input is read from its current offset.
        None => {
            let mut data = Vec::new();
            let error = stdin.read_to_end(&mut data).err();
            Counted {
                rec: count(&data),
                error,
            }
        }
    }
}

/// Counts `src` (GNU's `wc_file`); `Err` is the open error.
fn count_source(src: &InputSource, opts: &WcOpts, ctx: &DogfishContext) -> io::Result<Counted> {
    let name = match src {
        InputSource::Stdin => "standard input",
        InputSource::File(p) => p.as_str(),
    };
    Ok(count_input(open_input(src)?, name, opts, ctx))
}

/// (is_regular_file, size) of standard input.
fn stdin_stat() -> Option<(bool, u64)> {
    // GNU's fstat of a closed fd 0 fails; ours sees the /dev/null holding it.
    if stdin_is_closed() {
        return None;
    }
    let meta = stdin_file().metadata().ok()?;
    Some((meta.is_file(), meta.len()))
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

/// Writes one line of counts (GNU's `write_counts`); a name containing a
/// newline is quoted.
fn write_counts(
    out: &mut impl Write,
    opts: &WcOpts,
    width: usize,
    r: &WcRecord,
    name: Option<&str>,
) -> io::Result<()> {
    let mut row = String::new();
    let cols = [
        (opts.lines, r.lines),
        (opts.words, r.words),
        (opts.chars, r.chars),
        (opts.bytes, r.bytes),
        (opts.max_line_length, r.max_line_length),
    ];
    for (shown, val) in cols {
        if shown {
            if !row.is_empty() {
                row.push(' ');
            }
            row.push_str(&format!("{val:>width$}"));
        }
    }
    if let Some(n) = name {
        row.push(' ');
        if n.contains('\n') {
            row.push_str(&qf(n));
        } else {
            row.push_str(n);
        }
    }
    row.push('\n');
    out.write_all(row.as_bytes())
}

/// Running totals with GNU's overflow detection.
#[derive(Default)]
struct Totals {
    rec: WcRecord,
    overflow: [bool; 4],
}

impl Totals {
    fn add(&mut self, r: &WcRecord) {
        let t = &mut self.rec;
        for (i, (sum, v)) in [
            (&mut t.lines, r.lines),
            (&mut t.words, r.words),
            (&mut t.chars, r.chars),
            (&mut t.bytes, r.bytes),
        ]
        .into_iter()
        .enumerate()
        {
            match sum.checked_add(v) {
                Some(s) => *sum = s,
                None => {
                    *sum = u64::MAX;
                    self.overflow[i] = true;
                }
            }
        }
        t.max_line_length = t.max_line_length.max(r.max_line_length);
    }
}

/// A name from `--files0-from`: `-` is standard input, as an operand is.
fn name_source(name: &str) -> InputSource {
    if name == "-" {
        InputSource::Stdin
    } else {
        InputSource::File(name.to_string())
    }
}

/// The inputs: operands, or names read from `--files0-from`.
enum Names {
    Args(Vec<InputSource>),
    Files0(Files0From, Option<Vec<String>>),
}

pub fn run_wc(args: Vec<std::ffi::OsString>, ctx: &DogfishContext) -> Result<i32> {
    // GNU line buffers standard output, before parsing options.
    stdout::gnu_flushes_early();
    let parsed = match parse_wc_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let total_mode = parsed.total;

    let utf8 = utf8_locale();
    let default_cols =
        !parsed.bytes && !parsed.chars && !parsed.lines && !parsed.words && !parsed.max_line_length;
    let opts = WcOpts {
        lines: default_cols || parsed.lines,
        words: default_cols || parsed.words,
        chars: parsed.chars,
        bytes: default_cols || parsed.bytes,
        max_line_length: parsed.max_line_length,
        need_scan: default_cols
            || parsed.words
            || (parsed.chars && utf8)
            || parsed.max_line_length
            || parsed.json
            || parsed.table,
        utf8,
    };

    let mut names = match &parsed.files0_from {
        None => Names::Args(InputSource::from_args(&parsed.files)),
        Some(from) => {
            if let Some(extra) = parsed.files.first() {
                return Ok(extra_operand_error("wc", extra, 1));
            }
            let mut list = match Files0From::open(from) {
                Ok(list) => list,
                Err(e) => {
                    eprintln!(
                        "wc: cannot open {} for reading: {}",
                        qa(from),
                        gnu_io_error(&e)
                    );
                    return Ok(1);
                }
            };
            // A small regular list is read up front, as GNU does: its
            // length then decides the column width.
            let all = if list.is_small_regular() || parsed.json || parsed.table {
                match list.read_all() {
                    Ok(all) => Some(all),
                    Err(_) => {
                        eprintln!("wc: cannot read file names from {}", qa(from));
                        return Ok(1);
                    }
                }
            } else {
                None
            };
            Names::Files0(list, all)
        }
    };

    ctx.telemetry.set_strategy(if !opts.need_scan {
        "SIMD bytecount::count (newlines + len only)"
    } else {
        "SIMD bytecount + parallel line-aligned UTF-8 scanner"
    });

    if parsed.json || parsed.table {
        let sources = match &names {
            Names::Args(sources) => sources.clone(),
            Names::Files0(_, all) => all.iter().flatten().map(|n| name_source(n)).collect(),
        };
        return wc_rich(&sources, &opts, parsed.json, ctx);
    }

    // Column width: 1 for a single count of a single input, or when the
    // names are not known in advance; else from the inputs' sizes.
    let width = match &names {
        _ if total_mode == TotalMode::Only => 1,
        Names::Args(sources) if sources.len() == 1 && opts.columns() == 1 => 1,
        Names::Args(sources) => wc_number_width(sources),
        Names::Files0(_, Some(all)) if all.len() == 1 && opts.columns() == 1 => 1,
        Names::Files0(_, Some(all)) => {
            wc_number_width(&all.iter().map(|n| name_source(n)).collect::<Vec<_>>())
        }
        Names::Files0(_, None) => 1,
    };

    // GNU never checks its writes: every file is counted, and a write
    // error is reported at exit. Line buffered, as in GNU.
    let mut out = stdout::stdout();
    let mut ok = true;
    let mut totals = Totals::default();
    let mut read_stdin = false;
    let show_names = !parsed.files.is_empty() || parsed.files0_from.is_some();
    let mut emit = |out: &mut stdout::Stdout,
                    src: &InputSource,
                    res: io::Result<Counted>|
     -> io::Result<bool> {
        match res {
            Ok(c) => {
                if let Some(e) = &c.error {
                    let _ = out.flush();
                    eprintln!("wc: {}: {}", qf(&c.rec.file), gnu_io_error(e));
                }
                if total_mode != TotalMode::Only {
                    let name = show_names.then_some(src.display_name());
                    let _ = write_counts(out, &opts, width, &c.rec, name);
                }
                totals.add(&c.rec);
                Ok(c.error.is_none())
            }
            Err(e) => {
                let _ = out.flush();
                eprintln!("wc: {}: {}", qf(src.display_name()), gnu_io_error(&e));
                Ok(false)
            }
        }
    };

    let n_args = match &mut names {
        Names::Args(sources) => {
            read_stdin = sources.iter().any(|s| matches!(s, InputSource::Stdin));
            let can_parallel =
                sources.len() > 1 && sources.iter().all(|s| matches!(s, InputSource::File(_)));
            let results: Vec<io::Result<Counted>> = if can_parallel {
                sources
                    .par_iter()
                    .map(|s| count_source(s, &opts, ctx))
                    .collect()
            } else {
                sources
                    .iter()
                    .map(|s| count_source(s, &opts, ctx))
                    .collect()
            };
            for (src, res) in sources.iter().zip(results) {
                ok &= emit(&mut out, src, res)?;
            }
            if parsed.files.is_empty() {
                0
            } else {
                sources.len() as u64
            }
        }
        Names::Files0(list, all) => {
            let mut index = 0u64;
            let mut queued = all.take().map(|v| v.into_iter());
            loop {
                let name = match &mut queued {
                    Some(it) => match it.next() {
                        Some(n) => n,
                        None => break,
                    },
                    None => match list.next_name() {
                        Ok(Some(n)) => n,
                        Ok(None) => break,
                        Err(e) => {
                            let _ = out.flush();
                            eprintln!("wc: {}: read error: {}", qf(&list.name), gnu_io_error(&e));
                            ok = false;
                            break;
                        }
                    },
                };
                index += 1;
                if let Some(msg) = list.check_name(&name, index, true) {
                    let _ = out.flush();
                    eprintln!("wc: {msg}");
                    ok = false;
                    continue;
                }
                let src = name_source(&name);
                read_stdin |= matches!(src, InputSource::Stdin);
                let res = count_source(&src, &opts, ctx);
                ok &= emit(&mut out, &src, res)?;
            }
            index
        }
    };

    if total_mode != TotalMode::Never && (total_mode != TotalMode::Auto || n_args > 1) {
        for (overflowed, what) in totals.overflow.iter().zip([
            "total lines",
            "total words",
            "total characters",
            "total bytes",
        ]) {
            if *overflowed {
                let _ = out.flush();
                eprintln!("wc: {what}: Value too large for defined data type");
                ok = false;
            }
        }
        let name = (total_mode != TotalMode::Only).then_some("total");
        let _ = write_counts(&mut out, &opts, width, &totals.rec, name);
    }
    let _ = out.flush();
    if read_stdin && let Err(e) = close_stdin() {
        eprintln!("wc: -: {}", gnu_io_error(&e));
        return Ok(1);
    }
    Ok(i32::from(!ok))
}

/// `--json` / `--table`: records for every input, then NDJSON or a table.
fn wc_rich(
    sources: &[InputSource],
    opts: &WcOpts,
    json: bool,
    ctx: &DogfishContext,
) -> Result<i32> {
    let mut exit_code = 0;
    let mut records = Vec::with_capacity(sources.len());
    for src in sources {
        match count_source(src, opts, ctx) {
            Ok(c) => {
                if let Some(e) = &c.error {
                    eprintln!("wc: {}: {}", qf(&c.rec.file), gnu_io_error(e));
                    exit_code = 1;
                }
                let mut rec = c.rec;
                rec.file = src.display_name().to_string();
                records.push(rec);
            }
            Err(e) => {
                eprintln!("wc: {}: {}", qf(src.display_name()), gnu_io_error(&e));
                exit_code = 1;
            }
        }
    }
    if json {
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
    Ok(exit_code)
}

/// wc's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("wc", WC_HELP, SPECS)
        .choices("total", ["auto", "always", "only", "never"])
}
