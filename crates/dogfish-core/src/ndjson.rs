//! The structured-output contract behind every tool's `-J, --json`
//! (`doc/ndjson.md` states it for users; this module implements it once).
//!
//! - **Framing**: NDJSON. One compact JSON object per line, UTF-8, ended by
//!   `\n`; never an array, never pretty-printed. Records are written as they
//!   are produced ([`RecordWriter`]'s flush policy says when they reach the
//!   file descriptor).
//! - **Types**: sizes and counts are integers. A time is two fields: an
//!   RFC 3339 UTC string with nanoseconds and an integer of nanoseconds
//!   since the epoch, `mtime` and `mtime_ns` ([`Record::time`]). A mode is
//!   an octal string and a symbolic one, `mode` and `mode_symbolic`
//!   ([`Record::mode`]). A path or other name from the system is a string as
//!   received; when its bytes are not UTF-8 the string is the lossy form and
//!   `<field>_bytes` holds the bytes in base64 ([`Record::bytes`]).
//! - **Names**: snake_case and shared across tools (the glossary in
//!   `doc/ndjson.md`). No presentation fields: no human-readable sizes,
//!   relative times or colours.
//! - **Versions**: what is described here is version [`VERSION`]; `--json`
//!   and `-J` select it, and `--json=N` names one explicitly
//!   ([`parse_version`]).
//!
//! A tool writes a record with [`RecordWriter::record`], filling the fields
//! in a closure: they are encoded straight into the output buffer, with no
//! intermediate `serde_json::Value`. A failure that the tool also reports on
//! standard error goes in the stream too, as an error record
//! ([`RecordWriter::error`]), so that a pipeline sees it in order.
//!
//! Most error records are not written by the tools themselves: once `-J` is
//! given ([`capture_errors`]), every diagnostic printed through
//! [`crate::gnu_error!`] (which `eprintln!` is in the tools) also becomes an
//! error record ([`diagnostic`]), queued for the next record a
//! [`RecordWriter::stdout`] writes, so that it lands in order.

use std::cell::RefCell;
use std::io::{self, IsTerminal, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Serialize;

use crate::stdout::{Stdout, stdout};

/// The record format version `-J` and `--json` select.
pub const VERSION: u32 = 1;

/// The versions `--json=VERSION` accepts, as spelled on the command line.
pub const VERSIONS: &[&str] = &["1"];

/// Why `--json=VERSION` was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionError {
    pub given: String,
}

impl std::fmt::Display for VersionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unsupported JSON version '{}' (supported: {})",
            self.given,
            VERSIONS.join(", ")
        )
    }
}

/// The version asked for by `--json[=VERSION]` (`None`: `-J` or a bare
/// `--json`, which mean the current version).
pub fn parse_version(arg: Option<&[u8]>) -> Result<u32, VersionError> {
    match arg {
        None | Some(b"1") => Ok(VERSION),
        Some(other) => Err(VersionError {
            given: String::from_utf8_lossy(other).into_owned(),
        }),
    }
}

/// Buffered records go out once they hold this much.
pub const FLUSH_BYTES: usize = 32 * 1024;

/// Buffered records go out with the next record written this long after the
/// last write, so a slow producer's records are not held back.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(10);

/// Writes NDJSON records to `W` (standard output by default).
///
/// Records are buffered and written in blocks of up to [`FLUSH_BYTES`]; a
/// record is also written at once when [`FLUSH_INTERVAL`] has passed since
/// the last write (a producer that is slow between records streams them
/// one by one), and every record is when the output is a terminal. Write
/// errors are returned from the call that meets them; on standard output
/// they are also sticky, as for every other write ([`crate::stdout`]).
pub struct RecordWriter<W: Write = Stdout> {
    out: W,
    buf: Vec<u8>,
    last_write: Instant,
    line_buffered: bool,
    /// Takes the error records of diagnostics ([`diagnostic`]) into its
    /// stream, as the writers to standard output do.
    takes_errors: bool,
}

impl RecordWriter<Stdout> {
    /// Records to standard output, line-buffered when it is a terminal.
    /// While it lives, the error record of each diagnostic goes into its
    /// stream, before the next record written.
    pub fn stdout() -> Self {
        let tty = io::stdout().is_terminal();
        WRITERS.fetch_add(1, Ordering::AcqRel);
        let mut w = Self::new(stdout()).line_buffered(tty);
        w.takes_errors = true;
        w
    }
}

