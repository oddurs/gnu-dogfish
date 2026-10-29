//! b2sum, sha256sum, md5sum — one engine, as in GNU (`md5sum.c`).

use std::io::{self, BufRead, Read, Write};

use anyhow::Result;
use blake2::Blake2bVar;
use clap::{ArgAction, ArgMatches, Args, CommandFactory, FromArgMatches, Parser};
use digest::{Digest, Update, VariableOutput};
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, emit_objects_or_table, gnu_io_error,
    gnu_quote, gnu_quote_file, io::ignore_broken_pipe, stdout,
};
use md5::Md5;
use rayon::prelude::*;
use serde::Serialize;

use crate::common::arg_order::last_index;

/// SHA-256 backed by `ring`, adapted to the RustCrypto [`Digest`] interface the hashing
/// pipeline is generic over.
///
/// `ring` carries the OpenSSL/BoringSSL assembly with runtime CPU dispatch (SHA-NI,
/// AVX2+BMI2, SSSE3, ARMv8 crypto), the same code GNU `sha256sum` reaches through
/// libcrypto. RustCrypto's `sha2` only accelerates SHA-NI and ARMv8, so it ran ~1.7x
/// slower than GNU on CPUs without SHA extensions.
struct RingSha256(ring::digest::Context);

impl Default for RingSha256 {
    fn default() -> Self {
        Self(ring::digest::Context::new(&ring::digest::SHA256))
    }
}

impl digest::HashMarker for RingSha256 {}

impl digest::OutputSizeUser for RingSha256 {
    type OutputSize = digest::consts::U32;
}

impl digest::Update for RingSha256 {
    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
}

impl digest::FixedOutput for RingSha256 {
    fn finalize_into(self, out: &mut digest::Output<Self>) {
        out.copy_from_slice(self.0.finish().as_ref());
    }
}

/// The digest a `*sum` tool computes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Algo {
    Md5,
    Sha256,
    /// BLAKE2b with a digest length in bits (a multiple of 8, at most 512).
    Blake2b(usize),
}

impl Algo {
    /// The name in BSD-style (`--tag`) lines and in diagnostics.
    fn type_name(self) -> &'static str {
        match self {
            Self::Md5 => "MD5",
            Self::Sha256 => "SHA256",
            Self::Blake2b(_) => "BLAKE2b",
        }
    }

    fn tag(self) -> String {
        match self {
            Self::Blake2b(bits) if bits != 512 => format!("BLAKE2b-{bits}"),
            _ => self.type_name().to_string(),
        }
    }

    fn hex_len(self) -> usize {
        match self {
            Self::Md5 => 32,
            Self::Sha256 => 64,
            Self::Blake2b(bits) => bits / 4,
        }
    }

    fn hasher(self) -> Hasher {
        match self {
            Self::Md5 => Hasher::Md5(Md5::new()),
            Self::Sha256 => Hasher::Sha256(RingSha256::default()),
            Self::Blake2b(bits) => {
                Hasher::Blake2b(Blake2bVar::new(bits / 8).expect("valid BLAKE2b length"))
            }
        }
    }
}

enum Hasher {
    Md5(Md5),
    Sha256(RingSha256),
    Blake2b(Blake2bVar),
}

impl Hasher {
    fn update(&mut self, data: &[u8]) {
        match self {
            Self::Md5(h) => Digest::update(h, data),
            Self::Sha256(h) => Digest::update(h, data),
            Self::Blake2b(h) => Update::update(h, data),
        }
    }

    fn finalize_hex(self) -> String {
        let bytes = match self {
            Self::Md5(h) => h.finalize().to_vec(),
            Self::Sha256(h) => h.finalize().to_vec(),
            Self::Blake2b(h) => {
                let mut out = vec![0u8; h.output_size()];
                h.finalize_variable(&mut out)
                    .expect("buffer matches the output size");
                out
            }
        };
        data_encoding::HEXLOWER.encode(&bytes)
    }
}

#[derive(Args)]
struct HashArgs {
    /// Read in binary mode
    #[arg(short = 'b', long = "binary", action = ArgAction::Count)]
    binary: u8,

