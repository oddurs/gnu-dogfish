//! b2sum, sha256sum, md5sum — one engine, as in GNU (`md5sum.c`).

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use anyhow::Result;
use blake2::Blake2bVar;
use digest::{Digest, Update, VariableOutput};
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, RecordWriter, gnu_io_error,
    gnu_quote, stdout,
};
use md5::Md5;
use rayon::prelude::*;

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy, print_help, print_version,
};
use crate::common::json_opt::{check_json_version, json_long, json_short};
use crate::common::quote::qf;
use crate::common::sha256::Sha256;

/// SHA-256 ([`Sha256`]: `ring`, or an AVX2 path where that is faster), adapted
/// to the RustCrypto [`Digest`] interface the hashing pipeline is generic over.
///
/// `ring` carries the OpenSSL/BoringSSL assembly with runtime CPU dispatch (SHA-NI,
/// AVX, SSSE3, ARMv8 crypto); GNU `sha256sum` reaches OpenSSL's through libcrypto,
/// including its AVX2 code, which [`Sha256`] matches. RustCrypto's `sha2` only
/// accelerates SHA-NI and ARMv8, so it ran ~1.7x slower than GNU on CPUs without
/// SHA extensions.
struct RingSha256(Sha256);

impl Default for RingSha256 {
    fn default() -> Self {
        Self(Sha256::new())
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
        out.copy_from_slice(&self.0.finish());
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

const B2SUM_HELP: &str = "\
Print or check BLAKE2b (512-bit) checksums. With no FILE, or when FILE is -, read standard input.

Usage: b2sum [OPTIONS] [FILES]...

Arguments:
  [FILES]...  Files to hash (defaults to stdin `-`)

Options:
  -l, --length <BITS>   Digest length in bits; must not exceed the maximum for the blake2 algorithm
                        and must be a multiple of 8
  -b, --binary...       Read in binary mode
  -c, --check           Read checksums from the FILEs and check them
      --tag...          Create a BSD-style checksum
  -t, --text...         Read in text mode (default)
  -z, --zero            End each output line with NUL, not newline, and disable file name escaping
      --ignore-missing  Don't fail or report status for missing files (with --check)
      --quiet...        Don't print OK for each successfully verified file (with --check)
      --status...       Don't output anything, status code shows success (with --check)
      --strict          Exit non-zero for improperly formatted checksum lines (with --check)
  -w, --warn...         Warn about improperly formatted checksum lines (with --check)
  -J, --json[=VERSION]  One NDJSON record per file (with --check: per line)
      --help            Display this help and exit
";

const SHA256SUM_HELP: &str = "\
Print or check checksums. With no FILE, or when FILE is -, read standard input.

Usage: sha256sum [OPTIONS] [FILES]...

Arguments:
  [FILES]...  Files to hash (defaults to stdin `-`)

Options:
  -b, --binary...       Read in binary mode
  -c, --check           Read checksums from the FILEs and check them
      --tag...          Create a BSD-style checksum
  -t, --text...         Read in text mode (default)
  -z, --zero            End each output line with NUL, not newline, and disable file name escaping
      --ignore-missing  Don't fail or report status for missing files (with --check)
      --quiet...        Don't print OK for each successfully verified file (with --check)
      --status...       Don't output anything, status code shows success (with --check)
      --strict          Exit non-zero for improperly formatted checksum lines (with --check)
  -w, --warn...         Warn about improperly formatted checksum lines (with --check)
  -J, --json[=VERSION]  One NDJSON record per file (with --check: per line)
      --help            Display this help and exit
";

const MD5SUM_HELP: &str = "\
Print or check checksums. With no FILE, or when FILE is -, read standard input.

Usage: md5sum [OPTIONS] [FILES]...

Arguments:
  [FILES]...  Files to hash (defaults to stdin `-`)

Options:
  -b, --binary...       Read in binary mode
  -c, --check           Read checksums from the FILEs and check them
      --tag...          Create a BSD-style checksum
  -t, --text...         Read in text mode (default)
  -z, --zero            End each output line with NUL, not newline, and disable file name escaping
      --ignore-missing  Don't fail or report status for missing files (with --check)
      --quiet...        Don't print OK for each successfully verified file (with --check)
      --status...       Don't output anything, status code shows success (with --check)
      --strict          Exit non-zero for improperly formatted checksum lines (with --check)
  -w, --warn...         Warn about improperly formatted checksum lines (with --check)
  -J, --json[=VERSION]  One NDJSON record per file (with --check: per line)
      --help            Display this help and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum SumOpt {
    Length,
    Check,
    IgnoreMissing,
    Quiet,
    Status,
    Warn,
    Strict,
    Tag,
    Zero,
    Binary,
    Text,
    Json,
    Help,
    Version,
}

/// GNU's `long_options` (`--length` for b2sum only) and `"l:bctwz"` /
/// `"bctwz"`, plus the Dogfish `-J, --json`.
const B2SUM_SPECS: &[OptSpec<SumOpt>] = &[
    OptSpec::both(b'l', "length", HasArg::Required, SumOpt::Length),
    OptSpec::both(b'c', "check", HasArg::No, SumOpt::Check),
    OptSpec::long("ignore-missing", HasArg::No, SumOpt::IgnoreMissing),
    OptSpec::long("quiet", HasArg::No, SumOpt::Quiet),
    OptSpec::long("status", HasArg::No, SumOpt::Status),
    OptSpec::both(b'w', "warn", HasArg::No, SumOpt::Warn),
    OptSpec::long("strict", HasArg::No, SumOpt::Strict),
    OptSpec::long("tag", HasArg::No, SumOpt::Tag),
    OptSpec::both(b'z', "zero", HasArg::No, SumOpt::Zero),
    OptSpec::both(b'b', "binary", HasArg::No, SumOpt::Binary),
    OptSpec::both(b't', "text", HasArg::No, SumOpt::Text),
    OptSpec::long("help", HasArg::No, SumOpt::Help),
    OptSpec::long("version", HasArg::No, SumOpt::Version),
    json_short(SumOpt::Json),
    json_long(SumOpt::Json),
];

/// `tool`'s table: b2sum's, or the others' without `--length`.
fn specs_for(tool: &str) -> &'static [OptSpec<SumOpt>] {
    if tool == "b2sum" {
        B2SUM_SPECS
    } else {
        &B2SUM_SPECS[1..]
    }
}

/// The options of a `*sum` tool, resolved the way GNU's option loop does
/// (`--tag` implies binary mode; --status/--quiet/--warn override each other).
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
    /// b2sum's digest length in bits.
    bits: usize,
    files: Vec<OsString>,
}