impl<W: Write> RecordWriter<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            buf: Vec::with_capacity(FLUSH_BYTES + 4096),
            last_write: Instant::now(),
            line_buffered: false,
            takes_errors: false,
        }
    }

    /// Moves the error records queued by diagnostics into the buffer, so
    /// that they come before whatever is written next.
    fn take_errors(&mut self) {
        if self.takes_errors && HAVE_PENDING.load(Ordering::Acquire) {
            let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
            self.buf.append(&mut pending);
            HAVE_PENDING.store(false, Ordering::Release);
        }
    }

    /// Writes every record as soon as it is complete.
    pub fn line_buffered(mut self, on: bool) -> Self {
        self.line_buffered = on;
        self
    }

    /// Writes one record, its fields filled in by `fill`.
    pub fn record(&mut self, fill: impl FnOnce(&mut Record<'_>)) -> io::Result<()> {
        self.take_errors();
        self.buf.push(b'{');
        let mut rec = Record {
            buf: &mut self.buf,
            empty: true,
        };
        fill(&mut rec);
        self.buf.extend_from_slice(b"}\n");
        self.written()
    }

    /// Writes `value`, which must serialize as a JSON object, as one record.
    pub fn serialize<T: Serialize + ?Sized>(&mut self, value: &T) -> io::Result<()> {
        self.take_errors();
        let start = self.buf.len();
        serde_json::to_writer(&mut self.buf, value).map_err(io::Error::other)?;
        if self.buf.get(start) != Some(&b'{') {
            self.buf.truncate(start);
            return Err(io::Error::other("a record must be a JSON object"));
        }
        self.buf.push(b'\n');
        self.written()
    }

    /// Writes a record that is already one compact JSON object (a line read
    /// from another producer), without its line terminator.
    pub fn line(&mut self, object: &[u8]) -> io::Result<()> {
        self.take_errors();
        self.buf.extend_from_slice(object);
        self.buf.push(b'\n');
        self.written()
    }

    /// Writes an error record: the structured twin of a diagnostic the tool
    /// prints on standard error (which it still does, in GNU's form).
    pub fn error(&mut self, err: &ErrorRecord<'_>) -> io::Result<()> {
        self.record(|r| err.fill(r))
    }

    /// Writes out every buffered record.
    pub fn flush(&mut self) -> io::Result<()> {
        self.take_errors();
        if !self.buf.is_empty() {
            let res = self.out.write_all(&self.buf);
            self.buf.clear();
            self.last_write = Instant::now();
            res?;
        }
        self.out.flush()
    }

    /// The underlying writer (after a [`RecordWriter::flush`]).
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.out
    }

    fn written(&mut self) -> io::Result<()> {
        if self.line_buffered
            || self.buf.len() >= FLUSH_BYTES
            || self.last_write.elapsed() >= FLUSH_INTERVAL
        {
            self.flush()
        } else {
            Ok(())
        }
    }
}