    /// Read checksums from the FILEs and check them
    #[arg(short = 'c', long = "check")]
    check: bool,

    /// Create a BSD-style checksum
    #[arg(long = "tag", action = ArgAction::Count)]
    tag: u8,

    /// Read in text mode (default)
    #[arg(short = 't', long = "text", action = ArgAction::Count)]
    text: u8,

    /// End each output line with NUL, not newline, and disable file name escaping
    #[arg(short = 'z', long = "zero")]
    zero: bool,

    /// Don't fail or report status for missing files (with --check)
    #[arg(long = "ignore-missing")]
    ignore_missing: bool,

    /// Don't print OK for each successfully verified file (with --check)
    #[arg(long = "quiet", action = ArgAction::Count)]
    quiet: u8,

    /// Don't output anything, status code shows success (with --check)
    #[arg(long = "status", action = ArgAction::Count)]
    status: u8,

    /// Exit non-zero for improperly formatted checksum lines (with --check)
    #[arg(long = "strict")]
    strict: bool,

    /// Warn about improperly formatted checksum lines (with --check)
    #[arg(short = 'w', long = "warn", action = ArgAction::Count)]
    warn: u8,

    /// Emit structured JSON / NDJSON
    #[arg(short = 'J', long = "json")]
    json: bool,

    /// Display this help and exit
    #[arg(long, action = ArgAction::Help)]
    help: Option<bool>,

    /// Files to hash (defaults to stdin `-`)
    files: Vec<String>,
}

#[derive(Parser)]
#[command(
    about = "Print or check checksums. With no FILE, or when FILE is -, read standard input.",
    disable_help_flag = true,
    infer_long_args = true
)]
struct PlainSumArgs {
    #[command(flatten)]
    common: HashArgs,
}

#[derive(Parser)]
#[command(
    about = "Print or check BLAKE2b (512-bit) checksums. With no FILE, or when FILE is -, \
             read standard input.",
    disable_help_flag = true,
    infer_long_args = true
)]
struct B2sumArgs {
    /// Digest length in bits; must not exceed the maximum for the blake2 algorithm
    /// and must be a multiple of 8
    #[arg(short = 'l', long = "length", value_name = "BITS")]
    length: Option<String>,

    #[command(flatten)]
    common: HashArgs,
}

/// The options whose meaning depends on which came last, resolved the way GNU
/// does it (`--tag` implies binary mode; --status/--quiet/--warn override each other).
struct HashMode {
    check: bool,
    tag: bool,
    /// `None` when neither -b nor -t (nor --tag) was given.
    binary: Option<bool>,
    status_only: bool,
    quiet: bool,
    warn: bool,
    strict: bool,
    ignore_missing: bool,
    zero: bool,
    json: bool,
}

impl HashMode {
    fn resolve(a: &HashArgs, m: &ArgMatches) -> Self {
        let latest = |ids: &[&'static str]| {
            ids.iter()
                .filter_map(|id| last_index(m, id).map(|i| (i, *id)))
                .max()
                .map(|(_, id)| id)
        };
        let binary = latest(&["binary", "tag", "text"]).map(|id| id != "text");
        let report = latest(&["status", "quiet", "warn"]);
        Self {
            check: a.check,
            tag: a.tag > 0,
            binary,
            status_only: report == Some("status"),
            quiet: report == Some("quiet"),
            warn: report == Some("warn"),
            strict: a.strict,
            ignore_missing: a.ignore_missing,
            zero: a.zero,
            json: a.json,
        }
    }