impl HashMode {
    /// Parses `tool`'s command line; `Err` is the exit status after
    /// `--help`, `--version` or a diagnostic.
    fn parse(tool: &str, args: Vec<OsString>, help: &str) -> std::result::Result<Self, i32> {
        let mut m = HashMode {
            check: false,
            tag: false,
            binary: None,
            status_only: false,
            quiet: false,
            warn: false,
            strict: false,
            ignore_missing: false,
            zero: false,
            json: false,
            bits: 512,
            files: Vec::new(),
        };
        let mut opts = Getopt::new(args.into_iter().skip(1).collect(), specs_for(tool));
        while let Some(opt) = opts.next() {
            let (o, v) = opt.map_err(|e| getopt_error(tool, &e))?;
            match o {
                SumOpt::Length => {
                    m.bits = blake2_length(&v.as_deref().map(lossy).unwrap_or_default())?
                }
                SumOpt::Check => m.check = true,
                SumOpt::IgnoreMissing => m.ignore_missing = true,
                SumOpt::Status => (m.status_only, m.warn, m.quiet) = (true, false, false),
                SumOpt::Warn => (m.status_only, m.warn, m.quiet) = (false, true, false),
                SumOpt::Quiet => (m.status_only, m.warn, m.quiet) = (false, false, true),
                SumOpt::Strict => m.strict = true,
                SumOpt::Tag => (m.tag, m.binary) = (true, Some(true)),
                SumOpt::Zero => m.zero = true,
                SumOpt::Binary => m.binary = Some(true),
                SumOpt::Text => m.binary = Some(false),
                SumOpt::Json => {
                    check_json_version(tool, v.as_deref())?;
                    m.json = true;
                }
                SumOpt::Help => return Err(print_help(help)),
                SumOpt::Version => return Err(print_version(tool)),
            }
        }
        m.files = opts.operands();
        Ok(m)
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

#[derive(Clone)]
struct HashRecord {
    algorithm: String,
    /// The file name as given (raw bytes), for the GNU output lines.
    name: Vec<u8>,
    digest: String,
    bytes: u64,
}

/// A file to hash or a checksum list to read: names stay raw bytes, so one that
/// is not UTF-8 is opened and printed exactly as given.
enum Source {
    Stdin,
    File(OsString),
}

impl Source {
    fn from_args(files: &[OsString]) -> Vec<Self> {
        if files.is_empty() {
            return vec![Self::Stdin];
        }
        files
            .iter()
            .map(|f| {
                if f == "-" {
                    Self::Stdin
                } else {
                    Self::File(f.clone())
                }
            })
            .collect()
    }

    fn name(&self) -> &OsStr {
        match self {
            Self::Stdin => OsStr::new("-"),
            Self::File(p) => p,
        }
    }

    /// Opens the input: `mmap` through `FastReader` when the name is UTF-8 (all
    /// `InputSource` can carry), a plain buffered read otherwise.
    fn open(&self) -> io::Result<FastReader> {
        match self {
            Self::Stdin => FastReader::open(&InputSource::Stdin),
            Self::File(p) => match p.to_str() {
                Some(s) => FastReader::open(&InputSource::File(s.to_owned())),
                None => Ok(FastReader::Buffered(BufReader::with_capacity(
                    DEFAULT_BUF_SIZE,
                    Box::new(File::open(p)?),
                ))),
            },
        }
    }
}

/// b2sum's `--length`: a multiple of 8 up to 512 bits, 0 meaning 512.
fn blake2_length(l: &str) -> std::result::Result<usize, i32> {
    let detail = match l.trim_start().parse::<u64>() {
        Err(_) => None,
        Ok(n) if n > 512 => Some("maximum digest length for 'BLAKE2b' is 512 bits"),
        Ok(n) if !n.is_multiple_of(8) => Some("length is not a multiple of 8"),
        Ok(0) => return Ok(512),
        Ok(n) => return Ok(n as usize),
    };
    eprintln!("b2sum: invalid length: {}", gnu_quote(l));
    if let Some(detail) = detail {
        eprintln!("b2sum: {detail}");
    }
    Err(1)
}

// GNU's digest.c line buffers standard output before parsing options.

pub fn run_b2sum(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    stdout::gnu_flushes_early();
    match HashMode::parse("b2sum", args, B2SUM_HELP) {
        Ok(mode) => run_sum(Algo::Blake2b(mode.bits), mode, ctx),
        Err(code) => Ok(code),
    }
}

pub fn run_sha256sum(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    stdout::gnu_flushes_early();
    match HashMode::parse("sha256sum", args, SHA256SUM_HELP) {
        Ok(mode) => run_sum(Algo::Sha256, mode, ctx),
        Err(code) => Ok(code),
    }
}

pub fn run_md5sum(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    stdout::gnu_flushes_early();
    match HashMode::parse("md5sum", args, MD5SUM_HELP) {
        Ok(mode) => run_sum(Algo::Md5, mode, ctx),
        Err(code) => Ok(code),
    }
}

fn run_sum(algo: Algo, mode: HashMode, ctx: &DogfishContext) -> Result<i32> {
    let tool = ctx.tool_name.as_str();
    if let Some(msg) = mode.validate() {
        eprintln!("{tool}: {msg}\nTry '{tool} --help' for more information.");
        return Ok(1);
    }
    let sources = Source::from_args(&mode.files);

    if mode.check {
        return verify_checksums(algo, &sources, &mode, ctx);
    }

    ctx.telemetry.set_strategy(if sources.len() > 1 {
        format!("rayon parallel {} + mmap", algo.type_name())
    } else {
        format!("{} + mmap", algo.type_name())
    });

    // If multiple file sources and none is stdin, hash in parallel using Rayon!
    let can_parallel = sources.len() > 1 && sources.iter().all(|s| matches!(s, Source::File(_)));
    let hash_one = |src: &Source| compute_source_hash(algo, src, ctx);
    let results: Vec<io::Result<HashRecord>> = if can_parallel {
        sources.par_iter().map(hash_one).collect()
    } else {
        sources.iter().map(hash_one).collect()
    };

    let mut exit_code = 0;
    // Line buffered, and never checked: as in GNU, every file is done and a
    // write error is reported at exit.
    let mut out = stdout::stdout();
    let mut records = mode.json.then(RecordWriter::stdout);
    for (src, res) in sources.iter().zip(results) {
        match res {
            Ok(rec) => match &mut records {
                Some(w) => w.record(|r| {
                    r.bytes("path", &rec.name)
                        .str("algorithm", &rec.algorithm)
                        .str("digest", &rec.digest)
                        .u64("bytes", rec.bytes);
                })?,
                None => {
                    let _ = write_digest_line(&mut out, algo, &rec, &mode);
                }
            },
            Err(e) => {
                if let Some(w) = &mut records {
                    w.flush()?;
                }
                eprintln!("{tool}: {}: {}", qf(src.name()), gnu_io_error(&e));
                exit_code = 1;
            }
        }
    }
    if let Some(w) = &mut records {
        w.flush()?;
    }
    Ok(exit_code)
}

/// GNU escapes `\`, newline and carriage return in file names, and marks such
/// lines with a leading backslash. Other bytes, UTF-8 or not, are kept.
fn escape_file_name(name: &[u8]) -> Option<Vec<u8>> {
    if !name.iter().any(|c| matches!(c, b'\\' | b'\n' | b'\r')) {
        return None;
    }
    let mut out = Vec::with_capacity(name.len() + 4);
    for &c in name {
        match c {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            c => out.push(c),
        }
    }
    Some(out)
}

/// A file name as a `--check` verdict line shows it (escaped, `\`-prefixed).
fn shown_name(name: &[u8]) -> Vec<u8> {
    match escape_file_name(name) {
        Some(esc) => [b"\\".as_slice(), &esc].concat(),
        None => name.to_vec(),
    }
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
        escape_file_name(&rec.name)
    };
    let name = escaped.as_deref().unwrap_or(&rec.name);
    let mut line = Vec::with_capacity(rec.digest.len() + name.len() + 16);
    if escaped.is_some() {
        line.push(b'\\');
    }
    if mode.tag {
        line.extend_from_slice(algo.tag().as_bytes());
        line.extend_from_slice(b" (");
        line.extend_from_slice(name);
        line.extend_from_slice(b") = ");
        line.extend_from_slice(rec.digest.as_bytes());
    } else {
        line.extend_from_slice(rec.digest.as_bytes());
        line.push(b' ');
        line.push(if mode.binary == Some(true) {
            b'*'
        } else {
            b' '
        });
        line.extend_from_slice(name);
    }
    line.push(if mode.zero { b'\0' } else { b'\n' });
    out.write_all(&line)
}

fn compute_source_hash(algo: Algo, src: &Source, ctx: &DogfishContext) -> io::Result<HashRecord> {
    let mut hasher = algo.hasher();
    let mut bytes_len = 0u64;
    match src.open()? {
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
        name: src.name().as_bytes().to_vec(),
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
    sources: &[Source],
    mode: &HashMode,
    ctx: &DogfishContext,
) -> Result<i32> {
    let tool = ctx.tool_name.as_str();
    // Unchecked, as in GNU: every line is verified, and a write error is
    // reported at exit.
    let mut out = stdout::StdioBuf::new(stdout::stdout());
    // -J: one record per checked line, each written at once (diagnostics
    // about the same file follow it in order).
    let mut records = mode
        .json
        .then(|| RecordWriter::stdout().line_buffered(true));
    let mut all_ok = true;
    let mut style = LineStyle::Unknown;

    for src in sources {
        let list_name = qf(match src {
            Source::Stdin => OsStr::new("standard input"),
            Source::File(p) => p,
        });
        let mut reader = match src.open() {
            Ok(r) => r.into_bufread(),
            Err(e) => {
                let _ = out.flush();
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
                    let _ = out.flush();
                    eprintln!(
                        "{tool}: {list_name}: {line_no}: improperly formatted {} checksum line",
                        algo.type_name()
                    );
                }
                continue;
            };
            tally.properly_formatted = true;
            let mut shown = shown_name(&entry.file);
            let file = entry.file.clone();
            let target = if entry.file == b"-" {
                Source::Stdin
            } else {
                Source::File(OsString::from_vec(entry.file))
            };
            let check_algo = match algo {
                Algo::Blake2b(_) => Algo::Blake2b(entry.digest.len() * 4),
                a => a,
            };
            match compute_source_hash(check_algo, &target, ctx) {
                Err(e) if mode.ignore_missing && e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    tally.unreadable += 1;
                    let _ = out.flush();
                    eprintln!("{tool}: {}: {}", qf(target.name()), gnu_io_error(&e));
                    if let Some(w) = &mut records {
                        w.record(|r| {
                            r.bytes("path", &file)
                                .str("status", "unreadable")
                                .str("expected", &entry.digest);
                        })?;
                    } else if !mode.status_only {
                        shown.extend_from_slice(b": FAILED open or read\n");
                        let _ = out.write_all(&shown);
                    }
                }
                Ok(actual) => {
                    tally.matched = true;
                    let ok = actual.digest == entry.digest;
                    if !ok {
                        tally.mismatched += 1;
                    }
                    let verdict = if ok { "OK" } else { "FAILED" };
                    if let Some(w) = &mut records {
                        w.record(|r| {
                            r.bytes("path", &file)
                                .str("status", if ok { "ok" } else { "failed" })
                                .str("expected", &entry.digest)
                                .str("actual", &actual.digest);
                        })?;
                    } else if !mode.status_only && (!ok || !mode.quiet) {
                        shown.extend_from_slice(format!(": {verdict}\n").as_bytes());
                        let _ = out.write_all(&shown);
                    }
                }
            }
        }
        let _ = out.flush();
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

    let _ = out.flush();
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

/// The option table and help text of `tool` (b2sum, sha256sum or md5sum),
/// for its manual page and shell completions (`crate::manual`).
pub(crate) fn manual(tool: &'static str) -> crate::manual::Manual {
    let help = match tool {
        "b2sum" => B2SUM_HELP,
        "sha256sum" => SHA256SUM_HELP,
        _ => MD5SUM_HELP,
    };
    crate::manual::Manual::new(tool, help, specs_for(tool))
}