impl<W: Write> Drop for RecordWriter<W> {
    /// Writes what is left; a failure here is sticky on standard output and
    /// reported at exit. Tools call [`RecordWriter::flush`] to see it sooner.
    fn drop(&mut self) {
        let _ = self.flush();
        if self.takes_errors {
            WRITERS.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

// ------------------------------------------------- diagnostics as records

/// `-J` was given: diagnostics become error records too.
static CAPTURE: AtomicBool = AtomicBool::new(false);
/// The program name that starts each diagnostic (`rm: ...`).
static TOOL: Mutex<String> = Mutex::new(String::new());
/// Live [`RecordWriter::stdout`] writers; with none, an error record is
/// written to standard output at once.
static WRITERS: AtomicUsize = AtomicUsize::new(0);
/// Error records waiting for the next record of a live writer.
static PENDING: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static HAVE_PENDING: AtomicBool = AtomicBool::new(false);

/// How many names quoted since the last diagnostic are remembered.
const QUOTED_KEPT: usize = 16;

thread_local! {
    /// Names quoted for messages since the last diagnostic: the bytes as
    /// given, and as quoted ([`note_quoted`]).
    static QUOTED: RefCell<Vec<(Vec<u8>, String)>> = const { RefCell::new(Vec::new()) };
}

/// From now on, every diagnostic `tool` prints through
/// [`crate::gnu_error!`] also writes an error record to standard output
/// ([`diagnostic`]). Called when `-J` is given.
pub fn capture_errors(tool: &str) {
    let mut t = TOOL.lock().unwrap_or_else(|e| e.into_inner());
    t.clear();
    t.push_str(tool);
    CAPTURE.store(true, Ordering::Release);
}

/// Diagnostics are no longer records: called at exit, before standard
/// output is closed (a write error found there has no stream to go to).
pub fn stop_capture() {
    CAPTURE.store(false, Ordering::Release);
}

/// Whether diagnostics are turned into records.
#[inline]
pub fn capturing() -> bool {
    CAPTURE.load(Ordering::Relaxed)
}

/// Remembers that `raw` (a file name as given) was quoted as `quoted` for
/// a message, so that a diagnostic naming it can give its bytes as `path`.
/// Does nothing unless [`capturing`].
#[inline]
pub fn note_quoted(raw: &[u8], quoted: &[u8]) {
    if capturing() {
        QUOTED.with(|q| {
            let mut q = q.borrow_mut();
            if q.len() == QUOTED_KEPT {
                q.remove(0);
            }
            q.push((raw.to_vec(), String::from_utf8_lossy(quoted).into_owned()));
        });
    }
}

/// Writes the error record of `msg`, a diagnostic just written to standard
/// error (one line, without its newline), when [`capturing`]:
///
/// - `error` is the message without the `tool: ` prefix. A line that does
///   not start with it (`Try 'rm --help' ...`) writes no record.
/// - `path` is the file the message names first: of the names quoted since
///   the last diagnostic ([`note_quoted`]), the one that appears earliest.
/// - `errno` is the error number whose `strerror` text ends the message.
///
/// The record goes into the stream of a live [`RecordWriter::stdout`],
/// before its next record, or else straight to standard output.
pub fn diagnostic(msg: &str) {
    let quoted = QUOTED.with(|q| std::mem::take(&mut *q.borrow_mut()));
    if !capturing() {
        return;
    }
    let plain = strip_sgr(msg);
    let rest = {
        let tool = TOOL.lock().unwrap_or_else(|e| e.into_inner());
        match plain
            .strip_prefix(tool.as_str())
            .and_then(|r| r.strip_prefix(": "))
        {
            Some(r) => r.to_owned(),
            None => return,
        }
    };
    let mut rec = ErrorRecord::new(&rest);
    if let Some(p) = named_path(&rest, &quoted) {
        rec = rec.path(p);
    }
    rec.errno = errno_of(&rest);
    let mut bytes = vec![b'{'];
    rec.fill(&mut Record {
        buf: &mut bytes,
        empty: true,
    });
    bytes.extend_from_slice(b"}\n");
    if WRITERS.load(Ordering::Acquire) > 0 {
        PENDING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(&bytes);
        HAVE_PENDING.store(true, Ordering::Release);
    } else {
        let _ = stdout().write_all(&bytes);
    }
}

/// `msg` without terminal colour sequences (a diagnostic's program name is
/// bold on a terminal).
fn strip_sgr(msg: &str) -> std::borrow::Cow<'_, str> {
    if !msg.contains('\x1b') {
        return msg.into();
    }
    let mut out = String::with_capacity(msg.len());
    let mut chars = msg.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out.into()
}

/// Of the names `quoted` for a message, the bytes of the one that appears
/// first in `msg`. A name quoted only when it needs it (`quotef`) may stand
/// bare, so a bare one must be a whole word: at the start or after a space,
/// and before `:`, `,`, a space or the end.
fn named_path<'q>(msg: &str, quoted: &'q [(Vec<u8>, String)]) -> Option<&'q [u8]> {
    let mut best: Option<(usize, usize, &[u8])> = None;
    for (raw, q) in quoted {
        if q.is_empty() {
            continue;
        }
        let delimited = q.starts_with(['\'', '"', '$']) || q.starts_with('\u{2018}');
        let found = msg.match_indices(q.as_str()).map(|(i, _)| i).find(|&i| {
            delimited || {
                let before = msg[..i].chars().next_back();
                let after = msg[i + q.len()..].chars().next();
                before.is_none_or(|c| c == ' ')
                    && after.is_none_or(|c| matches!(c, ':' | ',' | ' '))
            }
        });
        if let Some(i) = found
            && best.is_none_or(|(bi, blen, _)| i < bi || (i == bi && q.len() > blen))
        {
            best = Some((i, q.len(), raw));
        }
    }
    best.map(|(_, _, raw)| raw)
}