    /// GNU's consistency checks, in GNU's order.
    fn validate(&self) -> Option<&'static str> {
        if self.tag && self.binary == Some(false) {
            return Some("--tag does not support --text mode");
        }
        if self.zero && self.check {
            return Some("the --zero option is not supported when verifying checksums");
        }
        if self.tag && self.check {
            return Some("the --tag option is meaningless when verifying checksums");
        }
        if self.binary.is_some() && self.check {
            return Some(
                "the --binary and --text options are meaningless when verifying checksums",
            );
        }
        if self.check {
            return None;
        }
        [
            (self.ignore_missing, "ignore-missing"),
            (self.status_only, "status"),
            (self.warn, "warn"),
            (self.quiet, "quiet"),
            (self.strict, "strict"),
        ]
        .iter()
        .find(|(set, _)| *set)
        .map(|(_, name)| match *name {
            "ignore-missing" => {
                "the --ignore-missing option is meaningful only when verifying checksums"
            }
            "status" => "the --status option is meaningful only when verifying checksums",
            "warn" => "the --warn option is meaningful only when verifying checksums",
            "quiet" => "the --quiet option is meaningful only when verifying checksums",
            _ => "the --strict option is meaningful only when verifying checksums",
        })
    }
}

#[derive(Serialize, Clone)]
struct HashRecord {
    algorithm: String,
    file: String,
    digest: String,
    bytes: u64,
}

pub fn run_b2sum(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let matches = B2sumArgs::command().try_get_matches_from(args)?;
    let parsed = B2sumArgs::from_arg_matches(&matches)?;
    let mut bits = 512;
    if let Some(l) = &parsed.length {
        let detail = match l.trim_start().parse::<u64>() {
            Err(_) => Some(None),
            Ok(n) if n > 512 => Some(Some("maximum digest length for 'BLAKE2b' is 512 bits")),
            Ok(n) if !n.is_multiple_of(8) => Some(Some("length is not a multiple of 8")),
            Ok(0) => None,
            Ok(n) => {
                bits = n as usize;
                None
            }
        };
        if let Some(detail) = detail {
            eprintln!("b2sum: invalid length: {}", gnu_quote(l));
            if let Some(detail) = detail {
                eprintln!("b2sum: {detail}");
            }
            return Ok(1);
        }
    }
    run_sum(Algo::Blake2b(bits), &parsed.common, &matches, ctx)
}

pub fn run_sha256sum(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let matches = PlainSumArgs::command().try_get_matches_from(args)?;
    let parsed = PlainSumArgs::from_arg_matches(&matches)?;
    run_sum(Algo::Sha256, &parsed.common, &matches, ctx)
}

pub fn run_md5sum(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let matches = PlainSumArgs::command().try_get_matches_from(args)?;
    let parsed = PlainSumArgs::from_arg_matches(&matches)?;
    run_sum(Algo::Md5, &parsed.common, &matches, ctx)
}

fn run_sum(algo: Algo, args: &HashArgs, m: &ArgMatches, ctx: &DogfishContext) -> Result<i32> {
    let tool = ctx.tool_name.as_str();
    let mode = HashMode::resolve(args, m);
    if let Some(msg) = mode.validate() {
        eprintln!("{tool}: {msg}\nTry '{tool} --help' for more information.");
        return Ok(1);
    }
    let sources = InputSource::from_args(&args.files);

    if mode.check {
        return verify_checksums(algo, &sources, &mode, ctx);
    }

    ctx.telemetry.set_strategy(if sources.len() > 1 {
        format!("rayon parallel {} + mmap", algo.type_name())
    } else {
        format!("{} + mmap", algo.type_name())
    });

    // If multiple file sources and none is stdin, hash in parallel using Rayon!
    let can_parallel =
        sources.len() > 1 && sources.iter().all(|s| matches!(s, InputSource::File(_)));
    let hash_one = |src: &InputSource| compute_source_hash(algo, src, ctx);
    let results: Vec<io::Result<HashRecord>> = if can_parallel {
        sources.par_iter().map(hash_one).collect()
    } else {
        sources.iter().map(hash_one).collect()
    };

    let mut exit_code = 0;
    let mut records = Vec::with_capacity(results.len());
    let mut out = io::BufWriter::new(stdout::stdout());
    for (src, res) in sources.iter().zip(results) {
        match res {
            Ok(rec) => {
                if !mode.json {
                    ignore_broken_pipe(write_digest_line(&mut out, algo, &rec, &mode))?;
                }
                records.push(rec);
            }
            Err(e) => {
                ignore_broken_pipe(out.flush())?;
                eprintln!(
                    "{tool}: {}: {}",
                    gnu_quote_file(src.display_name()),
                    gnu_io_error(&e)
                );
                exit_code = 1;
            }
        }
    }
    ignore_broken_pipe(out.flush())?;
    if mode.json {
        drop(out);
        emit_objects_or_table(&records, ctx, true, false)?;
    }
    Ok(exit_code)
}

/// GNU escapes `\`, newline and carriage return in file names, and marks such
/// lines with a leading backslash.
fn escape_file_name(name: &str) -> Option<String> {
    if !name.contains(['\\', '\n', '\r']) {
        return None;
    }
    Some(
        name.replace('\\', "\\\\")
            .replace('\n', "\\n")
            .replace('\r', "\\r"),
    )
}

fn write_digest_line<W: Write>(
    out: &mut W,
    algo: Algo,
    rec: &HashRecord,
    mode: &HashMode,
) -> io::Result<()> {
    let escaped = if mode.zero {
        None
    } else {
        escape_file_name(&rec.file)
    };
    let name = escaped.as_deref().unwrap_or(&rec.file);
    let prefix = if escaped.is_some() { "\\" } else { "" };
    let term = if mode.zero { '\0' } else { '\n' };
    let line = if mode.tag {
        format!("{prefix}{} ({name}) = {}{term}", algo.tag(), rec.digest)
    } else {
        let marker = if mode.binary == Some(true) { '*' } else { ' ' };
        format!("{prefix}{} {marker}{name}{term}", rec.digest)
    };
    out.write_all(line.as_bytes())
}

fn compute_source_hash(
    algo: Algo,
    src: &InputSource,
    ctx: &DogfishContext,
) -> io::Result<HashRecord> {
    let mut hasher = algo.hasher();
    let mut bytes_len = 0u64;
    match FastReader::open(src)? {
        FastReader::Mapped(map) => {
            hasher.update(&map);
            bytes_len = map.len() as u64;
        }
        FastReader::Buffered(mut reader) => {
            let mut buf = vec![0u8; DEFAULT_BUF_SIZE];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                };
                hasher.update(&buf[..n]);
                bytes_len += n as u64;
            }
        }
    }
    ctx.telemetry.add_read(bytes_len);
    ctx.telemetry.add_items(1);
    Ok(HashRecord {
        algorithm: algo.tag(),
        file: src.display_name().to_string(),
        digest: hasher.finalize_hex(),
        bytes: bytes_len,
    })
}

/// Undoes GNU's file name escaping (`\\`, `\n`, `\r`); other escapes are invalid.
fn unescape_file_name(name: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(name.len());
    let mut it = name.iter();
    while let Some(&c) = it.next() {
        if c != b'\\' {
            out.push(c);
            continue;
        }
        out.push(match it.next()? {
            b'\\' => b'\\',
            b'n' => b'\n',
            b'r' => b'\r',
            _ => return None,
        });
    }
    Some(out)
}