/// The error number whose message ends `msg` (`...: Permission denied`).
fn errno_of(msg: &str) -> Option<i32> {
    static TABLE: std::sync::OnceLock<Vec<(i32, String)>> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        (1..256)
            .map(|n| {
                (
                    n,
                    crate::format::gnu_io_error(&io::Error::from_raw_os_error(n)),
                )
            })
            .filter(|(_, t)| !t.starts_with("Unknown error"))
            .collect()
    });
    let tail = msg.rsplit_once(": ").map_or(msg, |(_, t)| t);
    table.iter().find(|(_, t)| t == tail).map(|(n, _)| *n)
}

/// An error record: `{"error":MESSAGE,"path":...,"errno":N}`. `message` is
/// the diagnostic without the `tool: ` prefix; `errno` is the system error
/// number when there is one.
#[derive(Debug, Clone, Copy)]
pub struct ErrorRecord<'a> {
    pub message: &'a str,
    pub path: Option<&'a [u8]>,
    pub errno: Option<i32>,
}

impl<'a> ErrorRecord<'a> {
    pub fn new(message: &'a str) -> Self {
        Self {
            message,
            path: None,
            errno: None,
        }
    }

    pub fn path(self, path: &'a [u8]) -> Self {
        Self {
            path: Some(path),
            ..self
        }
    }

    /// The error number of `err`, if it came from the system.
    pub fn io(self, err: &io::Error) -> Self {
        Self {
            errno: err.raw_os_error(),
            ..self
        }
    }

    fn fill(&self, r: &mut Record<'_>) {
        r.str("error", self.message);
        if let Some(p) = self.path {
            r.bytes("path", p);
        }
        if let Some(n) = self.errno {
            r.i64("errno", i64::from(n));
        }
    }
}

/// The fields of one record, encoded into the writer's buffer as they are
/// added. Keys are the tool's own snake_case names (plain ASCII, never
/// escaped); every value is encoded as the contract says.
pub struct Record<'a> {
    buf: &'a mut Vec<u8>,
    empty: bool,
}

impl Record<'_> {
    fn key(&mut self, key: &str) {
        debug_assert!(
            key.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "record keys are snake_case: {key}"
        );
        if !self.empty {
            self.buf.push(b',');
        }
        self.empty = false;
        self.buf.push(b'"');
        self.buf.extend_from_slice(key.as_bytes());
        self.buf.extend_from_slice(b"\":");
    }

    /// A string field.
    pub fn str(&mut self, key: &str, value: &str) -> &mut Self {
        self.key(key);
        push_json_str(self.buf, value);
        self
    }

    /// A text field from bytes as received (a path, a name, a value from the
    /// environment): the string, and when the bytes are not UTF-8 its lossy
    /// form plus `<key>_bytes`, the bytes in base64.
    pub fn bytes(&mut self, key: &str, value: &[u8]) -> &mut Self {
        match std::str::from_utf8(value) {
            Ok(s) => self.str(key, s),
            Err(_) => {
                self.str(key, &String::from_utf8_lossy(value));
                self.key_suffixed(key, "_bytes");
                self.buf.push(b'"');
                push_base64(self.buf, value);
                self.buf.push(b'"');
                self
            }
        }
    }

    /// [`Record::bytes`] for a path or other `OsStr`.
    pub fn os_str(&mut self, key: &str, value: impl AsRef<std::ffi::OsStr>) -> &mut Self {
        use std::os::unix::ffi::OsStrExt as _;
        self.bytes(key, value.as_ref().as_bytes())
    }

    pub fn u64(&mut self, key: &str, value: u64) -> &mut Self {
        self.key(key);
        self.buf
            .extend_from_slice(itoa::Buffer::new().format(value).as_bytes());
        self
    }

    pub fn i64(&mut self, key: &str, value: i64) -> &mut Self {
        self.key(key);
        self.buf
            .extend_from_slice(itoa::Buffer::new().format(value).as_bytes());
        self
    }

    /// A number that need not be an integer; `null` if it is not finite.
    pub fn f64(&mut self, key: &str, value: f64) -> &mut Self {
        self.key(key);
        if value.is_finite() {
            // serde_json writes finite floats; it cannot fail on a Vec.
            let _ = serde_json::to_writer(&mut *self.buf, &value);
        } else {
            self.buf.extend_from_slice(b"null");
        }
        self
    }

    pub fn bool(&mut self, key: &str, value: bool) -> &mut Self {
        self.key(key);
        self.buf
            .extend_from_slice(if value { b"true" } else { b"false" });
        self
    }

    /// Any serializable value (a nested object or array).
    pub fn value<T: Serialize + ?Sized>(&mut self, key: &str, value: &T) -> &mut Self {
        self.key(key);
        let start = self.buf.len();
        if serde_json::to_writer(&mut *self.buf, value).is_err() {
            self.buf.truncate(start);
            self.buf.extend_from_slice(b"null");
        }
        self
    }

    /// A time as two fields: `key`, the RFC 3339 UTC time with nanoseconds
    /// (`2026-10-29T18:03:19.123456789Z`), and `key_ns`, the nanoseconds
    /// since the epoch. `nsec` must be below 10^9.
    pub fn time(&mut self, key: &str, sec: i64, nsec: u32) -> &mut Self {
        self.key(key);
        self.buf.push(b'"');
        push_rfc3339(self.buf, sec, nsec);
        self.buf.push(b'"');
        self.key_suffixed(key, "_ns");
        let ns = i128::from(sec) * 1_000_000_000 + i128::from(nsec);
        self.buf
            .extend_from_slice(itoa::Buffer::new().format(ns).as_bytes());
        self
    }

    /// A file mode as `mode`, the permission bits in octal (`"0644"`), and
    /// `mode_symbolic`, as `ls -l` shows it (`"-rw-r--r--"`).
    pub fn mode(&mut self, mode: u32) -> &mut Self {
        self.mode_as("mode", mode)
    }

    /// [`Record::mode`] under another name: `key` and `key_symbolic`
    /// (`mode_before`, `mode_before_symbolic`).
    pub fn mode_as(&mut self, key: &str, mode: u32) -> &mut Self {
        self.key(key);
        let perm = mode & 0o7777;
        self.buf.extend_from_slice(&[
            b'"',
            b'0' + (perm >> 9) as u8,
            b'0' + ((perm >> 6) & 7) as u8,
            b'0' + ((perm >> 3) & 7) as u8,
            b'0' + (perm & 7) as u8,
            b'"',
        ]);
        self.key_suffixed(key, "_symbolic");
        self.buf.push(b'"');
        self.buf.extend_from_slice(&mode_symbolic(mode));
        self.buf.push(b'"');
        self
    }

    /// The file type of `mode` as `type` ([`file_type_name`]).
    pub fn file_type(&mut self, mode: u32) -> &mut Self {
        self.key("type");
        self.buf.push(b'"');
        self.buf.extend_from_slice(file_type_name(mode).as_bytes());
        self.buf.push(b'"');
        self
    }

    fn key_suffixed(&mut self, key: &str, suffix: &str) {
        self.buf.extend_from_slice(b",\"");
        self.buf.extend_from_slice(key.as_bytes());
        self.buf.extend_from_slice(suffix.as_bytes());
        self.buf.extend_from_slice(b"\":");
    }
}

/// Appends the padded base64 of `data`.
fn push_base64(buf: &mut Vec<u8>, data: &[u8]) {
    let start = buf.len();
    let len = base64::encoded_len(data.len(), true).unwrap_or(0);
    buf.resize(start + len, 0);
    match STANDARD.encode_slice(data, &mut buf[start..]) {
        Ok(n) => buf.truncate(start + n),
        Err(_) => buf.truncate(start),
    }
}

/// The file types of `st_mode`: the `S_IF*` value (as `u32`: `mode_t` is
/// narrower on macOS, so the casts are redundant on Linux), the `type` name
/// and the `ls -l` letter.
#[allow(clippy::unnecessary_cast)]
const FILE_TYPES: [(u32, &str, u8); 7] = [
    (libc::S_IFREG as u32, "file", b'-'),
    (libc::S_IFDIR as u32, "directory", b'd'),
    (libc::S_IFLNK as u32, "symlink", b'l'),
    (libc::S_IFIFO as u32, "fifo", b'p'),
    (libc::S_IFSOCK as u32, "socket", b's'),
    (libc::S_IFBLK as u32, "block_device", b'b'),
    (libc::S_IFCHR as u32, "char_device", b'c'),
];