fn is_blank(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

/// A parsed `--check` line: the expected digest (lower-case hex) and the file.
struct CheckLine {
    digest: String,
    file: Vec<u8>,
}

/// How earlier lines were written: GNU refuses to mix the standard format
/// with the "reversed BSD" one (`DIGEST NAME` with a single blank).
#[derive(Clone, Copy, PartialEq, Eq)]
enum LineStyle {
    Unknown,
    Standard,
    BsdReversed,
}

/// Parses one checksum line like GNU's `split_3`: `DIGEST  NAME`,
/// `DIGEST *NAME`, or `ALGO (NAME) = DIGEST`, optionally `\`-escaped.
fn parse_check_line(line: &[u8], algo: Algo, style: &mut LineStyle) -> Option<CheckLine> {
    let mut i = line.iter().take_while(|&&c| is_blank(c)).count();
    let escaped = line.get(i) == Some(&b'\\');
    if escaped {
        i += 1;
    }
    let name = algo.type_name().as_bytes();
    if line[i..].starts_with(name) {
        let mut j = i + name.len();
        let mut hex_len = algo.hex_len();
        if let Algo::Blake2b(_) = algo {
            hex_len = 128;
            if line.get(j) == Some(&b'-') {
                let digits = line[j + 1..]
                    .iter()
                    .take_while(|c| c.is_ascii_digit())
                    .count();
                let bits: usize = std::str::from_utf8(&line[j + 1..j + 1 + digits])
                    .ok()?
                    .parse()
                    .ok()?;
                if bits == 0 || bits > 512 || !bits.is_multiple_of(8) {
                    return None;
                }
                hex_len = bits / 4;
                j += 1 + digits;
            }
        }
        if line.get(j) == Some(&b' ') {
            j += 1;
        }
        if line.get(j) != Some(&b'(') {
            return None;
        }
        return parse_bsd_tail(&line[j + 1..], hex_len, escaped);
    }

    let rest = &line[i..];
    let hex_len = match algo {
        Algo::Blake2b(_) => {
            let n = rest.iter().take_while(|c| c.is_ascii_hexdigit()).count();
            if n < 2 || !n.is_multiple_of(2) || n > 128 {
                return None;
            }
            n
        }
        _ => {
            if rest.len() < algo.hex_len() + 2 {
                return None;
            }
            algo.hex_len()
        }
    };
    let digest = rest.get(..hex_len)?;
    if !digest.iter().all(u8::is_ascii_hexdigit) || !rest.get(hex_len).is_some_and(|&c| is_blank(c))
    {
        return None;
    }
    let mut k = hex_len + 1;
    let reversed = rest.len() - k == 1 || !matches!(rest.get(k), Some(b' ' | b'*'));
    if reversed {
        if *style == LineStyle::Standard {
            return None;
        }
        *style = LineStyle::BsdReversed;
    } else if *style != LineStyle::BsdReversed {
        *style = LineStyle::Standard;
        k += 1;
    }
    let file = &rest[k..];
    let file = if escaped {
        unescape_file_name(file)?
    } else {
        file.to_vec()
    };
    Some(CheckLine {
        digest: String::from_utf8_lossy(digest).to_ascii_lowercase(),
        file,
    })
}

/// The part of a BSD-style line after `ALGO (`: `NAME) = DIGEST`.
fn parse_bsd_tail(s: &[u8], hex_len: usize, escaped: bool) -> Option<CheckLine> {
    let close = s.iter().rposition(|&c| c == b')')?;
    let file = &s[..close];
    let file = if escaped {
        unescape_file_name(file)?
    } else {
        file.to_vec()
    };
    let mut j = close + 1;
    while s.get(j).is_some_and(|&c| is_blank(c)) {
        j += 1;
    }
    if s.get(j) != Some(&b'=') {
        return None;
    }
    j += 1;
    while s.get(j).is_some_and(|&c| is_blank(c)) {
        j += 1;
    }
    let digest = &s[j..];
    if digest.len() != hex_len || !digest.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    Some(CheckLine {
        digest: String::from_utf8_lossy(digest).to_ascii_lowercase(),
        file,
    })
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

/// Per-list counters behind GNU's closing `WARNING:` lines and exit status.
#[derive(Default)]
struct CheckTally {
    misformatted: usize,
    unreadable: usize,
    mismatched: usize,
    properly_formatted: bool,
    matched: bool,
}

fn verify_checksums(
    algo: Algo,
    sources: &[InputSource],
    mode: &HashMode,
    ctx: &DogfishContext,
) -> Result<i32> {
    let tool = ctx.tool_name.as_str();
    let mut out = io::BufWriter::new(stdout::stdout());
    let mut json_results = Vec::new();
    let mut all_ok = true;
    let mut style = LineStyle::Unknown;

    for src in sources {
        let list_name = gnu_quote_file(match src {
            InputSource::Stdin => "standard input",
            InputSource::File(p) => p,
        })
        .into_owned();
        let mut reader = match FastReader::open(src) {
            Ok(r) => r.into_bufread(),
            Err(e) => {
                ignore_broken_pipe(out.flush())?;
                eprintln!("{tool}: {list_name}: {}", gnu_io_error(&e));
                all_ok = false;
                continue;
            }
        };
        let mut tally = CheckTally::default();
        let mut read_error = false;
        let mut line_no = 0usize;
        let mut raw = Vec::new();
        loop {
            raw.clear();
            match reader.read_until(b'\n', &mut raw) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => {
                    read_error = true;
                    break;
                }
            }
            line_no += 1;
            if raw.first() == Some(&b'#') {
                continue;
            }
            let line = raw.strip_suffix(b"\n").unwrap_or(&raw);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() {
                continue;
            }
            let Some(entry) = parse_check_line(line, algo, &mut style) else {
                tally.misformatted += 1;
                if mode.warn {
                    ignore_broken_pipe(out.flush())?;
                    eprintln!(
                        "{tool}: {list_name}: {line_no}: improperly formatted {} checksum line",
                        algo.type_name()
                    );
                }
                continue;
            };
            tally.properly_formatted = true;
            let file = String::from_utf8_lossy(&entry.file).into_owned();
            let shown = match escape_file_name(&file) {
                Some(esc) => format!("\\{esc}"),
                None => file.clone(),
            };
            let target = if file == "-" {
                InputSource::Stdin
            } else {
                InputSource::File(file.clone())
            };
            let check_algo = match algo {
                Algo::Blake2b(_) => Algo::Blake2b(entry.digest.len() * 4),
                a => a,
            };
            match compute_source_hash(check_algo, &target, ctx) {
                Err(e) if mode.ignore_missing && e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    tally.unreadable += 1;
                    ignore_broken_pipe(out.flush())?;
                    eprintln!("{tool}: {}: {}", gnu_quote_file(&file), gnu_io_error(&e));
                    if mode.json {
                        json_results.push(serde_json::json!({
                            "file": file,
                            "status": "FAILED open or read",
                            "expected": entry.digest,
                        }));
                    } else if !mode.status_only {
                        ignore_broken_pipe(writeln!(out, "{shown}: FAILED open or read"))?;
                    }
                }
                Ok(actual) => {
                    tally.matched = true;
                    let ok = actual.digest == entry.digest;
                    if !ok {
                        tally.mismatched += 1;
                    }
                    let verdict = if ok { "OK" } else { "FAILED" };
                    if mode.json {
                        json_results.push(serde_json::json!({
                            "file": file,
                            "status": verdict,
                            "expected": entry.digest,
                            "actual": actual.digest,
                        }));
                    } else if !mode.status_only && (!ok || !mode.quiet) {
                        ignore_broken_pipe(writeln!(out, "{shown}: {verdict}"))?;
                    }
                }
            }
        }
        ignore_broken_pipe(out.flush())?;
        if read_error {
            eprintln!("{tool}: {list_name}: read error");
            all_ok = false;
            continue;
        }
        report_tally(tool, &list_name, &tally, mode);
        all_ok &= tally.properly_formatted
            && tally.matched
            && tally.mismatched == 0
            && tally.unreadable == 0
            && !(mode.strict && tally.misformatted > 0);
    }

    if mode.json {
        writeln!(out, "{}", serde_json::to_string_pretty(&json_results)?)?;
    }
    ignore_broken_pipe(out.flush())?;
    Ok(if all_ok { 0 } else { 1 })
}

fn report_tally(tool: &str, list_name: &str, tally: &CheckTally, mode: &HashMode) {
    if !tally.properly_formatted {
        eprintln!("{tool}: {list_name}: no properly formatted checksum lines found");
    } else if !mode.status_only {
        let warnings = [
            (
                tally.misformatted,
                "line is improperly formatted",
                "lines are improperly formatted",
            ),
            (
                tally.unreadable,
                "listed file could not be read",
                "listed files could not be read",
            ),
            (
                tally.mismatched,
                "computed checksum did NOT match",
                "computed checksums did NOT match",
            ),
        ];
        for (n, one, many) in warnings {
            if n > 0 {
                eprintln!("{tool}: WARNING: {n} {}", plural(n, one, many));
            }
        }
    }
    if mode.ignore_missing && tally.properly_formatted && !tally.matched {
        eprintln!("{tool}: {list_name}: no file was verified");
    }
}