#[allow(clippy::unnecessary_cast)]
const S_IFMT: u32 = libc::S_IFMT as u32;

fn file_type_of(mode: u32) -> Option<&'static (u32, &'static str, u8)> {
    FILE_TYPES
        .iter()
        .find(|(bits, _, _)| mode & S_IFMT == *bits)
}

/// The `type` of a file from its `st_mode`: `file`, `directory`,
/// `symlink`, `fifo`, `socket`, `block_device`, `char_device`, or `unknown`.
pub fn file_type_name(mode: u32) -> &'static str {
    file_type_of(mode).map_or("unknown", |(_, name, _)| name)
}

/// The ten characters `ls -l` shows for `mode` (gnulib's `filemodestring`).
pub fn mode_symbolic(mode: u32) -> [u8; 10] {
    let kind = file_type_of(mode).map_or(b'?', |(_, _, letter)| *letter);
    let bit = |b: u32, c: u8| if mode & b != 0 { c } else { b'-' };
    let special = |x: u32, s: u32, set: u8| match (mode & s != 0, mode & x != 0) {
        (true, true) => set,
        (true, false) => set.to_ascii_uppercase(),
        (false, true) => b'x',
        (false, false) => b'-',
    };
    [
        kind,
        bit(0o400, b'r'),
        bit(0o200, b'w'),
        special(0o100, 0o4000, b's'),
        bit(0o040, b'r'),
        bit(0o020, b'w'),
        special(0o010, 0o2000, b's'),
        bit(0o004, b'r'),
        bit(0o002, b'w'),
        special(0o001, 0o1000, b't'),
    ]
}

/// Appends `s` as a JSON string.
pub fn push_json_str(buf: &mut Vec<u8>, s: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    buf.push(b'"');
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let esc: &[u8] = match b {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\t' => b"\\t",
            b'\r' => b"\\r",
            0..=0x1f => b"",
            _ => continue,
        };
        buf.extend_from_slice(&bytes[start..i]);
        if esc.is_empty() {
            buf.extend_from_slice(&[
                b'\\',
                b'u',
                b'0',
                b'0',
                HEX[usize::from(b >> 4)],
                HEX[usize::from(b & 15)],
            ]);
        } else {
            buf.extend_from_slice(esc);
        }
        start = i + 1;
    }
    buf.extend_from_slice(&bytes[start..]);
    buf.push(b'"');
}

/// Appends the RFC 3339 UTC form of `sec` seconds and `nsec` nanoseconds
/// since the epoch: `YYYY-MM-DDTHH:MM:SS.NNNNNNNNNZ`. Years outside 0..=9999
/// (which RFC 3339 cannot write) get ISO 8601's expanded form, a sign and
/// at least four digits.
pub fn push_rfc3339(buf: &mut Vec<u8>, sec: i64, nsec: u32) {
    let days = sec.div_euclid(86_400);
    let secs = sec.rem_euclid(86_400) as u32;
    let (year, month, day) = civil_from_days(days);
    if (0..=9999).contains(&year) {
        push_digits(buf, year as u32, 4);
    } else {
        buf.push(if year < 0 { b'-' } else { b'+' });
        let abs = year.unsigned_abs();
        let width = abs.checked_ilog10().map_or(1, |l| l as usize + 1).max(4);
        push_digits_u64(buf, abs, width);
    }
    buf.push(b'-');
    push_digits(buf, month, 2);
    buf.push(b'-');
    push_digits(buf, day, 2);
    buf.push(b'T');
    push_digits(buf, secs / 3600, 2);
    buf.push(b':');
    push_digits(buf, secs / 60 % 60, 2);
    buf.push(b':');
    push_digits(buf, secs % 60, 2);
    buf.push(b'.');
    push_digits(buf, nsec.min(999_999_999), 9);
    buf.push(b'Z');
}

fn push_digits(buf: &mut Vec<u8>, n: u32, width: usize) {
    push_digits_u64(buf, u64::from(n), width);
}

fn push_digits_u64(buf: &mut Vec<u8>, mut n: u64, width: usize) {
    let start = buf.len();
    buf.resize(start + width, b'0');
    for slot in buf[start..].iter_mut().rev() {
        *slot = b'0' + (n % 10) as u8;
        n /= 10;
    }
}

/// The proleptic Gregorian (year, month, day) of a day count from
/// 1970-01-01 (Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(fill: impl FnOnce(&mut Record<'_>)) -> String {
        let mut out = Vec::new();
        {
            let mut w = RecordWriter::new(&mut out);
            w.record(fill).unwrap();
            w.flush().unwrap();
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn records_are_compact_objects_on_one_line() {
        let line = one(|r| {
            r.str("name", "a\"b\\c\n\u{1}")
                .u64("size", 42)
                .i64("delta", -3)
                .bool("ok", true)
                .f64("ratio", 0.5)
                .f64("nan", f64::NAN);
        });
        assert_eq!(
            line,
            "{\"name\":\"a\\\"b\\\\c\\n\\u0001\",\"size\":42,\"delta\":-3,\"ok\":true,\
             \"ratio\":0.5,\"nan\":null}\n"
        );
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["name"], "a\"b\\c\n\u{1}");
        assert_eq!(one(|_| {}), "{}\n");
    }

    #[test]
    fn non_utf8_bytes_get_a_base64_twin() {
        let line = one(|r| {
            r.bytes("path", b"ok");
        });
        assert_eq!(line, "{\"path\":\"ok\"}\n");
        let line = one(|r| {
            r.bytes("path", b"a\xffb");
        });
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["path"], "a\u{fffd}b");
        assert_eq!(
            STANDARD.decode(v["path_bytes"].as_str().unwrap()).unwrap(),
            b"a\xffb"
        );
    }

    #[test]
    fn times_are_rfc3339_utc_with_nanoseconds_and_an_epoch_twin() {
        let line = one(|r| {
            r.time("mtime", 1_793_296_999, 123_456_789);
        });
        assert_eq!(
            line,
            "{\"mtime\":\"2026-10-29T18:03:19.123456789Z\",\"mtime_ns\":1793296999123456789}\n"
        );
        let mut b = Vec::new();
        push_rfc3339(&mut b, 0, 0);
        assert_eq!(b, b"1970-01-01T00:00:00.000000000Z");
        b.clear();
        push_rfc3339(&mut b, -1, 5);
        assert_eq!(b, b"1969-12-31T23:59:59.000000005Z");
        b.clear();
        push_rfc3339(&mut b, 951_782_400, 0);
        assert_eq!(b, b"2000-02-29T00:00:00.000000000Z");
        b.clear();
        push_rfc3339(&mut b, 253_402_300_800, 0);
        assert_eq!(b, b"+10000-01-01T00:00:00.000000000Z");
        b.clear();
        push_rfc3339(&mut b, -62_167_219_201, 0);
        assert_eq!(b, b"-0001-12-31T23:59:59.000000000Z");
        // Agrees with chrono over a spread of times.
        for sec in (-5_000_000_000i64..5_000_000_000).step_by(98_765_431) {
            b.clear();
            push_rfc3339(&mut b, sec, 7);
            let want = chrono::DateTime::from_timestamp(sec, 7)
                .unwrap()
                .format("%Y-%m-%dT%H:%M:%S%.9fZ")
                .to_string();
            assert_eq!(String::from_utf8_lossy(&b), want);
        }
        let line = one(|r| {
            r.time("t", i64::MIN / 1_000_000_000 - 10, 0);
        });
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(v["t_ns"].is_number());
    }

    #[test]
    fn modes_are_octal_and_symbolic() {
        let line = one(|r| {
            r.mode(FILE_TYPES[0].0 | 0o644);
        });
        assert_eq!(
            line,
            "{\"mode\":\"0644\",\"mode_symbolic\":\"-rw-r--r--\"}\n"
        );
        assert_eq!(&mode_symbolic(FILE_TYPES[1].0 | 0o1777), b"drwxrwxrwt");
        assert_eq!(&mode_symbolic(FILE_TYPES[0].0 | 0o6644), b"-rwSr-Sr--");
        assert_eq!(&mode_symbolic(FILE_TYPES[0].0 | 0o4755), b"-rwsr-xr-x");
        assert_eq!(&mode_symbolic(FILE_TYPES[2].0 | 0o777), b"lrwxrwxrwx");
        assert_eq!(file_type_name(FILE_TYPES[1].0), "directory");
        assert_eq!(file_type_name(FILE_TYPES[6].0), "char_device");
        assert_eq!(file_type_name(0), "unknown");
    }

    #[test]
    fn serialize_and_line_take_objects_only() {
        let mut out = Vec::new();
        {
            let mut w = RecordWriter::new(&mut out);
            w.serialize(&serde_json::json!({"a": 1})).unwrap();
            assert!(w.serialize(&[1, 2]).is_err());
            w.line(br#"{"b":2}"#).unwrap();
            w.error(
                &ErrorRecord::new("No such file or directory")
                    .path(b"x")
                    .io(&io::Error::from_raw_os_error(libc::ENOENT)),
            )
            .unwrap();
        }
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            format!(
                "{{\"a\":1}}\n{{\"b\":2}}\n\
                 {{\"error\":\"No such file or directory\",\"path\":\"x\",\"errno\":{}}}\n",
                libc::ENOENT
            )
        );
    }

    #[test]
    fn records_are_buffered_until_a_block_fills() {
        let mut out = Vec::new();
        let mut w = RecordWriter::new(&mut out);
        w.last_write = Instant::now() + Duration::from_secs(3600);
        w.record(|r| {
            r.u64("n", 1);
        })
        .unwrap();
        assert!(w.get_mut().is_empty());
        while w.get_mut().is_empty() {
            w.record(|r| {
                r.str("pad", &"x".repeat(1000));
            })
            .unwrap();
        }
        drop(w);
        assert!(out.ends_with(b"\"}\n"));
        let mut lb = Vec::new();
        let mut w = RecordWriter::new(&mut lb).line_buffered(true);
        w.record(|r| {
            r.u64("n", 1);
        })
        .unwrap();
        assert_eq!(w.get_mut().as_slice(), b"{\"n\":1}\n");
    }

    #[test]
    fn diagnostics_name_their_file_and_error_number() {
        let q = |raw: &[u8], quoted: &str| (raw.to_vec(), quoted.to_string());
        let quoted = [
            q(b"src", "'src'"),
            q(b"a b", "'a b'"),
            q(b"o", "o"),
            q(b"x\xff", "'x'$'\\377'"),
        ];
        let msg = "cannot move 'a b' to 'src': Permission denied";
        assert_eq!(named_path(msg, &quoted), Some(&b"a b"[..]));
        // A bare (quotef) name is a whole word, not a part of one.
        assert_eq!(named_path("cannot open: x", &quoted[2..3]), None);
        assert_eq!(named_path("o: Is a directory", &quoted), Some(&b"o"[..]));
        assert_eq!(
            named_path("cannot stat 'x'$'\\377': No such file", &quoted),
            Some(&b"x\xff"[..])
        );
        assert_eq!(named_path("missing operand", &quoted), None);
        assert_eq!(errno_of(msg), Some(libc::EACCES));
        assert_eq!(
            errno_of("cannot remove 'd': Is a directory"),
            Some(libc::EISDIR)
        );
        assert_eq!(errno_of("-r not specified; omitting directory 'd'"), None);
        assert_eq!(strip_sgr("\x1b[1mrm\x1b[0m: x"), "rm: x");
    }

    #[test]
    fn queued_error_records_come_before_the_next_record() {
        let mut out = Vec::new();
        {
            let mut w = RecordWriter::new(&mut out);
            w.takes_errors = true;
            w.record(|r| {
                r.u64("n", 1);
            })
            .unwrap();
            PENDING
                .lock()
                .unwrap()
                .extend_from_slice(b"{\"error\":\"e\"}\n");
            HAVE_PENDING.store(true, Ordering::Release);
            w.record(|r| {
                r.u64("n", 2);
            })
            .unwrap();
        }
        assert_eq!(out, b"{\"n\":1}\n{\"error\":\"e\"}\n{\"n\":2}\n");
        assert!(!HAVE_PENDING.load(Ordering::Acquire));
        let mut b = Vec::new();
        {
            let mut w = RecordWriter::new(&mut b);
            w.record(|r| {
                r.mode_as("mode_before", FILE_TYPES[0].0 | 0o600);
            })
            .unwrap();
        }
        assert_eq!(
            b,
            b"{\"mode_before\":\"0600\",\"mode_before_symbolic\":\"-rw-------\"}\n"
        );
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version(None), Ok(1));
        assert_eq!(parse_version(Some(b"1")), Ok(1));
        let err = parse_version(Some(b"2")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "unsupported JSON version '2' (supported: 1)"
        );
    }
}
