use std::ffi::{CStr, CString};
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use clap::{ArgAction, Parser};
use dogfish_core::{
    ColumnAlign, DEFAULT_BUF_SIZE, DogfishContext, Table, gnu_io_error, gnu_quote, gnu_quote_file,
    io::ignore_broken_pipe, parse_duration_str,
};
use serde::Serialize;

// ---------------------------------------------------------------------------
// Shared GNU command-line plumbing
// ---------------------------------------------------------------------------

/// Prints a GNU usage diagnostic (`tool: msg` plus the `--help` hint) and
/// returns `status` for the caller to exit with.
fn usage_error(tool: &str, msg: impl std::fmt::Display, status: i32) -> i32 {
    eprintln!("{tool}: {msg}\nTry '{tool} --help' for more information.");
    status
}

/// Prints the GNU "Try 'tool --help'" hint alone, as GNU does for some
/// operand-count errors, and returns `status`.
fn try_help(tool: &str, status: i32) -> i32 {
    eprintln!("Try '{tool} --help' for more information.");
    status
}

/// Resolves `value` against `valid` like gnulib `argmatch`: an exact match or a
/// unique prefix. On failure prints GNU's diagnostic (with the list of valid
/// arguments) and returns the usage `status` as the error.
fn argmatch(
    tool: &str,
    option: &str,
    value: &str,
    valid: &[&str],
    status: i32,
) -> std::result::Result<usize, i32> {
    if let Some(i) = valid.iter().position(|v| *v == value) {
        return Ok(i);
    }
    let matches: Vec<usize> = (0..valid.len())
        .filter(|&i| !value.is_empty() && valid[i].starts_with(value))
        .collect();
    if let [only] = matches.as_slice() {
        return Ok(*only);
    }
    let kind = if matches.is_empty() {
        "invalid"
    } else {
        "ambiguous"
    };
    let mut msg = format!(
        "{kind} argument {} for '{option}'\nValid arguments are:",
        gnu_quote(value)
    );
    for v in valid {
        msg.push_str(&format!("\n  - '{v}'"));
    }
    Err(usage_error(tool, msg, status))
}

/// Whether an option takes an argument, as in `getopt_long`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HasArg {
    No,
    Required,
    /// Only attached: `-xVALUE` or `--name=VALUE`.
    Optional,
}

/// One option a [`GetOpt`] parser accepts; `key` identifies it to the caller.
struct OptSpec {
    short: Option<char>,
    long: Option<&'static str>,
    arg: HasArg,
    key: &'static str,
}

const fn opt(
    short: Option<char>,
    long: Option<&'static str>,
    arg: HasArg,
    key: &'static str,
) -> OptSpec {
    OptSpec {
        short,
        long,
        arg,
        key,
    }
}

/// A small `getopt_long` work-alike for the tools whose GNU parsing clap cannot
/// express: options that stop at the first operand (`env`, `timeout`, `seq`),
/// argument vectors rewritten mid-parse (`env -S`), and option arguments that
/// are only ever attached (`date -I`). Diagnostics use glibc's exact wording.
struct GetOpt {
    args: Vec<String>,
    idx: usize,
    /// Byte offset of the next short option inside `args[idx]`, or 0.
    cluster: usize,
    /// Accept options after operands (GNU's default permuting mode).
    permute: bool,
    operands: Vec<String>,
    done: bool,
    specs: &'static [OptSpec],
}

type OptResult = std::result::Result<(&'static str, Option<String>), String>;

impl GetOpt {
    /// `args` excludes the program name.
    fn new(args: Vec<String>, specs: &'static [OptSpec], permute: bool) -> Self {
        Self {
            args,
            idx: 0,
            cluster: 0,
            permute,
            operands: Vec::new(),
            done: false,
            specs,
        }
    }

    /// The argument the parser would examine next, when not inside a cluster.
    fn peek(&self) -> Option<&str> {
        if self.done || self.cluster != 0 {
            return None;
        }
        self.args.get(self.idx).map(String::as_str)
    }

    /// Replaces the arguments consumed so far with `items` and resumes parsing
    /// at the first of them (how GNU `env -S` re-injects its split string).
    fn splice_front(&mut self, items: Vec<String>) {
        let rest = self.args.split_off(self.idx);
        self.args = items;
        self.args.extend(rest);
        self.idx = 0;
        self.cluster = 0;
    }

    /// Operands collected so far plus every argument not yet parsed.
    fn into_operands(mut self) -> Vec<String> {
        let rest = self.args.split_off(self.idx.min(self.args.len()));
        self.operands.extend(rest);
        self.operands
    }

    fn next_opt(&mut self) -> Option<OptResult> {
        if self.cluster != 0 {
            return Some(self.short_opt());
        }
        loop {
            if self.done {
                return None;
            }
            let arg = self.args.get(self.idx)?;
            if arg == "--" {
                self.idx += 1;
                self.done = true;
                return None;
            }
            if arg.len() > 2 && arg.starts_with("--") {
                return Some(self.long_opt());
            }
            if arg.len() > 1 && arg.starts_with('-') {
                self.cluster = 1;
                return Some(self.short_opt());
            }
            if !self.permute {
                self.done = true;
                return None;
            }
            self.operands.push(arg.clone());
            self.idx += 1;
        }
    }

    fn long_opt(&mut self) -> OptResult {
        let arg = self.args[self.idx].clone();
        self.idx += 1;
        let body = &arg[2..];
        let (name, attached) = match body.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (body, None),
        };
        let spec = match self.specs.iter().find(|s| s.long == Some(name)) {
            Some(s) => s,
            None => {
                let candidates: Vec<&OptSpec> = self
                    .specs
                    .iter()
                    .filter(|s| s.long.is_some_and(|l| l.starts_with(name)))
                    .collect();
                match candidates.as_slice() {
                    [] => return Err(format!("unrecognized option '{arg}'")),
                    [first, rest @ ..]
                        if rest
                            .iter()
                            .all(|s| s.key == first.key && s.arg == first.arg) =>
                    {
                        *first
                    }
                    many => {
                        let names: Vec<String> = many
                            .iter()
                            .map(|s| format!("'--{}'", s.long.unwrap_or_default()))
                            .collect();
                        return Err(format!(
                            "option '--{name}' is ambiguous; possibilities: {}",
                            names.join(" ")
                        ));
                    }
                }
            }
        };
        let full = spec.long.unwrap_or_default();
        match (spec.arg, attached) {
            (HasArg::No, Some(_)) => Err(format!("option '--{full}' doesn't allow an argument")),
            (HasArg::No, None) | (HasArg::Optional, None) => Ok((spec.key, None)),
            (_, Some(v)) => Ok((spec.key, Some(v))),
            (HasArg::Required, None) => match self.args.get(self.idx) {
                Some(v) => {
                    let v = v.clone();
                    self.idx += 1;
                    Ok((spec.key, Some(v)))
                }
                None => Err(format!("option '--{full}' requires an argument")),
            },
        }
    }

    fn short_opt(&mut self) -> OptResult {
        let arg = self.args[self.idx].clone();
        let c = arg[self.cluster..].chars().next().unwrap_or('?');
        self.cluster += c.len_utf8();
        let rest = arg[self.cluster..].to_string();
        let at_end = rest.is_empty();
        let finish_cluster = |me: &mut Self| {
            me.cluster = 0;
            me.idx += 1;
        };
        let Some(spec) = self.specs.iter().find(|s| s.short == Some(c)) else {
            if at_end {
                finish_cluster(self);
            }
            return Err(format!("invalid option -- '{c}'"));
        };
        match spec.arg {
            HasArg::No => {
                if at_end {
                    finish_cluster(self);
                }
                Ok((spec.key, None))
            }
            HasArg::Optional => {
                finish_cluster(self);
                Ok((spec.key, (!at_end).then_some(rest)))
            }
            HasArg::Required => {
                finish_cluster(self);
                if !at_end {
                    return Ok((spec.key, Some(rest)));
                }
                match self.args.get(self.idx) {
                    Some(v) => {
                        let v = v.clone();
                        self.idx += 1;
                        Ok((spec.key, Some(v)))
                    }
                    None => Err(format!("option requires an argument -- '{c}'")),
                }
            }
        }
    }
}

/// Parses the longest prefix of `s` that C `strtod` accepts: leading white
/// space, a sign, decimal or hexadecimal digits with an optional fraction and
/// exponent, or `inf`/`infinity`/`nan`. Returns the value and the number of
/// bytes consumed, or `None` when no number starts `s`.
fn strtod_prefix(s: &str) -> Option<(f64, usize)> {
    let b = s.as_bytes();
    let mut i = b
        .iter()
        .take_while(|c| matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
        .count();
    let negative = match b.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let signed = |v: f64| if negative { -v } else { v };
    let rest = &s[i..];
    let starts_with_ci = |p: &str| {
        rest.len() >= p.len() && rest.as_bytes()[..p.len()].eq_ignore_ascii_case(p.as_bytes())
    };
    if starts_with_ci("infinity") {
        return Some((signed(f64::INFINITY), i + 8));
    }
    if starts_with_ci("inf") {
        return Some((signed(f64::INFINITY), i + 3));
    }
    if starts_with_ci("nan") {
        let mut end = i + 3;
        if b.get(end) == Some(&b'(')
            && let Some(close) = b[end + 1..]
                .iter()
                .position(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))
            && b[end + 1 + close] == b')'
        {
            end += close + 2;
        }
        return Some((f64::NAN, end));
    }
    let hex_digit_at = |j: usize| b.get(j).is_some_and(u8::is_ascii_hexdigit);
    if b.get(i) == Some(&b'0')
        && matches!(b.get(i + 1), Some(b'x' | b'X'))
        && (hex_digit_at(i + 2) || (b.get(i + 2) == Some(&b'.') && hex_digit_at(i + 3)))
    {
        let mut j = i + 2;
        let mut mantissa = 0f64;
        let mut exp = 0i64;
        while hex_digit_at(j) {
            mantissa = mantissa * 16.0 + f64::from((b[j] as char).to_digit(16).unwrap_or(0));
            j += 1;
        }
        if b.get(j) == Some(&b'.') {
            j += 1;
            while hex_digit_at(j) {
                mantissa = mantissa * 16.0 + f64::from((b[j] as char).to_digit(16).unwrap_or(0));
                exp -= 4;
                j += 1;
            }
        }
        if matches!(b.get(j), Some(b'p' | b'P')) {
            let mut k = j + 1;
            let exp_negative = match b.get(k) {
                Some(b'-') => {
                    k += 1;
                    true
                }
                Some(b'+') => {
                    k += 1;
                    false
                }
                _ => false,
            };
            let digits = b[k..].iter().take_while(|c| c.is_ascii_digit()).count();
            if digits > 0 {
                let e: i64 = s[k..k + digits].parse().unwrap_or(i64::MAX / 2);
                exp += if exp_negative { -e } else { e };
                j = k + digits;
            }
        }
        let value = mantissa * 2f64.powi(exp.clamp(-2200, 2200) as i32);
        return Some((signed(value), j));
    }
    let int_digits = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
    let mut j = i + int_digits;
    let mut frac_digits = 0;
    if b.get(j) == Some(&b'.') {
        frac_digits = b[j + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
        if int_digits + frac_digits > 0 {
            j += 1 + frac_digits;
        }
    }
    if int_digits + frac_digits == 0 {
        return None;
    }
    if matches!(b.get(j), Some(b'e' | b'E')) {
        let mut k = j + 1;
        if matches!(b.get(k), Some(b'-' | b'+')) {
            k += 1;
        }
        let digits = b[k..].iter().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 {
            j = k + digits;
        }
    }
    let value: f64 = s[i..j].parse().ok()?;
    Some((signed(value), j))
}

/// Parses `s` as a whole C floating-point number (see [`strtod_prefix`]).
fn strtod_full(s: &str) -> Option<f64> {
    strtod_prefix(s).and_then(|(v, n)| (n == s.len()).then_some(v))
}

/// Parses a GNU `sleep`/`timeout` interval: a floating-point number with an
/// optional `s`, `m`, `h` or `d` suffix. As an extension, compound human
/// durations (`500ms`, `1m30s`) are accepted too.
fn parse_interval(s: &str) -> Option<f64> {
    if let Some((value, used)) = strtod_prefix(s) {
        let scale = match &s[used..] {
            "" | "s" => Some(1.0),
            "m" => Some(60.0),
            "h" => Some(3600.0),
            "d" => Some(86400.0),
            _ => None,
        };
        if let Some(scale) = scale {
            // `!(0 <= v)` also rejects NaN, like GNU.
            return (value >= 0.0).then_some(value * scale);
        }
    }
    let human = !s.is_empty()
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'.' || !c.is_ascii());
    human
        .then(|| parse_duration_str(s).ok())
        .flatten()
        .map(|d| d.as_secs_f64())
}

fn duration_from_secs(secs: f64) -> Duration {
    Duration::try_from_secs_f64(secs).unwrap_or(Duration::MAX)
}

fn print_help(text: &str) -> Result<i32> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    ignore_broken_pipe(out.write_all(text.as_bytes()))?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// 1. true & false
// ---------------------------------------------------------------------------
const TRUE_HELP: &str = "\
Usage: true [ignored command line arguments]
  or:  true OPTION
Exit with a status code indicating success.

      --help        display this help and exit
      --version     output version information and exit
";

const FALSE_HELP: &str = "\
Usage: false [ignored command line arguments]
  or:  false OPTION
Exit with a status code indicating failure.

      --help        display this help and exit
      --version     output version information and exit
";

/// GNU `true`/`false` ignore every argument except a lone `--help`, which
/// still exits with the tool's own status.
fn run_trivial(args: &[String], status: i32, help: &str) -> Result<i32> {
    if args.len() == 2 && args[1] == "--help" {
        print_help(help)?;
    }
    Ok(status)
}

pub fn run_true(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    ctx.telemetry.set_strategy("immediate exit(0)");
    run_trivial(&args, 0, TRUE_HELP)
}

pub fn run_false(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    ctx.telemetry.set_strategy("immediate exit(1)");
    run_trivial(&args, 1, FALSE_HELP)
}

// ---------------------------------------------------------------------------
// 2. yes — Page-aligned 64 KiB vector I/O
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "yes",
    about = "Repeatedly output a line with all specified STRING(s), or 'y'",
    disable_help_flag = true,
    infer_long_args = true
)]
struct YesArgs {
    /// Display this help and exit
    #[arg(long, action = ArgAction::Help)]
    help: Option<bool>,

    /// String(s) to repeat
    strings: Vec<String>,
}

pub fn run_yes(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = YesArgs::try_parse_from(args)?;
    let line = if parsed.strings.is_empty() {
        "y\n".to_string()
    } else {
        format!("{}\n", parsed.strings.join(" "))
    };

    ctx.telemetry.set_strategy("64KiB page-aligned buffer");
    let line_bytes = line.as_bytes();
    let mut buf = Vec::with_capacity(DEFAULT_BUF_SIZE + line_bytes.len());
    while buf.len() < DEFAULT_BUF_SIZE {
        buf.extend_from_slice(line_bytes);
    }

    let stdout = io::stdout();
    let mut handle = stdout.lock();
    loop {
        match handle.write_all(&buf) {
            Ok(()) => ctx.telemetry.add_written(buf.len() as u64),
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// 3. echo
// ---------------------------------------------------------------------------
const ECHO_HELP: &str = "\
Usage: echo [SHORT-OPTION]... [STRING]...
  or:  echo LONG-OPTION
Echo the STRING(s) to standard output.

  -n             do not output the trailing newline
  -e             enable interpretation of backslash escapes
  -E             disable interpretation of backslash escapes (default)
      --help        display this help and exit
      --version     output version information and exit

If -e is in effect, the following sequences are recognized:

  \\\\      backslash
  \\a      alert (BEL)
  \\b      backspace
  \\c      produce no further output
  \\e      escape
  \\f      form feed
  \\n      new line
  \\r      carriage return
  \\t      horizontal tab
  \\v      vertical tab
  \\0NNN   byte with octal value NNN (1 to 3 digits)
  \\xHH    byte with hexadecimal value HH (1 to 2 digits)
";

pub fn run_echo(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    // Like GNU (without POSIXLY_CORRECT), only a lone `--help` is an option.
    if args.len() == 2 && args[1] == "--help" {
        return print_help(ECHO_HELP);
    }
    let mut no_newline = false;
    let mut interpret_escapes = false;
    let mut idx = 1;

    while idx < args.len() {
        let arg = &args[idx];
        if arg.starts_with('-')
            && arg.len() > 1
            && arg[1..].chars().all(|c| matches!(c, 'n' | 'e' | 'E'))
        {
            for ch in arg[1..].chars() {
                match ch {
                    'n' => no_newline = true,
                    'e' => interpret_escapes = true,
                    'E' => interpret_escapes = false,
                    _ => {}
                }
            }
            idx += 1;
        } else {
            break;
        }
    }

    let payload = args[idx..].join(" ");
    let stdout = io::stdout();
    let mut out = stdout.lock();

    let (rendered, stop) = if interpret_escapes {
        unescape_echo_bytes(payload.as_bytes(), false)
    } else {
        (payload.into_bytes(), false)
    };
    ignore_broken_pipe(out.write_all(&rendered))?;
    ctx.telemetry.add_written(rendered.len() as u64);
    if !no_newline && !stop {
        ignore_broken_pipe(out.write_all(b"\n"))?;
        ctx.telemetry.add_written(1);
    }
    Ok(0)
}

/// Expands echo-style escapes (also used by `printf %b`). Returns the bytes and
/// whether `\c` requested that all further output stop.
///
/// `\0NNN` is octal; with `bare_octal` (printf `%b`), `\NNN` is octal too.
pub fn unescape_echo_bytes(s: &[u8], bare_octal: bool) -> (Vec<u8>, bool) {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let ch = s[i];
        i += 1;
        if ch != b'\\' || i >= s.len() {
            out.push(ch);
            continue;
        }
        let e = s[i];
        i += 1;
        match e {
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'c' => return (out, true),
            b'e' | b'E' => out.push(0x1b),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0b),
            b'\\' => out.push(b'\\'),
            b'x' if i < s.len() && s[i].is_ascii_hexdigit() => {
                let mut v = 0u32;
                let mut n = 0;
                while n < 2 && i < s.len() && s[i].is_ascii_hexdigit() {
                    v = v * 16 + (s[i] as char).to_digit(16).unwrap();
                    i += 1;
                    n += 1;
                }
                out.push(v as u8);
            }
            b'0'..=b'7' if e == b'0' || bare_octal => {
                let mut v = if e == b'0' { 0u32 } else { (e - b'0') as u32 };
                let max = if e == b'0' { 3 } else { 2 };
                let mut n = 0;
                while n < max && i < s.len() && (b'0'..=b'7').contains(&s[i]) {
                    v = v * 8 + (s[i] - b'0') as u32;
                    i += 1;
                    n += 1;
                }
                out.push(v as u8);
            }
            other => {
                out.push(b'\\');
                out.push(other);
            }
        }
    }
    (out, false)
}

// ---------------------------------------------------------------------------
// 4. printf — GNU-compatible format engine
// ---------------------------------------------------------------------------
#[derive(Default, Clone, Copy)]
struct PrintfFlags {
    left: bool,
    zero: bool,
    plus: bool,
    space: bool,
    alt: bool,
}

/// State shared while rendering: diagnostics flip the exit status to 1.
struct PrintfRun<'a> {
    args: &'a [String],
    next: usize,
    failed: bool,
}

impl<'a> PrintfRun<'a> {
    fn take(&mut self) -> Option<&'a str> {
        let v = self.args.get(self.next).map(|s| s.as_str());
        if v.is_some() {
            self.next += 1;
        }
        v
    }

    /// Parses an integer argument like GNU (`'c` char codes, 0x hex, 0 octal),
    /// clamping to the range of `intmax_t` (`signed`) or `uintmax_t`.
    fn int_arg(&mut self, s: Option<&str>, signed: bool) -> i128 {
        let Some(raw) = s else { return 0 };
        let t = raw.trim_start();
        let b = t.as_bytes();
        if let [b'\'' | b'"', rest @ ..] = b {
            let code = std::str::from_utf8(rest)
                .ok()
                .and_then(|r| r.chars().next())
                .map(|c| c as i128)
                .or_else(|| rest.first().map(|&c| i128::from(c)));
            if code.is_none() {
                self.complain(raw, "expected a numeric value");
            }
            return code.unwrap_or(0);
        }
        let (neg, body) = match b.first() {
            Some(b'-') => (true, &t[1..]),
            Some(b'+') => (false, &t[1..]),
            _ => (false, t),
        };
        let (radix, digits) =
            if let Some(h) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
                (16, h)
            } else if body.len() > 1 && body.starts_with('0') {
                (8, &body[1..])
            } else {
                (10, body)
            };
        let valid = digits
            .bytes()
            .take_while(|c| (*c as char).is_digit(radix))
            .count();
        // Overflowing digits saturate (and are reported as out of range below).
        let value = match valid {
            0 => 0,
            n => i128::from_str_radix(&digits[..n], radix).unwrap_or(i128::MAX),
        };
        let value = if neg { -value } else { value };
        if valid == 0 && !(radix == 8 && digits.is_empty()) {
            self.complain(raw, "expected a numeric value");
            return value;
        }
        let (min, max) = if signed {
            (i128::from(i64::MIN), i128::from(i64::MAX))
        } else {
            (-i128::from(u64::MAX), i128::from(u64::MAX))
        };
        if valid < digits.len() {
            self.complain(raw, "value not completely converted");
        } else if !(min..=max).contains(&value) {
            self.complain(raw, "Numerical result out of range");
        }
        value.clamp(min, max)
    }

    fn float_arg(&mut self, s: Option<&str>) -> f64 {
        let Some(raw) = s else { return 0.0 };
        if let [b'\'' | b'"', rest @ ..] = raw.trim_start().as_bytes() {
            return rest.first().copied().unwrap_or(0) as f64;
        }
        match strtod_prefix(raw) {
            Some((v, used)) if used == raw.len() => v,
            Some((v, _)) => {
                self.complain(raw, "value not completely converted");
                v
            }
            None => {
                self.complain(raw, "expected a numeric value");
                0.0
            }
        }
    }

    fn complain(&mut self, raw: &str, what: &str) {
        eprintln!("printf: {}: {what}", gnu_quote(raw));
        self.failed = true;
    }
}

fn pad_field(body: Vec<u8>, width: usize, flags: PrintfFlags, zero_ok: bool) -> Vec<u8> {
    if body.len() >= width {
        return body;
    }
    let pad = width - body.len();
    if flags.left {
        let mut v = body;
        v.extend(std::iter::repeat_n(b' ', pad));
        v
    } else if flags.zero && zero_ok {
        // Zeros go after any sign / 0x prefix.
        let sign_len = match body.as_slice() {
            [b'-' | b'+' | b' ', b'0', b'x' | b'X', ..] => 3,
            [b'0', b'x' | b'X', ..] => 2,
            [b'-' | b'+' | b' ', ..] => 1,
            _ => 0,
        };
        let mut v = body[..sign_len].to_vec();
        v.extend(std::iter::repeat_n(b'0', pad));
        v.extend_from_slice(&body[sign_len..]);
        v
    } else {
        let mut v = vec![b' '; pad];
        v.extend(body);
        v
    }
}

/// C-style `%e` rendering of a non-negative finite value.
fn c_exp(v: f64, prec: usize, upper: bool) -> String {
    let s = format!("{:.prec$e}", v);
    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let e = if upper { 'E' } else { 'e' };
    format!(
        "{}{}{}{:02}",
        mant,
        e,
        if exp < 0 { '-' } else { '+' },
        exp.abs()
    )
}

fn strip_frac_zeros(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn format_float(v: f64, conv: u8, prec: Option<usize>, flags: PrintfFlags) -> (String, bool) {
    let upper = conv.is_ascii_uppercase();
    let sign = if v.is_sign_negative() && !v.is_nan() {
        "-"
    } else if flags.plus {
        "+"
    } else if flags.space {
        " "
    } else {
        ""
    };
    let a = v.abs();
    if !a.is_finite() {
        let s = if a.is_nan() { "nan" } else { "inf" };
        let s = if upper {
            s.to_uppercase()
        } else {
            s.to_string()
        };
        return (format!("{}{}", sign, s), false);
    }
    let prec = prec.unwrap_or(6);
    let body = match conv.to_ascii_lowercase() {
        b'f' => {
            let mut s = format!("{:.prec$}", a);
            if flags.alt && prec == 0 {
                s.push('.');
            }
            s
        }
        b'e' => {
            let mut s = c_exp(a, prec, upper);
            if flags.alt && prec == 0 {
                let idx = s.find(['e', 'E']).unwrap_or(s.len());
                s.insert(idx, '.');
            }
            s
        }
        _ => {
            // %g
            let p = if prec == 0 { 1 } else { prec };
            let x: i32 = if a == 0.0 {
                0
            } else {
                let e = c_exp(a, p - 1, false);
                e[e.find('e').unwrap() + 1..].parse().unwrap_or(0)
            };
            if (x as i64) < p as i64 && x >= -4 {
                let s = format!("{:.*}", (p as i64 - 1 - x as i64).max(0) as usize, a);
                if flags.alt { s } else { strip_frac_zeros(&s) }
            } else {
                let s = c_exp(a, p - 1, upper);
                if flags.alt {
                    s
                } else {
                    let idx = s.find(['e', 'E']).unwrap();
                    format!("{}{}", strip_frac_zeros(&s[..idx]), &s[idx..])
                }
            }
        }
    };
    (format!("{}{}", sign, body), true)
}

fn format_int(v: i128, conv: u8, prec: Option<usize>, flags: PrintfFlags) -> String {
    let (neg, mag): (bool, u128) = match conv {
        b'd' | b'i' => (v < 0, v.unsigned_abs()),
        // Unsigned conversions see the 64-bit two's complement value, as in C.
        _ => (false, (v as i64 as u64) as u128),
    };
    let mut digits = match conv {
        b'o' => format!("{:o}", mag),
        b'x' => format!("{:x}", mag),
        b'X' => format!("{:X}", mag),
        _ => mag.to_string(),
    };
    if let Some(p) = prec {
        if p == 0 && mag == 0 {
            digits.clear();
        }
        while digits.len() < p {
            digits.insert(0, '0');
        }
    }
    let mut prefix = String::new();
    if neg {
        prefix.push('-');
    } else if matches!(conv, b'd' | b'i') && flags.plus {
        prefix.push('+');
    } else if matches!(conv, b'd' | b'i') && flags.space {
        prefix.push(' ');
    }
    if flags.alt {
        match conv {
            b'o' if !digits.starts_with('0') => digits.insert(0, '0'),
            b'x' if mag != 0 => prefix.push_str("0x"),
            b'X' if mag != 0 => prefix.push_str("0X"),
            _ => {}
        }
    }
    format!("{}{}", prefix, digits)
}

/// Renders the format once. Returns `false` if `\c` stopped all output.
fn printf_once(
    fmt: &[u8],
    run: &mut PrintfRun<'_>,
    out: &mut Vec<u8>,
) -> std::result::Result<bool, ()> {
    let mut i = 0;
    while i < fmt.len() {
        let ch = fmt[i];
        if ch == b'\\' {
            i += 1;
            let Some(&e) = fmt.get(i) else {
                out.push(b'\\');
                break;
            };
            i += 1;
            match e {
                b'"' => out.push(b'"'),
                b'\\' => out.push(b'\\'),
                b'a' => out.push(0x07),
                b'b' => out.push(0x08),
                b'c' => return Ok(false),
                b'e' => out.push(0x1b),
                b'f' => out.push(0x0c),
                b'n' => out.push(b'\n'),
                b'r' => out.push(b'\r'),
                b't' => out.push(b'\t'),
                b'v' => out.push(0x0b),
                b'0'..=b'7' => {
                    let mut v = (e - b'0') as u32;
                    let mut n = 1;
                    while n < 3 && i < fmt.len() && (b'0'..=b'7').contains(&fmt[i]) {
                        v = v * 8 + (fmt[i] - b'0') as u32;
                        i += 1;
                        n += 1;
                    }
                    out.push(v as u8);
                }
                b'x' | b'u' | b'U' => {
                    let max = match e {
                        b'x' => 2,
                        b'u' => 4,
                        _ => 8,
                    };
                    let st = i;
                    while i < fmt.len() && i - st < max && fmt[i].is_ascii_hexdigit() {
                        i += 1;
                    }
                    if i == st {
                        eprintln!("printf: missing hexadecimal number in escape");
                        return Err(());
                    }
                    let v = u32::from_str_radix(std::str::from_utf8(&fmt[st..i]).unwrap(), 16)
                        .unwrap_or(0);
                    if e == b'x' {
                        out.push(v as u8);
                    } else {
                        let c = char::from_u32(v).unwrap_or('\u{fffd}');
                        out.extend_from_slice(c.to_string().as_bytes());
                    }
                }
                other => {
                    out.push(b'\\');
                    out.push(other);
                }
            }
            continue;
        }
        if ch != b'%' {
            out.push(ch);
            i += 1;
            continue;
        }
        let spec_start = i;
        i += 1;
        if fmt.get(i) == Some(&b'%') {
            out.push(b'%');
            i += 1;
            continue;
        }
        let mut flags = PrintfFlags::default();
        while let Some(&f) = fmt.get(i) {
            match f {
                b'-' => flags.left = true,
                b'0' => flags.zero = true,
                b'+' => flags.plus = true,
                b' ' => flags.space = true,
                b'#' => flags.alt = true,
                _ => break,
            }
            i += 1;
        }
        let mut width = 0usize;
        if fmt.get(i) == Some(&b'*') {
            i += 1;
            let w = run.take();
            let w = run.int_arg(w, true);
            if w < 0 {
                flags.left = true;
            }
            width = w.unsigned_abs() as usize;
        } else {
            while let Some(d @ b'0'..=b'9') = fmt.get(i).copied() {
                width = width * 10 + (d - b'0') as usize;
                i += 1;
            }
        }
        let mut prec: Option<usize> = None;
        if fmt.get(i) == Some(&b'.') {
            i += 1;
            if fmt.get(i) == Some(&b'*') {
                i += 1;
                let p = run.take();
                let p = run.int_arg(p, true);
                prec = (p >= 0).then_some(p as usize);
            } else {
                let mut p = 0usize;
                while let Some(d @ b'0'..=b'9') = fmt.get(i).copied() {
                    p = p * 10 + (d - b'0') as usize;
                    i += 1;
                }
                prec = Some(p);
            }
        }
        // Length modifiers are accepted and ignored.
        while matches!(fmt.get(i), Some(b'h' | b'l' | b'L' | b'j' | b'z' | b't')) {
            i += 1;
        }
        let Some(&conv) = fmt.get(i) else {
            eprintln!(
                "printf: {}: invalid conversion specification",
                String::from_utf8_lossy(&fmt[spec_start..])
            );
            return Err(());
        };
        i += 1;
        let field: Vec<u8> = match conv {
            b'd' | b'i' | b'o' | b'u' | b'x' | b'X' => {
                let a = run.take();
                let v = run.int_arg(a, matches!(conv, b'd' | b'i'));
                let s = format_int(v, conv, prec, flags);
                pad_field(s.into_bytes(), width, flags, prec.is_none())
            }
            b'f' | b'F' | b'e' | b'E' | b'g' | b'G' => {
                let a = run.take();
                let v = run.float_arg(a);
                let (s, finite) = format_float(v, conv, prec, flags);
                pad_field(s.into_bytes(), width, flags, finite)
            }
            b's' => {
                let a = run.take().unwrap_or("").as_bytes();
                let a = match prec {
                    Some(p) => &a[..p.min(a.len())],
                    None => a,
                };
                pad_field(a.to_vec(), width, flags, false)
            }
            b'b' => {
                let a = run.take().unwrap_or("");
                let (bytes, stop) = unescape_echo_bytes(a.as_bytes(), true);
                let bytes = match prec {
                    Some(p) => bytes[..p.min(bytes.len())].to_vec(),
                    None => bytes,
                };
                out.extend(pad_field(bytes, width, flags, false));
                if stop {
                    return Ok(false);
                }
                continue;
            }
            b'q' => {
                let a = run.take().unwrap_or("");
                pad_field(gnu_quote_file(a).as_bytes().to_vec(), width, flags, false)
            }
            b'c' => {
                let a = run.take().unwrap_or("");
                let c = a.as_bytes().first().copied().unwrap_or(0);
                pad_field(vec![c], width, flags, false)
            }
            _ => {
                eprintln!(
                    "printf: {}: invalid conversion specification",
                    String::from_utf8_lossy(&fmt[spec_start..i])
                );
                return Err(());
            }
        };
        out.extend(field);
    }
    Ok(true)
}

const PRINTF_HELP: &str = r#"Usage: printf FORMAT [ARGUMENT]...
  or:  printf OPTION
Print ARGUMENT(s) according to FORMAT, or execute according to OPTION:

      --help        display this help and exit
      --version     output version information and exit

FORMAT controls the output as in C printf.  Interpreted sequences are:

  \"      double quote
  \\      backslash
  \a      alert (BEL)
  \b      backspace
  \c      produce no further output
  \e      escape
  \f      form feed
  \n      new line
  \r      carriage return
  \t      horizontal tab
  \v      vertical tab
  \NNN    byte with octal value NNN (1 to 3 digits)
  \xHH    byte with hexadecimal value HH (1 to 2 digits)
  \uHHHH  Unicode (ISO/IEC 10646) character with hex value HHHH (4 digits)
  \UHHHHHHHH  Unicode character with hex value HHHHHHHH (8 digits)
  %%      a single %
  %b      ARGUMENT as a string with '\' escapes interpreted,
          except that octal escapes are of the form \0 or \0NNN

and all C format specifications ending with one of diouxXfeEgGcs, with
ARGUMENTs converted to proper type first.  Variable widths are handled.
"#;

pub fn run_printf(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    if args.len() == 2 && args[1] == "--help" {
        return print_help(PRINTF_HELP);
    }
    // A leading "--" ends (the absent) options, as with getopt.
    let first = if args.get(1).is_some_and(|a| a == "--") {
        2
    } else {
        1
    };
    if args.len() <= first {
        eprintln!("printf: missing operand\nTry 'printf --help' for more information.");
        return Ok(1);
    }
    let fmt = args[first].as_bytes();
    let mut run = PrintfRun {
        args: &args[first + 1..],
        next: 0,
        failed: false,
    };
    let mut output = Vec::new();
    let mut ok = true;
    loop {
        let before = run.next;
        match printf_once(fmt, &mut run, &mut output) {
            Ok(true) => {}
            Ok(false) => break,
            Err(()) => {
                ok = false;
                break;
            }
        }
        if run.next >= run.args.len() {
            break;
        }
        if run.next == before {
            eprintln!(
                "printf: warning: ignoring excess arguments, starting with {}",
                gnu_quote(&run.args[run.next])
            );
            break;
        }
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    ignore_broken_pipe(out.write_all(&output))?;
    ctx.telemetry.add_written(output.len() as u64);
    Ok(if ok && !run.failed { 0 } else { 1 })
}

// ---------------------------------------------------------------------------
// 5. sleep — GNU intervals (`1.5`, `2m`, `1d`), plus human durations (`500ms`)
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "sleep",
    about = "Pause for NUMBER seconds, where NUMBER may have a suffix of 's' (seconds), \
             'm' (minutes), 'h' (hours) or 'd' (days); several arguments are summed. \
             Compound durations such as 500ms or 1m30s are accepted too.",
    disable_help_flag = true,
    infer_long_args = true
)]
struct SleepArgs {
    /// Display this help and exit
    #[arg(long, action = ArgAction::Help)]
    help: Option<bool>,

    #[arg(required = true, value_name = "NUMBER")]
    durations: Vec<String>,
}

pub fn run_sleep(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = SleepArgs::try_parse_from(args)?;
    let mut total = 0f64;
    let mut ok = true;
    for d in &parsed.durations {
        match parse_interval(d) {
            Some(secs) => total += secs,
            None => {
                eprintln!("sleep: invalid time interval {}", gnu_quote(d));
                ok = false;
            }
        }
    }
    if !ok {
        return Ok(try_help("sleep", 1));
    }
    let total = duration_from_secs(total);
    ctx.telemetry.set_strategy(format!("sleep {:?}", total));
    thread::sleep(total);
    Ok(0)
}

// ---------------------------------------------------------------------------
// 6. seq — GNU number scanning, widths, formats and arbitrary-size integers
// ---------------------------------------------------------------------------
const SEQ_HELP: &str = "\
Usage: seq [OPTION]... LAST
  or:  seq [OPTION]... FIRST LAST
  or:  seq [OPTION]... FIRST INCREMENT LAST
Print numbers from FIRST to LAST, in steps of INCREMENT.

  -f, --format=FORMAT      use printf style floating-point FORMAT
  -s, --separator=STRING   use STRING to separate numbers (default: \\n)
  -w, --equal-width        equalize width by padding with leading zeroes
  -J, --json               print the numbers as a JSON array
      --help        display this help and exit
      --version     output version information and exit

If FIRST or INCREMENT is omitted, it defaults to 1.  That is, an
omitted INCREMENT defaults to 1 even when LAST is smaller than FIRST.
The sequence of numbers ends when the sum of the current number and
INCREMENT would become greater than LAST.
FIRST, INCREMENT, and LAST are interpreted as floating point values.
INCREMENT is usually positive if FIRST is smaller than LAST, and
INCREMENT is usually negative if FIRST is greater than LAST.
INCREMENT must not be 0; none of FIRST, INCREMENT and LAST may be NaN.
FORMAT must be suitable for printing one argument of type 'double';
it defaults to %.PRECf if FIRST, INCREMENT, and LAST are all fixed point
decimal numbers with maximum precision PREC, and to %g otherwise.
";

static SEQ_OPTS: [OptSpec; 5] = [
    opt(Some('f'), Some("format"), HasArg::Required, "format"),
    opt(Some('s'), Some("separator"), HasArg::Required, "separator"),
    opt(Some('w'), Some("equal-width"), HasArg::No, "equal-width"),
    opt(Some('J'), Some("json"), HasArg::No, "json"),
    opt(None, Some("help"), HasArg::No, "help"),
];

/// A scanned `seq` operand: its value and the output width and precision
/// GNU derives from how it was written.
#[derive(Clone, Copy)]
struct SeqOperand {
    value: f64,
    width: usize,
    /// Fraction digits; `None` means "unknown" (hex or non-finite input).
    precision: Option<usize>,
    /// The exact value when it is an integer that fits `i64` (not `-0`).
    int: Option<i64>,
}

fn scan_seq_operand(arg: &str) -> std::result::Result<SeqOperand, String> {
    let Some(value) = strtod_full(arg) else {
        return Err(format!(
            "invalid floating point argument: {}",
            gnu_quote(arg)
        ));
    };
    if value.is_nan() {
        return Err(format!(
            "invalid 'not-a-number' argument: {}",
            gnu_quote(arg)
        ));
    }
    // Spaces and '+' are not output, so they don't count towards the width.
    let s = arg.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '+');
    let decimal_point = s.find('.');
    let mut precision: Option<i64> =
        (decimal_point.is_none() && !s.contains(['p', 'P'])).then_some(0);
    let mut width: i64 = 0;
    if !s.contains(['x', 'X']) && value.is_finite() {
        width = s.len() as i64;
        let mut fraction_len = 0i64;
        if let Some(dp) = decimal_point {
            fraction_len = s[dp + 1..].find(['e', 'E']).unwrap_or(s.len() - dp - 1) as i64;
            precision = Some(fraction_len);
            let before = s[..dp].bytes().last();
            width += if fraction_len == 0 {
                -1 // "#." prints as "#"
            } else if !before.is_some_and(|c| c.is_ascii_digit()) {
                1 // ".#" and "-.#" print as "0.#"
            } else {
                0
            };
        }
        if let Some(e) = s.find(['e', 'E']) {
            let exponent: i64 = {
                let tail = &s[e + 1..];
                let digits_end = tail
                    .char_indices()
                    .find(|&(i, c)| !(c.is_ascii_digit() || (i == 0 && (c == '-' || c == '+'))))
                    .map_or(tail.len(), |(i, _)| i);
                tail[..digits_end].parse().unwrap_or(0)
            };
            let p = precision.unwrap_or(0);
            precision = Some(if exponent < 0 {
                p - exponent
            } else {
                p - p.min(exponent)
            });
            // The exponent itself is not output.
            width -= (s.len() - e) as i64;
            if exponent < 0 {
                if decimal_point.is_none_or(|dp| e == dp + 1) {
                    width += 1;
                }
                width += -exponent;
            } else {
                if decimal_point.is_some() && precision == Some(0) {
                    width -= 1; // no '.' once the fraction is shifted out
                }
                width += exponent - fraction_len.min(exponent);
            }
        }
    }
    let precision = precision.map(|p| p.max(0) as usize);
    let negative_zero = value == 0.0 && value.is_sign_negative();
    let int = if precision != Some(0) || negative_zero {
        None
    } else if let Ok(n) = s.parse::<i64>() {
        Some(n)
    } else {
        // Integral values written another way (`1e2`), while f64 is exact.
        (value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_992.0).then_some(value as i64)
    };
    Ok(SeqOperand {
        value,
        width: width.max(0) as usize,
        precision,
        int,
    })
}

/// A validated `-f` format: the text around its single floating-point
/// directive, and the directive itself.
struct SeqFormat {
    prefix: Vec<u8>,
    flags: PrintfFlags,
    width: usize,
    precision: Option<usize>,
    conv: u8,
    suffix: Vec<u8>,
}

impl SeqFormat {
    fn parse(fmt: &str) -> std::result::Result<Self, String> {
        let b = fmt.as_bytes();
        let unescape_pct = |part: &[u8]| -> Vec<u8> {
            let mut out = Vec::with_capacity(part.len());
            let mut i = 0;
            while i < part.len() {
                out.push(part[i]);
                i += if part[i] == b'%' { 2 } else { 1 };
            }
            out
        };
        let mut i = 0;
        let start = loop {
            match b.get(i) {
                None => return Err(format!("format {} has no % directive", gnu_quote(fmt))),
                Some(b'%') if b.get(i + 1) == Some(&b'%') => i += 2,
                Some(b'%') => break i,
                Some(_) => i += 1,
            }
        };
        i += 1;
        let mut flags = PrintfFlags::default();
        while let Some(&f) = b.get(i) {
            match f {
                b'-' => flags.left = true,
                b'0' => flags.zero = true,
                b'+' => flags.plus = true,
                b' ' => flags.space = true,
                b'#' => flags.alt = true,
                b'\'' => {}
                _ => break,
            }
            i += 1;
        }
        let mut width = 0usize;
        while let Some(d @ b'0'..=b'9') = b.get(i).copied() {
            width = width.saturating_mul(10).saturating_add((d - b'0') as usize);
            i += 1;
        }
        let mut precision = None;
        if b.get(i) == Some(&b'.') {
            i += 1;
            let mut p = 0usize;
            while let Some(d @ b'0'..=b'9') = b.get(i).copied() {
                p = p.saturating_mul(10).saturating_add((d - b'0') as usize);
                i += 1;
            }
            precision = Some(p);
        }
        if b.get(i) == Some(&b'L') {
            i += 1;
        }
        let conv = match b.get(i) {
            None => return Err(format!("format {} ends in %", gnu_quote(fmt))),
            Some(c) if b"efgaEFGA".contains(c) => *c,
            Some(_) => {
                let directive = &fmt[start..i + fmt[i..].chars().next().map_or(1, char::len_utf8)];
                return Err(format!(
                    "format {} has unknown {} directive",
                    gnu_quote(fmt),
                    directive
                ));
            }
        };
        let suffix = &b[i + 1..];
        let mut j = 0;
        while j < suffix.len() {
            if suffix[j] == b'%' {
                if suffix.get(j + 1) == Some(&b'%') {
                    j += 2;
                    continue;
                }
                return Err(format!(
                    "format {} has too many % directives",
                    gnu_quote(fmt)
                ));
            }
            j += 1;
        }
        Ok(Self {
            prefix: unescape_pct(&b[..start]),
            flags,
            width,
            precision,
            conv,
            suffix: unescape_pct(suffix),
        })
    }

    fn render(&self, v: f64) -> Vec<u8> {
        let mut out = self.prefix.clone();
        out.extend(self.render_number(v));
        out.extend_from_slice(&self.suffix);
        out
    }

    fn render_number(&self, v: f64) -> Vec<u8> {
        let conv = match self.conv {
            // Hexadecimal floats are rare in seq; render them as %g.
            b'a' => b'g',
            b'A' => b'G',
            c => c,
        };
        let (s, finite) = format_float(v, conv, self.precision, self.flags);
        pad_field(s.into_bytes(), self.width, self.flags, finite)
    }
}

/// The format GNU picks when no `-f` is given: `%.{prec}f` (or
/// `%0{width}.{prec}f` with `-w`), or `%g` when some operand has no fixed
/// precision (hexadecimal or infinite input).
fn seq_default_format(
    first: SeqOperand,
    step: SeqOperand,
    last: SeqOperand,
    equal_width: bool,
) -> SeqFormat {
    let mut fmt = SeqFormat {
        prefix: Vec::new(),
        flags: PrintfFlags::default(),
        width: 0,
        precision: None,
        conv: b'g',
        suffix: Vec::new(),
    };
    let (Some(fp), Some(sp), Some(lp)) = (first.precision, step.precision, last.precision) else {
        return fmt;
    };
    let prec = fp.max(sp);
    fmt.conv = b'f';
    fmt.precision = Some(prec);
    if equal_width {
        let first_width = first.width + (prec - fp) + usize::from(fp == 0 && prec > 0);
        let mut last_width = (last.width + prec).saturating_sub(lp);
        if lp > 0 && prec == 0 {
            last_width = last_width.saturating_sub(1);
        }
        if lp == 0 && prec > 0 {
            last_width += 1;
        }
        fmt.flags.zero = true;
        fmt.width = first_width.max(last_width);
    }
    fmt
}

fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit())
}

/// Adds `step` to the decimal digit string `n` in place.
fn decimal_add(n: &mut Vec<u8>, step: u32) {
    let mut carry = step;
    for d in n.iter_mut().rev() {
        if carry == 0 {
            return;
        }
        let v = u32::from(*d - b'0') + carry;
        *d = b'0' + (v % 10) as u8;
        carry = v / 10;
    }
    while carry > 0 {
        n.insert(0, b'0' + (carry % 10) as u8);
        carry /= 10;
    }
}

fn decimal_le(a: &[u8], b: &[u8]) -> bool {
    (a.len(), a) <= (b.len(), b)
}

fn write_or_pipe<W: Write>(out: &mut W, bytes: &[u8]) -> io::Result<bool> {
    match out.write_all(bytes) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(e) => Err(e),
    }
}

pub fn run_seq(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let mut opts = GetOpt::new(args.into_iter().skip(1).collect(), &SEQ_OPTS, false);
    let mut format: Option<String> = None;
    let mut separator = "\n".to_string();
    let mut equal_width = false;
    let mut json = false;
    loop {
        // A negative number is an operand, and ends option processing.
        if let Some(a) = opts.peek()
            && a.len() > 1
            && a.starts_with('-')
            && a[1..].starts_with(|c: char| c == '.' || c.is_ascii_digit())
        {
            break;
        }
        let Some(item) = opts.next_opt() else { break };
        match item {
            Ok(("format", v)) => format = v,
            Ok(("separator", v)) => separator = v.unwrap_or_default(),
            Ok(("equal-width", _)) => equal_width = true,
            Ok(("json", _)) => json = true,
            Ok(("help", _)) => return print_help(SEQ_HELP),
            Ok(_) => unreachable!("every seq option is handled"),
            Err(msg) => return Ok(usage_error("seq", msg, 1)),
        }
    }
    let nums = opts.into_operands();
    match nums.len() {
        0 => return Ok(usage_error("seq", "missing operand", 1)),
        1..=3 => {}
        _ => {
            return Ok(usage_error(
                "seq",
                format!("extra operand {}", gnu_quote(&nums[3])),
                1,
            ));
        }
    }
    let mut scanned = Vec::with_capacity(nums.len());
    for n in &nums {
        match scan_seq_operand(n) {
            Ok(s) => scanned.push(s),
            Err(msg) => return Ok(usage_error("seq", msg, 1)),
        }
    }
    let one = SeqOperand {
        value: 1.0,
        width: 1,
        precision: Some(0),
        int: Some(1),
    };
    let (first, step, last) = match scanned.as_slice() {
        [l] => (one, one, *l),
        [f, l] => (*f, one, *l),
        [f, s, l] => (*f, *s, *l),
        _ => unreachable!(),
    };
    if step.value == 0.0 {
        return Ok(usage_error(
            "seq",
            format!("invalid Zero increment value: {}", gnu_quote(&nums[1])),
            1,
        ));
    }
    if format.is_some() && equal_width {
        return Ok(usage_error(
            "seq",
            "format string may not be specified when printing equal width strings",
            1,
        ));
    }
    let user_format = match format.as_deref().map(SeqFormat::parse).transpose() {
        Ok(f) => f,
        Err(msg) => {
            eprintln!("seq: {msg}");
            return Ok(1);
        }
    };

    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());
    let out_of_range = |x: f64| {
        if step.value < 0.0 {
            x < last.value
        } else {
            last.value < x
        }
    };
    let ints = (first.int, step.int, last.int);

    if json {
        let all_int = matches!(ints, (Some(_), Some(_), Some(_)));
        let mut vals = Vec::new();
        let mut x = first.value;
        let mut i = 0f64;
        while !out_of_range(x) {
            vals.push(if all_int {
                serde_json::json!(x as i64)
            } else {
                serde_json::json!(x)
            });
            i += 1.0;
            x = first.value + i * step.value;
        }
        serde_json::to_writer(&mut out, &vals)?;
        writeln!(out)?;
        ignore_broken_pipe(out.flush())?;
        return Ok(0);
    }

    let sep = separator.as_bytes();
    // Non-negative integers with a small positive step: exact decimal
    // arithmetic on arbitrarily large numbers, like GNU's fast path (which
    // also takes integers written like `1e20`).
    // Digit strings are used as written; otherwise every operand goes through
    // its floating-point value, as GNU formats them with "%.0Lf".
    let digit_args = nums.iter().all(|n| all_digits(n));
    let as_digits = |o: &SeqOperand, text: &str| -> Option<Vec<u8>> {
        let digits = if digit_args {
            text.trim_start_matches('0').to_string()
        } else if o.precision == Some(0) && o.value.is_finite() && o.value.is_sign_positive() {
            format!("{:.0}", o.value)
        } else {
            return None;
        };
        Some(if digits.is_empty() {
            b"0".to_vec()
        } else {
            digits.into_bytes()
        })
    };
    let fast = match nums.len() {
        1 => as_digits(&last, &nums[0]).map(|end| (b"1".to_vec(), end)),
        n => as_digits(&first, &nums[0]).zip(as_digits(&last, &nums[n - 1])),
    };
    let small_step = step.int.is_some_and(|n| (1..=200).contains(&n));
    if let (None, false, true, Some((mut cur, end))) = (&user_format, equal_width, small_step, fast)
    {
        let inc = step.value as u32;
        let mut count = 0u64;
        // Like GNU's seq_fast: append to a local block and hand whole blocks to the
        // writer, instead of two small writes per number.
        let mut block: Vec<u8> = Vec::with_capacity(DEFAULT_BUF_SIZE + 64);
        while decimal_le(&cur, &end) {
            if count > 0 {
                block.extend_from_slice(sep);
            }
            block.extend_from_slice(&cur);
            count += 1;
            // Most increments touch only the last digit; carry the rest.
            let last = cur.len() - 1;
            let digit = u32::from(cur[last] - b'0') + inc;
            if digit < 10 {
                cur[last] = b'0' + digit as u8;
            } else {
                decimal_add(&mut cur, inc);
            }
            if block.len() >= DEFAULT_BUF_SIZE {
                if !write_or_pipe(&mut out, &block)? {
                    return Ok(0);
                }
                block.clear();
            }
        }
        ctx.telemetry.add_items(count);
        if count > 0 {
            block.push(b'\n');
        }
        if !write_or_pipe(&mut out, &block)? {
            return Ok(0);
        }
        ignore_broken_pipe(out.flush())?;
        return Ok(0);
    }

    if let (None, (Some(mut cur), Some(inc), Some(end))) = (&user_format, ints) {
        // Exact 64-bit integers; f64 would lose precision past 2^53.
        let width = if equal_width {
            first.width.max(last.width)
        } else {
            0
        };
        let mut count = 0u64;
        let mut itoa_buf = itoa::Buffer::new();
        while (inc > 0 && cur <= end) || (inc < 0 && cur >= end) {
            if count > 0 && !write_or_pipe(&mut out, sep)? {
                return Ok(0);
            }
            let written = if width > 0 {
                write_or_pipe(&mut out, format!("{:0width$}", cur).as_bytes())?
            } else {
                write_or_pipe(&mut out, itoa_buf.format(cur).as_bytes())?
            };
            if !written {
                return Ok(0);
            }
            count += 1;
            // Stop instead of wrapping around (which looped forever) near i64::MAX/MIN.
            match cur.checked_add(inc) {
                Some(next) => cur = next,
                None => break,
            }
        }
        ctx.telemetry.add_items(count);
        if count > 0 && !write_or_pipe(&mut out, b"\n")? {
            return Ok(0);
        }
        ignore_broken_pipe(out.flush())?;
        return Ok(0);
    }

    let fmt = user_format.unwrap_or_else(|| seq_default_format(first, step, last, equal_width));
    if out_of_range(first.value) {
        return Ok(0);
    }
    let mut x = first.value;
    let mut i = 1f64;
    let mut past_last = false;
    loop {
        if !write_or_pipe(&mut out, &fmt.render(x))? {
            return Ok(0);
        }
        ctx.telemetry.add_items(1);
        if past_last {
            break;
        }
        let previous = x;
        x = first.value + i * step.value;
        i += 1.0;
        past_last = out_of_range(x);
        if past_last {
            // Print one more number when it renders as LAST and differs from
            // the previous one, so rounding error doesn't drop the endpoint.
            let shown = fmt.render_number(x);
            let equals_last = std::str::from_utf8(&shown)
                .ok()
                .and_then(|s| strtod_full(s.trim()))
                == Some(last.value);
            if !(equals_last && shown != fmt.render_number(previous)) {
                break;
            }
        }
        if !write_or_pipe(&mut out, sep)? {
            return Ok(0);
        }
    }
    if !write_or_pipe(&mut out, b"\n")? {
        return Ok(0);
    }
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// 7. whoami
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "whoami",
    about = "Print the user name associated with the current effective user ID",
    disable_help_flag = true,
    infer_long_args = true
)]
struct WhoamiArgs {
    /// Display this help and exit
    #[arg(long, action = ArgAction::Help)]
    help: Option<bool>,

    #[arg(short = 'J', long = "json")]
    json: bool,
}

pub fn run_whoami(args: Vec<String>, _ctx: &DogfishContext) -> Result<i32> {
    let parsed = WhoamiArgs::try_parse_from(args)?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    let Some(username) = get_username_by_uid(uid) else {
        eprintln!("whoami: cannot find name for user ID {uid}");
        return Ok(1);
    };
    if parsed.json {
        println!(
            "{}",
            serde_json::json!({ "uid": uid, "username": username })
        );
    } else {
        println!("{}", username);
    }
    Ok(0)
}

pub fn get_username_by_uid(uid: u32) -> Option<String> {
    use std::cell::RefCell;
    use std::collections::HashMap;
    thread_local! {
        static UID_CACHE: RefCell<HashMap<u32, Option<String>>> = RefCell::new(HashMap::new());
    }
    UID_CACHE.with(|cache| {
        if let Some(cached) = cache.borrow().get(&uid) {
            return cached.clone();
        }
        let resolved = unsafe {
            let pw = libc::getpwuid(uid);
            if pw.is_null() || (*pw).pw_name.is_null() {
                None
            } else {
                Some(CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned())
            }
        };
        cache.borrow_mut().insert(uid, resolved.clone());
        resolved
    })
}

pub fn get_groupname_by_gid(gid: u32) -> Option<String> {
    use std::cell::RefCell;
    use std::collections::HashMap;
    thread_local! {
        static GID_CACHE: RefCell<HashMap<u32, Option<String>>> = RefCell::new(HashMap::new());
    }
    GID_CACHE.with(|cache| {
        if let Some(cached) = cache.borrow().get(&gid) {
            return cached.clone();
        }
        let resolved = unsafe {
            let gr = libc::getgrgid(gid);
            if gr.is_null() || (*gr).gr_name.is_null() {
                None
            } else {
                Some(CStr::from_ptr((*gr).gr_name).to_string_lossy().into_owned())
            }
        };
        cache.borrow_mut().insert(gid, resolved.clone());
        resolved
    })
}

// ---------------------------------------------------------------------------
// 8. id — With `--json` and colorized interactive output
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "id",
    about = "Print user and group information for each specified USER, \
             or (when USER omitted) for the current process",
    disable_help_flag = true,
    infer_long_args = true
)]
struct IdArgs {
    /// Ignore, for compatibility with other versions
    #[arg(short = 'a')]
    ignored: bool,

    /// Print only the security context of the process
    #[arg(short = 'Z', long = "context")]
    context: bool,

    /// Print only the effective group ID
    #[arg(short = 'g', long = "group")]
    group: bool,

    /// Print all group IDs
    #[arg(short = 'G', long = "groups")]
    groups: bool,

    /// Print a name instead of a number, for -u, -g, -G
    #[arg(short = 'n', long = "name")]
    name: bool,

    /// Print the real ID instead of the effective ID, with -u, -g, -G
    #[arg(short = 'r', long = "real")]
    real: bool,

    /// Print only the effective user ID
    #[arg(short = 'u', long = "user")]
    user: bool,

    /// Delimit entries with NUL characters, not whitespace
    #[arg(short = 'z', long = "zero")]
    zero: bool,

    /// Emit structured JSON
    #[arg(short = 'J', long = "json")]
    json: bool,

    /// Display this help and exit
    #[arg(long, action = ArgAction::Help)]
    help: Option<bool>,

    users: Vec<String>,
}

#[derive(Serialize)]
struct IdRecord {
    uid: u32,
    user: String,
    gid: u32,
    group: String,
    groups: Vec<(u32, String)>,
}

/// Looks `spec` up as a user name, falling back to a numeric user ID.
fn lookup_user(spec: &str) -> Option<(u32, u32, CString)> {
    let cname = CString::new(spec).ok()?;
    // SAFETY: getpwnam/getpwuid return null or a pointer to static storage that
    // stays valid until the next lookup; the fields are copied out immediately.
    unsafe {
        let mut pw = libc::getpwnam(cname.as_ptr());
        if pw.is_null()
            && let Ok(uid) = spec.parse::<u32>()
        {
            pw = libc::getpwuid(uid);
        }
        if pw.is_null() {
            return None;
        }
        Some((
            (*pw).pw_uid,
            (*pw).pw_gid,
            CStr::from_ptr((*pw).pw_name).to_owned(),
        ))
    }
}

/// The group list GNU's `mgetgroups` reports: `primary` first, then the user's
/// (or the process's) supplementary groups, minus duplicates of the first
/// entry and of their predecessor.
fn group_list(user: Option<&CStr>, primary: u32) -> Vec<u32> {
    let mut groups: Vec<u32> = vec![primary];
    match user {
        // SAFETY: getgroups is called with a correctly sized buffer.
        None => unsafe {
            let n = libc::getgroups(0, std::ptr::null_mut());
            if n > 0 {
                let mut buf = vec![0 as libc::gid_t; n as usize];
                let n = libc::getgroups(n, buf.as_mut_ptr());
                if n > 0 {
                    groups.extend_from_slice(&buf[..n as usize]);
                }
            }
        },
        Some(name) => {
            let mut size: libc::c_int = 64;
            loop {
                let mut buf = vec![0 as libc::gid_t; size as usize];
                let mut n = size;
                // SAFETY: `buf` holds `n` entries; getgrouplist updates `n`.
                #[cfg(not(target_vendor = "apple"))]
                let rc =
                    unsafe { libc::getgrouplist(name.as_ptr(), primary, buf.as_mut_ptr(), &mut n) };
                // SAFETY: as above; Apple's prototype uses `int` group IDs.
                #[cfg(target_vendor = "apple")]
                let rc = unsafe {
                    libc::getgrouplist(
                        name.as_ptr(),
                        primary as libc::c_int,
                        buf.as_mut_ptr().cast::<libc::c_int>(),
                        &mut n,
                    )
                };
                if rc >= 0 {
                    groups = buf[..n.max(0) as usize].to_vec();
                    groups.insert(0, primary);
                    break;
                }
                if size >= 1 << 16 {
                    break;
                }
                size = (size * 2).max(n);
            }
        }
    }
    let first = groups[0];
    let mut deduped = vec![first];
    for &g in &groups[1..] {
        if g != first && Some(&g) != deduped.last() {
            deduped.push(g);
        }
    }
    deduped
}

/// The identity `id` reports on: a named user, or the current process.
struct Identity {
    ruid: u32,
    euid: u32,
    rgid: u32,
    egid: u32,
    name: Option<CString>,
}

impl Identity {
    fn current() -> Self {
        // SAFETY: these getters have no preconditions and cannot fail.
        unsafe {
            Self {
                ruid: libc::getuid(),
                euid: libc::geteuid(),
                rgid: libc::getgid(),
                egid: libc::getegid(),
                name: None,
            }
        }
    }

    fn groups(&self) -> Vec<u32> {
        group_list(self.name.as_deref(), self.egid)
    }
}

/// Appends a user or group as GNU does with `-n`: the name, or the number plus
/// a diagnostic when there is none. Returns false if the name was missing.
fn push_id(out: &mut Vec<u8>, id: u32, use_name: bool, is_user: bool) -> bool {
    if !use_name {
        out.extend_from_slice(id.to_string().as_bytes());
        return true;
    }
    let name = if is_user {
        get_username_by_uid(id)
    } else {
        get_groupname_by_gid(id)
    };
    match name {
        Some(n) => {
            out.extend_from_slice(n.as_bytes());
            true
        }
        None => {
            let kind = if is_user { "user" } else { "group" };
            eprintln!("id: cannot find name for {kind} ID {id}");
            out.extend_from_slice(id.to_string().as_bytes());
            false
        }
    }
}

fn id_full_format(who: &Identity, st: dogfish_core::Style) -> String {
    let user = |id: u32| match get_username_by_uid(id) {
        Some(n) => format!("{}({})", st.bold_cyan(&id.to_string()), st.bold_green(&n)),
        None => st.bold_cyan(&id.to_string()),
    };
    let group = |id: u32, bold: bool| match get_groupname_by_gid(id) {
        Some(n) if bold => format!("{}({})", st.bold_cyan(&id.to_string()), st.bold_green(&n)),
        Some(n) => format!("{}({})", st.bold_cyan(&id.to_string()), st.green(&n)),
        None => st.bold_cyan(&id.to_string()),
    };
    let mut line = format!("uid={} gid={}", user(who.ruid), group(who.rgid, true));
    if who.euid != who.ruid {
        line.push_str(&format!(" euid={}", user(who.euid)));
    }
    if who.egid != who.rgid {
        line.push_str(&format!(" egid={}", group(who.egid, true)));
    }
    let groups: Vec<String> = who.groups().into_iter().map(|g| group(g, false)).collect();
    if !groups.is_empty() {
        line.push_str(&format!(" groups={}", groups.join(",")));
    }
    line
}

pub fn run_id(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = IdArgs::try_parse_from(args)?;
    let _ = parsed.ignored;
    // There is no SELinux support, so -Z can never be satisfied.
    if parsed.context {
        eprintln!("id: --context (-Z) works only on an SELinux-enabled kernel");
        return Ok(1);
    }
    let only = [parsed.user, parsed.group, parsed.groups];
    if only.iter().filter(|&&b| b).count() > 1 {
        eprintln!("id: cannot print \"only\" of more than one choice");
        return Ok(1);
    }
    let default_format = !only.contains(&true);
    if !parsed.json && default_format && (parsed.real || parsed.name) {
        eprintln!("id: cannot print only names or real IDs in default format");
        return Ok(1);
    }
    if !parsed.json && default_format && parsed.zero {
        eprintln!("id: option --zero not permitted in default format");
        return Ok(1);
    }

    let mut targets: Vec<Option<Identity>> = Vec::new();
    let mut status = 0;
    if parsed.users.is_empty() {
        targets.push(Some(Identity::current()));
    } else {
        for spec in &parsed.users {
            match lookup_user(spec) {
                Some((uid, gid, name)) => targets.push(Some(Identity {
                    ruid: uid,
                    euid: uid,
                    rgid: gid,
                    egid: gid,
                    name: Some(name),
                })),
                None => {
                    eprintln!("id: {}: no such user", gnu_quote(spec));
                    status = 1;
                    targets.push(None);
                }
            }
        }
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let st = ctx.style();
    let term = if parsed.zero { b'\0' } else { b'\n' };
    for who in targets.into_iter().flatten() {
        if parsed.json {
            let group_entry =
                |g: u32| (g, get_groupname_by_gid(g).unwrap_or_else(|| g.to_string()));
            let rec = IdRecord {
                uid: who.euid,
                user: get_username_by_uid(who.euid).unwrap_or_else(|| who.euid.to_string()),
                gid: who.egid,
                group: group_entry(who.egid).1,
                groups: who.groups().into_iter().map(group_entry).collect(),
            };
            writeln!(out, "{}", serde_json::to_string_pretty(&rec)?)?;
            continue;
        }
        let mut line = Vec::new();
        if parsed.user {
            let uid = if parsed.real { who.ruid } else { who.euid };
            if !push_id(&mut line, uid, parsed.name, true) {
                status = 1;
            }
        } else if parsed.group {
            let gid = if parsed.real { who.rgid } else { who.egid };
            if !push_id(&mut line, gid, parsed.name, false) {
                status = 1;
            }
        } else if parsed.groups {
            let delim = if parsed.zero { b'\0' } else { b' ' };
            let mut ok = push_id(&mut line, who.rgid, parsed.name, false);
            if who.egid != who.rgid {
                line.push(delim);
                ok &= push_id(&mut line, who.egid, parsed.name, false);
            }
            for g in who.groups() {
                if g != who.rgid && g != who.egid {
                    line.push(delim);
                    ok &= push_id(&mut line, g, parsed.name, false);
                }
            }
            if !ok {
                status = 1;
            }
        } else {
            line.extend_from_slice(id_full_format(&who, st).as_bytes());
        }
        line.push(term);
        ignore_broken_pipe(out.write_all(&line))?;
    }
    ignore_broken_pipe(out.flush())?;
    Ok(status)
}

// ---------------------------------------------------------------------------
// 9. uname — With `--json` and visual card mode
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "uname",
    about = "Print certain system information.  With no OPTION, same as -s.",
    disable_help_flag = true,
    infer_long_args = true
)]
struct UnameArgs {
    /// Print all information, in the following order, except omit -p and -i if unknown
    #[arg(short = 'a', long = "all")]
    all: bool,
    /// Print the kernel name
    #[arg(short = 's', long = "kernel-name")]
    sysname: bool,
    /// Print the network node hostname
    #[arg(short = 'n', long = "nodename")]
    nodename: bool,
    /// Print the kernel release
    #[arg(short = 'r', long = "kernel-release")]
    release: bool,
    /// Print the kernel version
    #[arg(short = 'v', long = "kernel-version")]
    version: bool,
    /// Print the machine hardware name
    #[arg(short = 'm', long = "machine")]
    machine: bool,
    /// Print the processor type (non-portable)
    #[arg(short = 'p', long = "processor")]
    processor: bool,
    /// Print the hardware platform (non-portable)
    #[arg(short = 'i', long = "hardware-platform")]
    platform: bool,
    /// Print the operating system
    #[arg(short = 'o', long = "operating-system")]
    os: bool,
    /// Emit structured JSON
    #[arg(short = 'J', long = "json")]
    json: bool,
    /// Display this help and exit
    #[arg(long, action = ArgAction::Help)]
    help: Option<bool>,
}

#[derive(Serialize)]
struct UnameInfo {
    sysname: String,
    nodename: String,
    release: String,
    version: String,
    machine: String,
    processor: String,
    hardware_platform: String,
    operating_system: String,
}

const UNKNOWN: &str = "unknown";

pub fn run_uname(args: Vec<String>, _ctx: &DogfishContext) -> Result<i32> {
    let parsed = UnameArgs::try_parse_from(args)?;
    let info = get_uname_info()?;

    if parsed.json {
        println!("{}", serde_json::to_string_pretty(&info)?);
        return Ok(0);
    }

    let any_specific = parsed.sysname
        || parsed.nodename
        || parsed.release
        || parsed.version
        || parsed.machine
        || parsed.processor
        || parsed.platform
        || parsed.os;
    let all = parsed.all;
    let fields = [
        (
            all || parsed.sysname || !(all || any_specific),
            &info.sysname,
        ),
        (all || parsed.nodename, &info.nodename),
        (all || parsed.release, &info.release),
        (all || parsed.version, &info.version),
        (all || parsed.machine, &info.machine),
        (
            parsed.processor || (all && info.processor != UNKNOWN),
            &info.processor,
        ),
        (
            parsed.platform || (all && info.hardware_platform != UNKNOWN),
            &info.hardware_platform,
        ),
        (all || parsed.os, &info.operating_system),
    ];
    let parts: Vec<&str> = fields
        .iter()
        .filter(|(wanted, _)| *wanted)
        .map(|(_, v)| v.as_str())
        .collect();
    println!("{}", parts.join(" "));
    Ok(0)
}

fn get_uname_info() -> Result<UnameInfo> {
    // SAFETY: `uts` is a valid out-pointer; on success every field holds a
    // NUL-terminated string.
    let uts = unsafe {
        let mut uts: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut uts) != 0 {
            return Err(io::Error::last_os_error().into());
        }
        uts
    };
    let field = |chars: &[libc::c_char]| -> String {
        let bytes: Vec<u8> = chars
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let machine = field(&uts.machine);
    let sysname = field(&uts.sysname);
    // GNU/Linux distributions report the machine for -p and -i; elsewhere
    // GNU has no portable answer.
    let (processor, hardware_platform) = if cfg!(target_os = "linux") {
        (machine.clone(), machine.clone())
    } else {
        (UNKNOWN.to_string(), UNKNOWN.to_string())
    };
    let operating_system = if cfg!(all(target_os = "linux", target_env = "gnu")) {
        "GNU/Linux".to_string()
    } else {
        sysname.clone()
    };
    Ok(UnameInfo {
        sysname,
        nodename: field(&uts.nodename),
        release: field(&uts.release),
        version: field(&uts.version),
        machine,
        processor,
        hardware_platform,
        operating_system,
    })
}

// ---------------------------------------------------------------------------
// 13. nproc — Print number of available processing units
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "nproc",
    about = "Print the number of processing units available to the current process, \
             which may be less than the number of online processors",
    disable_help_flag = true,
    infer_long_args = true
)]
struct NprocArgs {
    /// Print the number of installed processors
    #[arg(long = "all")]
    all: bool,

    /// If possible, exclude N processing units
    #[arg(long = "ignore", value_name = "N")]
    ignore: Option<String>,

    #[arg(short = 'J', long = "json")]
    json: bool,

    /// Display this help and exit
    #[arg(long, action = ArgAction::Help)]
    help: Option<bool>,
}

/// Parses an OpenMP thread count like gnulib: a decimal number, optionally
/// followed by a comma-separated list; anything else counts as unset (0).
fn parse_omp_threads(var: &str) -> u64 {
    let Ok(value) = std::env::var(var) else {
        return 0;
    };
    let s = value.trim_start();
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return 0;
    }
    let rest = s[digits..].trim_start();
    if rest.is_empty() || rest.starts_with(',') {
        s[..digits].parse().unwrap_or(u64::MAX)
    } else {
        0
    }
}

fn online_processors(all: bool) -> u64 {
    #[cfg(target_os = "linux")]
    if !all {
        // SAFETY: `set` is a properly sized, zeroed CPU set for the calling thread.
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) == 0 {
                let n = libc::CPU_COUNT(&set);
                if n > 0 {
                    return n as u64;
                }
            }
        }
    }
    let name = if all {
        libc::_SC_NPROCESSORS_CONF
    } else {
        libc::_SC_NPROCESSORS_ONLN
    };
    // SAFETY: sysconf has no preconditions.
    let n = unsafe { libc::sysconf(name) };
    if n > 0 { n as u64 } else { 1 }
}

pub fn run_nproc(args: Vec<String>, _ctx: &DogfishContext) -> Result<i32> {
    let parsed = NprocArgs::try_parse_from(args)?;
    let ignore = match parsed.ignore.as_deref() {
        None => 0,
        Some(s) => match s.trim_start().parse::<u64>() {
            Ok(n) => n,
            Err(e) => {
                let overflow = *e.kind() == std::num::IntErrorKind::PosOverflow;
                let detail = if overflow {
                    ": Value too large for defined data type"
                } else {
                    ""
                };
                eprintln!("nproc: invalid number: {}{detail}", gnu_quote(s));
                return Ok(1);
            }
        },
    };
    let total = online_processors(parsed.all);
    let available = if parsed.all {
        total
    } else {
        let mut limit = parse_omp_threads("OMP_THREAD_LIMIT");
        if limit == 0 {
            limit = u64::MAX;
        }
        match parse_omp_threads("OMP_NUM_THREADS") {
            0 => online_processors(false).min(limit),
            n => n.min(limit),
        }
    };
    let count = if ignore < available {
        available - ignore
    } else {
        1
    };
    if parsed.json {
        println!(
            "{}",
            serde_json::json!({ "available_cpus": count, "total_cpus": online_processors(true) })
        );
    } else {
        println!("{}", count);
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// 14. hostname — Print system network hostname
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "hostname",
    about = "Print the system's network host name",
    disable_help_flag = true,
    infer_long_args = true
)]
struct HostnameArgs {
    /// Print the host name cut at the first dot
    #[arg(short = 's', long = "short")]
    short: bool,

    /// Display this help and exit
    #[arg(long, action = ArgAction::Help)]
    help: Option<bool>,
}

pub fn run_hostname(args: Vec<String>, _ctx: &DogfishContext) -> Result<i32> {
    let parsed = HostnameArgs::try_parse_from(args)?;
    let info = get_uname_info()?;
    let name = if parsed.short {
        info.nodename.split('.').next().unwrap_or_default()
    } else {
        &info.nodename
    };
    println!("{}", name);
    Ok(0)
}

// ---------------------------------------------------------------------------
// 15. printenv — Print values of all or specified environment variables
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "printenv",
    about = "Print the values of the specified environment VARIABLE(s). \
             If no VARIABLE is specified, print name and value pairs for them all.",
    disable_help_flag = true,
    infer_long_args = true
)]
struct PrintenvArgs {
    /// End each output line with NUL, not newline
    #[arg(short = '0', long = "null")]
    null: bool,

    #[arg(short = 'J', long = "json")]
    json: bool,

    /// Display this help and exit
    #[arg(long, action = ArgAction::Help)]
    help: Option<bool>,

    vars: Vec<String>,
}

pub fn run_printenv(args: Vec<String>, _ctx: &DogfishContext) -> Result<i32> {
    let parsed = PrintenvArgs::try_parse_from(args)?;
    let term: &[u8] = if parsed.null { b"\0" } else { b"\n" };
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    if parsed.vars.is_empty() {
        if parsed.json {
            let map: std::collections::BTreeMap<String, String> = std::env::vars_os()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
                .collect();
            writeln!(out, "{}", serde_json::to_string_pretty(&map)?)?;
        } else {
            // Environment order, like GNU (which walks `environ`).
            for (k, v) in std::env::vars_os() {
                out.write_all(k.as_bytes())?;
                out.write_all(b"=")?;
                out.write_all(v.as_bytes())?;
                out.write_all(term)?;
            }
        }
        ignore_broken_pipe(out.flush())?;
        return Ok(0);
    }

    let mut exit_code = 0;
    for name in &parsed.vars {
        let value = (!name.contains('='))
            .then(|| std::env::var_os(name))
            .flatten();
        match value {
            Some(val) => {
                out.write_all(val.as_bytes())?;
                out.write_all(term)?;
            }
            None => exit_code = 1,
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(exit_code)
}

// ---------------------------------------------------------------------------
// Signal names, as gnulib's sig2str/str2sig spell them
// ---------------------------------------------------------------------------
#[cfg(target_os = "linux")]
const SIGNAL_NAMES: &[(&str, libc::c_int)] = &[
    ("HUP", libc::SIGHUP),
    ("INT", libc::SIGINT),
    ("QUIT", libc::SIGQUIT),
    ("ILL", libc::SIGILL),
    ("TRAP", libc::SIGTRAP),
    ("ABRT", libc::SIGABRT),
    ("BUS", libc::SIGBUS),
    ("FPE", libc::SIGFPE),
    ("KILL", libc::SIGKILL),
    ("USR1", libc::SIGUSR1),
    ("SEGV", libc::SIGSEGV),
    ("USR2", libc::SIGUSR2),
    ("PIPE", libc::SIGPIPE),
    ("ALRM", libc::SIGALRM),
    ("TERM", libc::SIGTERM),
    ("STKFLT", libc::SIGSTKFLT),
    ("CHLD", libc::SIGCHLD),
    ("CONT", libc::SIGCONT),
    ("STOP", libc::SIGSTOP),
    ("TSTP", libc::SIGTSTP),
    ("TTIN", libc::SIGTTIN),
    ("TTOU", libc::SIGTTOU),
    ("URG", libc::SIGURG),
    ("XCPU", libc::SIGXCPU),
    ("XFSZ", libc::SIGXFSZ),
    ("VTALRM", libc::SIGVTALRM),
    ("PROF", libc::SIGPROF),
    ("WINCH", libc::SIGWINCH),
    ("POLL", libc::SIGPOLL),
    ("PWR", libc::SIGPWR),
    ("SYS", libc::SIGSYS),
    // Aliases: accepted as input, never printed.
    ("IOT", libc::SIGABRT),
    ("CLD", libc::SIGCHLD),
    ("IO", libc::SIGIO),
];

#[cfg(not(target_os = "linux"))]
const SIGNAL_NAMES: &[(&str, libc::c_int)] = &[
    ("HUP", libc::SIGHUP),
    ("INT", libc::SIGINT),
    ("QUIT", libc::SIGQUIT),
    ("ILL", libc::SIGILL),
    ("TRAP", libc::SIGTRAP),
    ("ABRT", libc::SIGABRT),
    ("EMT", libc::SIGEMT),
    ("FPE", libc::SIGFPE),
    ("KILL", libc::SIGKILL),
    ("BUS", libc::SIGBUS),
    ("SEGV", libc::SIGSEGV),
    ("SYS", libc::SIGSYS),
    ("PIPE", libc::SIGPIPE),
    ("ALRM", libc::SIGALRM),
    ("TERM", libc::SIGTERM),
    ("URG", libc::SIGURG),
    ("STOP", libc::SIGSTOP),
    ("TSTP", libc::SIGTSTP),
    ("CONT", libc::SIGCONT),
    ("CHLD", libc::SIGCHLD),
    ("TTIN", libc::SIGTTIN),
    ("TTOU", libc::SIGTTOU),
    ("IO", libc::SIGIO),
    ("XCPU", libc::SIGXCPU),
    ("XFSZ", libc::SIGXFSZ),
    ("VTALRM", libc::SIGVTALRM),
    ("PROF", libc::SIGPROF),
    ("WINCH", libc::SIGWINCH),
    ("INFO", libc::SIGINFO),
    ("USR1", libc::SIGUSR1),
    ("USR2", libc::SIGUSR2),
    ("IOT", libc::SIGABRT),
    ("CLD", libc::SIGCHLD),
];

/// Real-time signal range, if the platform has one.
fn rt_signal_range() -> Option<(libc::c_int, libc::c_int)> {
    #[cfg(target_os = "linux")]
    {
        Some((libc::SIGRTMIN(), libc::SIGRTMAX()))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Largest signal number worth probing (GNU's SIGNUM_BOUND).
fn signum_bound() -> libc::c_int {
    rt_signal_range().map_or(31, |(_, max)| max)
}

/// Writes the name of `sig` (`TERM`, `RTMIN+2`, or the number when unnamed)
/// into `buf` without allocating, so a signal handler may use it.
fn sig_name_into(sig: libc::c_int, buf: &mut [u8; 16]) -> &[u8] {
    fn put_num(buf: &mut [u8], mut at: usize, n: libc::c_int) -> usize {
        let mut digits = [0u8; 10];
        let mut len = 0;
        let mut v = n.unsigned_abs();
        loop {
            digits[len] = b'0' + (v % 10) as u8;
            len += 1;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        for i in (0..len).rev() {
            buf[at] = digits[i];
            at += 1;
        }
        at
    }
    if let Some((name, _)) = SIGNAL_NAMES.iter().find(|(_, n)| *n == sig) {
        buf[..name.len()].copy_from_slice(name.as_bytes());
        return &buf[..name.len()];
    }
    if let Some((min, max)) = rt_signal_range()
        && (min..=max).contains(&sig)
    {
        let (base, offset) = if sig - min <= (max - min) / 2 {
            (b"RTMIN", sig - min)
        } else {
            (b"RTMAX", sig - max)
        };
        buf[..5].copy_from_slice(base);
        let mut len = 5;
        if offset != 0 {
            buf[len] = if offset > 0 { b'+' } else { b'-' };
            len = put_num(buf, len + 1, offset);
        }
        return &buf[..len];
    }
    let len = put_num(buf, 0, sig);
    &buf[..len]
}

fn sig_name(sig: libc::c_int) -> String {
    let mut buf = [0u8; 16];
    String::from_utf8_lossy(sig_name_into(sig, &mut buf)).into_owned()
}

/// Whether `sig` is a signal number gnulib's `sig2str` can name (0 included).
fn sig_known(sig: libc::c_int) -> bool {
    sig == 0
        || SIGNAL_NAMES.iter().any(|(_, n)| *n == sig)
        || rt_signal_range().is_some_and(|(min, max)| (min..=max).contains(&sig))
}

/// Parses a signal like GNU `operand2sig`: a number (or an exit status that
/// encodes one), or a name in any case with an optional `SIG` prefix.
fn parse_signal(spec: &str) -> Option<libc::c_int> {
    let sig = if spec.starts_with(|c: char| c.is_ascii_digit()) {
        let n: libc::c_int = spec.parse().ok()?;
        // An exit status like 137 that encodes a signal (WIFSIGNALED/WTERMSIG).
        if (n & 0x7f) != 0 && (n & 0x7f) != 0x7f {
            n & 0x7f
        } else {
            n
        }
    } else {
        let upper = spec.to_ascii_uppercase();
        let lookup = |name: &str| -> Option<libc::c_int> {
            if let Some((_, n)) = SIGNAL_NAMES.iter().find(|(s, _)| *s == name) {
                return Some(*n);
            }
            let (min, max) = rt_signal_range()?;
            let (base, rest) = if let Some(r) = name.strip_prefix("RTMIN") {
                (min, r)
            } else {
                (max, name.strip_prefix("RTMAX")?)
            };
            let offset: libc::c_int = if rest.is_empty() {
                0
            } else {
                rest.parse().ok()?
            };
            let sig = base + offset;
            (min..=max).contains(&sig).then_some(sig)
        };
        lookup(&upper).or_else(|| lookup(upper.strip_prefix("SIG")?))?
    };
    sig_known(sig).then_some(sig)
}

// ---------------------------------------------------------------------------
// 10. env — run a program in a modified environment (`-i`, `-u`, `-S`, `-C`)
// ---------------------------------------------------------------------------
const ENV_HELP: &str = "\
Usage: env [OPTION]... [-] [NAME=VALUE]... [COMMAND [ARG]...]
Set each NAME to VALUE in the environment and run COMMAND.

  -i, --ignore-environment  start with an empty environment
  -0, --null           end each output line with NUL, not newline
  -u, --unset=NAME     remove variable from the environment
  -C, --chdir=DIR      change working directory to DIR
  -S, --split-string=S  process and split S into separate arguments;
                        used to pass multiple arguments on shebang lines
      --block-signal[=SIG]    block delivery of SIG signal(s) to COMMAND
      --default-signal[=SIG]  reset handling of SIG signal(s) to the default
      --ignore-signal[=SIG]   set handling of SIG signal(s) to do nothing
      --list-signal-handling  list non default signal handling to stderr
  -v, --debug          print verbose information for each processing step
  -J, --json           print the environment as JSON objects
  -t, --table          print the environment as a table
      --help        display this help and exit
      --version     output version information and exit

A mere - implies -i.  If no COMMAND, print the resulting environment.

SIG may be a signal name like 'PIPE', or a signal number like '13'.
Without SIG, all known signals are included.  Multiple signals can be
comma-separated.  An empty SIG argument is a no-op.

Exit status:
  125  if the env command itself fails
  126  if COMMAND is found but cannot be invoked
  127  if COMMAND cannot be found
  -    the exit status of COMMAND otherwise
";

static ENV_OPTS: [OptSpec; 14] = [
    opt(
        Some('i'),
        Some("ignore-environment"),
        HasArg::No,
        "ignore-environment",
    ),
    opt(Some('0'), Some("null"), HasArg::No, "null"),
    opt(Some('u'), Some("unset"), HasArg::Required, "unset"),
    opt(Some('C'), Some("chdir"), HasArg::Required, "chdir"),
    opt(
        Some('S'),
        Some("split-string"),
        HasArg::Required,
        "split-string",
    ),
    opt(Some('v'), Some("debug"), HasArg::No, "debug"),
    opt(None, Some("block-signal"), HasArg::Optional, "block-signal"),
    opt(
        None,
        Some("default-signal"),
        HasArg::Optional,
        "default-signal",
    ),
    opt(
        None,
        Some("ignore-signal"),
        HasArg::Optional,
        "ignore-signal",
    ),
    opt(
        None,
        Some("list-signal-handling"),
        HasArg::No,
        "list-signal-handling",
    ),
    opt(Some('J'), Some("json"), HasArg::No, "json"),
    opt(Some('t'), Some("table"), HasArg::No, "table"),
    opt(None, Some("help"), HasArg::No, "help"),
    opt(None, Some("version"), HasArg::No, "version"),
];

/// GNU `env` reports its own failures with status 125.
const EXIT_CANCELED: i32 = 125;
const EXIT_CANNOT_INVOKE: i32 = 126;
const EXIT_ENOENT: i32 = 127;

/// Splits an `env -S` string into arguments like GNU: whitespace separates,
/// single and double quotes group, backslash escapes, `${NAME}` expands,
/// `#` starts a comment and `\c` ends the string.
fn split_env_string(s: &str) -> std::result::Result<Vec<String>, String> {
    let b = s.as_bytes();
    let mut args: Vec<Vec<u8>> = Vec::new();
    let mut cur: Option<Vec<u8>> = None;
    let (mut sq, mut dq) = (false, false);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        i += 1;
        match c {
            b'\'' if !dq => {
                sq = !sq;
                cur.get_or_insert_with(Vec::new);
            }
            b'"' if !sq => {
                dq = !dq;
                cur.get_or_insert_with(Vec::new);
            }
            b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r' if !sq && !dq => {
                args.extend(cur.take());
            }
            b'#' if !sq && !dq && cur.is_none() => break,
            b'$' if !sq => {
                let tail = &s[i - 1..];
                let err = || format!("only ${{VARNAME}} expansion is supported, error at: {tail}");
                if b.get(i) != Some(&b'{') {
                    return Err(err());
                }
                let name_len = b[i + 1..]
                    .iter()
                    .enumerate()
                    .take_while(|&(k, &ch)| {
                        ch == b'_' || ch.is_ascii_alphabetic() || (k > 0 && ch.is_ascii_digit())
                    })
                    .count();
                if name_len == 0 || b.get(i + 1 + name_len) != Some(&b'}') {
                    return Err(err());
                }
                let name = &s[i + 1..i + 1 + name_len];
                // An empty expansion does not by itself make an argument.
                let value = std::env::var_os(name).unwrap_or_default();
                if !value.is_empty() {
                    cur.get_or_insert_with(Vec::new)
                        .extend_from_slice(value.as_bytes());
                }
                i += name_len + 2;
            }
            b'\\' if !sq || matches!(b.get(i), Some(b'\\' | b'\'')) => {
                let Some(&e) = b.get(i) else {
                    return Err("invalid backslash at end of string in -S".to_string());
                };
                i += 1;
                let literal = match e {
                    b'"' | b'#' | b'$' | b'\'' | b'\\' => e,
                    b'_' if dq => b' ',
                    b'_' => {
                        args.extend(cur.take());
                        continue;
                    }
                    b'c' if dq => {
                        return Err("'\\c' must not appear in double-quoted -S string".to_string());
                    }
                    b'c' => break,
                    b'f' => 0x0c,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'v' => 0x0b,
                    _ => {
                        let seq = s[i - 1..].chars().next().unwrap_or('?');
                        return Err(format!("invalid sequence '\\{seq}' in -S"));
                    }
                };
                cur.get_or_insert_with(Vec::new).push(literal);
            }
            _ => cur.get_or_insert_with(Vec::new).push(c),
        }
    }
    if sq || dq {
        return Err("no terminating quote in -S string".to_string());
    }
    args.extend(cur);
    Ok(args
        .into_iter()
        .map(|a| String::from_utf8_lossy(&a).into_owned())
        .collect())
}

/// What to do with the signals named by `--block/default/ignore-signal`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SignalAction {
    Block,
    Default,
    Ignore,
}

/// Parses a `--*-signal` list (`PIPE,INT`); `None` means every signal.
fn parse_signal_list(list: Option<&str>) -> std::result::Result<Option<Vec<libc::c_int>>, String> {
    let Some(list) = list else { return Ok(None) };
    let mut sigs = Vec::new();
    for item in list.split(',').filter(|s| !s.is_empty()) {
        match parse_signal(item) {
            Some(sig) if sig != 0 => sigs.push(sig),
            _ => return Err(format!("{}: invalid signal", gnu_quote(item))),
        }
    }
    Ok(Some(sigs))
}

/// Applies the signal options; a signal named explicitly that cannot be
/// changed (e.g. `STOP`) is an error, as in GNU.
fn apply_signal_actions(
    actions: &[(SignalAction, Option<Vec<libc::c_int>>)],
) -> std::result::Result<(), String> {
    let all: Vec<libc::c_int> = (1..=signum_bound())
        .filter(|&s| s != libc::SIGKILL && s != libc::SIGSTOP)
        .collect();
    // SAFETY: plain signal-disposition and mask updates for this process,
    // which is single-threaded here and about to exec or print and exit.
    unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        let mut block = false;
        for (action, sigs) in actions {
            for &sig in sigs.as_deref().unwrap_or(&all) {
                let handler = match action {
                    SignalAction::Block => {
                        libc::sigaddset(&mut mask, sig);
                        block = true;
                        continue;
                    }
                    SignalAction::Default => libc::SIG_DFL,
                    SignalAction::Ignore => libc::SIG_IGN,
                };
                if libc::signal(sig, handler) == libc::SIG_ERR && sigs.is_some() {
                    let err = io::Error::last_os_error();
                    return Err(format!(
                        "failed to set signal action for signal {sig}: {}",
                        gnu_io_error(&err)
                    ));
                }
            }
        }
        if block {
            libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
        }
    }
    Ok(())
}

fn list_signal_handling() {
    let stderr = io::stderr();
    let mut err = stderr.lock();
    // SAFETY: querying (not changing) the mask and dispositions.
    unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigprocmask(libc::SIG_BLOCK, std::ptr::null(), &mut mask);
        for sig in 1..=signum_bound() {
            let mut act: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(sig, std::ptr::null(), &mut act) != 0 {
                continue;
            }
            let ignored = act.sa_sigaction == libc::SIG_IGN;
            let blocked = libc::sigismember(&mask, sig) == 1;
            if !ignored && !blocked {
                continue;
            }
            let state = match (blocked, ignored) {
                (true, true) => "BLOCK,IGNORE",
                (true, false) => "BLOCK",
                _ => "IGNORE",
            };
            let _ = writeln!(err, "{:<10} ({:>2}): {}", sig_name(sig), sig, state);
        }
    }
}

/// The environment GNU `env` builds: `NAME=VALUE` entries in `environ` order,
/// with `putenv` semantics (replace in place, else append).
struct EnvList(Vec<Vec<u8>>);

impl EnvList {
    fn name_of(entry: &[u8]) -> &[u8] {
        let eq = entry.iter().position(|&c| c == b'=').unwrap_or(entry.len());
        &entry[..eq]
    }

    fn unset(&mut self, name: &[u8]) {
        self.0.retain(|e| Self::name_of(e) != name);
    }

    fn put(&mut self, assignment: &[u8]) {
        let name = Self::name_of(assignment);
        match self.0.iter_mut().find(|e| Self::name_of(e) == name) {
            Some(e) => *e = assignment.to_vec(),
            None => self.0.push(assignment.to_vec()),
        }
    }

    fn get(&self, name: &[u8]) -> Option<&[u8]> {
        self.0
            .iter()
            .find(|e| Self::name_of(e) == name)
            .map(|e| &e[name.len() + 1..])
    }
}

/// Replaces this process with `argv[0]`, searching `PATH` (taken from the new
/// environment, defaulting to `/bin:/usr/bin`) the way `execvp` does. Returns
/// only on failure, with the error to report.
fn exec_with_env(argv: &[String], env: &EnvList) -> io::Error {
    let to_c = |b: &[u8]| CString::new(b.to_vec()).ok();
    let Some(c_args) = argv
        .iter()
        .map(|a| to_c(a.as_bytes()))
        .collect::<Option<Vec<_>>>()
    else {
        return io::Error::from_raw_os_error(libc::EINVAL);
    };
    let c_env: Vec<CString> = env.0.iter().filter_map(|e| to_c(e)).collect();
    let mut arg_ptrs: Vec<*const libc::c_char> = c_args.iter().map(|a| a.as_ptr()).collect();
    arg_ptrs.push(std::ptr::null());
    let mut env_ptrs: Vec<*const libc::c_char> = c_env.iter().map(|e| e.as_ptr()).collect();
    env_ptrs.push(std::ptr::null());

    let exec = |path: &CStr| -> io::Error {
        // SAFETY: `path`, `arg_ptrs` and `env_ptrs` are valid NUL-terminated
        // strings and null-terminated pointer arrays that outlive the call.
        unsafe { libc::execve(path.as_ptr(), arg_ptrs.as_ptr(), env_ptrs.as_ptr()) };
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOEXEC) {
            // Like execvp: run a script without a #! line through the shell.
            let mut sh_args: Vec<*const libc::c_char> = vec![c"/bin/sh".as_ptr(), path.as_ptr()];
            sh_args.extend_from_slice(&arg_ptrs[1..]);
            // SAFETY: as above.
            unsafe { libc::execve(c"/bin/sh".as_ptr(), sh_args.as_ptr(), env_ptrs.as_ptr()) };
        }
        err
    };

    let cmd = argv[0].as_bytes();
    if cmd.contains(&b'/') {
        return exec(&c_args[0]);
    }
    if cmd.is_empty() {
        return io::Error::from_raw_os_error(libc::ENOENT);
    }
    let path = env.get(b"PATH").unwrap_or(b"/bin:/usr/bin");
    let mut denied = false;
    let mut last = io::Error::from_raw_os_error(libc::ENOENT);
    for dir in path.split(|&c| c == b':') {
        let mut candidate = if dir.is_empty() {
            Vec::new()
        } else {
            let mut d = dir.to_vec();
            d.push(b'/');
            d
        };
        candidate.extend_from_slice(cmd);
        let Some(c_path) = to_c(&candidate) else {
            continue;
        };
        let err = exec(&c_path);
        match err.raw_os_error() {
            Some(libc::EACCES) => denied = true,
            Some(libc::ENOENT | libc::ENOTDIR | libc::ESTALE | libc::ETIMEDOUT | libc::ENODEV) => {}
            _ => return err,
        }
        last = err;
    }
    if denied {
        io::Error::from_raw_os_error(libc::EACCES)
    } else {
        last
    }
}

#[derive(Serialize)]
struct EnvRecord {
    key: String,
    value: String,
}

pub fn run_env(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let usage = |msg: String| Ok(usage_error("env", msg, EXIT_CANCELED));
    let mut opts = GetOpt::new(args.into_iter().skip(1).collect(), &ENV_OPTS, false);
    let mut ignore_env = false;
    let mut null = false;
    let mut debug = false;
    let mut json = false;
    let mut table = false;
    let mut list_signals = false;
    let mut unset: Vec<String> = Vec::new();
    let mut chdir: Option<String> = None;
    let mut signal_actions: Vec<(SignalAction, Option<Vec<libc::c_int>>)> = Vec::new();
    while let Some(item) = opts.next_opt() {
        let (key, value) = match item {
            Ok(kv) => kv,
            Err(msg) => return usage(msg),
        };
        match key {
            "ignore-environment" => ignore_env = true,
            "null" => null = true,
            "unset" => unset.push(value.unwrap_or_default()),
            "chdir" => chdir = value,
            "debug" => debug = true,
            "json" => json = true,
            "table" => table = true,
            "list-signal-handling" => list_signals = true,
            "help" => return print_help(ENV_HELP),
            "version" => {
                println!("env (GNU/Dogfish) {}", env!("CARGO_PKG_VERSION"));
                return Ok(0);
            }
            "split-string" => {
                let s = value.unwrap_or_default();
                match split_env_string(&s) {
                    Ok(items) => {
                        if debug {
                            eprintln!("split -S:  {}", gnu_quote(&s));
                            for (i, item) in items.iter().enumerate() {
                                let lead = if i == 0 { " into:    " } else { "     &    " };
                                eprintln!("{lead}{}", gnu_quote(item));
                            }
                        }
                        opts.splice_front(items);
                    }
                    Err(msg) => {
                        eprintln!("env: {msg}");
                        return Ok(EXIT_CANCELED);
                    }
                }
            }
            signal_opt => {
                let action = match signal_opt {
                    "block-signal" => SignalAction::Block,
                    "default-signal" => SignalAction::Default,
                    _ => SignalAction::Ignore,
                };
                match parse_signal_list(value.as_deref()) {
                    Ok(sigs) => signal_actions.push((action, sigs)),
                    Err(msg) => return usage(msg),
                }
            }
        }
    }
    let mut rest = opts.into_operands();
    if rest.first().is_some_and(|a| a == "-") {
        ignore_env = true;
        rest.remove(0);
    }

    let mut env = EnvList(if ignore_env {
        Vec::new()
    } else {
        std::env::vars_os()
            .map(|(k, v)| {
                let mut e = k.into_vec();
                e.push(b'=');
                e.extend_from_slice(v.as_bytes());
                e
            })
            .collect()
    });
    if ignore_env {
        if debug {
            eprintln!("cleaning environ");
        }
    } else {
        for name in &unset {
            if name.is_empty() || name.contains('=') {
                eprintln!("env: cannot unset {}: Invalid argument", gnu_quote(name));
                return Ok(EXIT_CANCELED);
            }
            if debug {
                eprintln!("unset:    {name}");
            }
            env.unset(name.as_bytes());
        }
    }
    let assignments = rest.iter().take_while(|a| a.contains('=')).count();
    for a in &rest[..assignments] {
        if debug {
            eprintln!("setenv:   {a}");
        }
        env.put(a.as_bytes());
    }
    let command = &rest[assignments..];

    if let Err(msg) = apply_signal_actions(&signal_actions) {
        eprintln!("env: {msg}");
        return Ok(EXIT_CANCELED);
    }
    if list_signals {
        list_signal_handling();
    }

    if command.is_empty() {
        if chdir.is_some() {
            return usage("must specify command with --chdir (-C)".to_string());
        }
        return print_env(&env, null, json, table, ctx);
    }
    if null {
        return usage("cannot specify --null (-0) with command".to_string());
    }
    if let Some(dir) = &chdir {
        if debug {
            eprintln!("chdir:    {}", gnu_quote(dir));
        }
        if let Err(e) = std::env::set_current_dir(dir) {
            eprintln!(
                "env: cannot change directory to {}: {}",
                gnu_quote(dir),
                gnu_io_error(&e)
            );
            return Ok(EXIT_CANCELED);
        }
    }
    if debug {
        eprintln!("executing: {}", command[0]);
        for (i, a) in command.iter().enumerate() {
            eprintln!("   arg[{i}]= {}", gnu_quote(a));
        }
    }
    let _ = io::stdout().flush();
    let err = exec_with_env(command, &env);
    let status = if err.raw_os_error() == Some(libc::ENOENT) {
        EXIT_ENOENT
    } else {
        EXIT_CANNOT_INVOKE
    };
    eprintln!("env: {}: {}", gnu_quote(&command[0]), gnu_io_error(&err));
    Ok(status)
}

fn print_env(
    env: &EnvList,
    null: bool,
    json: bool,
    table: bool,
    ctx: &DogfishContext,
) -> Result<i32> {
    if json || table {
        let records: Vec<EnvRecord> = env
            .0
            .iter()
            .map(|e| {
                let name = EnvList::name_of(e);
                EnvRecord {
                    key: String::from_utf8_lossy(name).into_owned(),
                    value: String::from_utf8_lossy(e.get(name.len() + 1..).unwrap_or_default())
                        .into_owned(),
                }
            })
            .collect();
        if json {
            dogfish_core::emit_objects_or_table(&records, ctx, true, false)?;
        } else {
            let mut tbl = Table::new(vec![
                ("VARIABLE", ColumnAlign::Left),
                ("VALUE", ColumnAlign::Left),
            ]);
            let st = ctx.style();
            for r in &records {
                tbl.add_row(vec![st.bold_cyan(&r.key), r.value.clone()]);
            }
            let stdout = io::stdout();
            let mut out = stdout.lock();
            tbl.render(&mut out, st)?;
        }
        return Ok(0);
    }
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    let term: &[u8] = if null { b"\0" } else { b"\n" };
    for e in &env.0 {
        out.write_all(e)?;
        out.write_all(term)?;
    }
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// 16. timeout — run a command with a time limit (`timeout -k 5 -s INT 1m cmd`)
// ---------------------------------------------------------------------------
const TIMEOUT_HELP: &str = "\
Usage: timeout [OPTION] DURATION COMMAND [ARG]...
  or:  timeout [OPTION]
Start COMMAND, and kill it if still running after DURATION.

Mandatory arguments to long options are mandatory for short options too.
      --preserve-status
                 exit with the same status as COMMAND, even when the
                   command times out
      --foreground
                 when not running timeout directly from a shell prompt,
                   allow COMMAND to read from the TTY and get TTY signals;
                   in this mode, children of COMMAND will not be timed out
  -k, --kill-after=DURATION
                 also send a KILL signal if COMMAND is still running
                   this long after the initial signal was sent
  -s, --signal=SIGNAL
                 specify the signal to be sent on timeout;
                   SIGNAL may be a name like 'HUP' or a number;
                   see 'kill -l' for a list of signals
  -v, --verbose  diagnose to stderr any signal sent upon timeout
      --help        display this help and exit
      --version     output version information and exit

DURATION is a floating point number with an optional suffix:
's' for seconds (the default), 'm' for minutes, 'h' for hours or 'd' for days.
A duration of 0 disables the associated timeout.  Compound durations such
as 500ms or 1m30s are accepted too.

Upon timeout, send the TERM signal to COMMAND, if no other SIGNAL specified.
The TERM signal kills any process that does not block or catch that signal.
It may be necessary to use the KILL signal, since this signal can't be caught.

Exit status:
  124  if COMMAND times out, and --preserve-status is not specified
  125  if the timeout command itself fails
  126  if COMMAND is found but cannot be invoked
  127  if COMMAND cannot be found
  137  if COMMAND (or timeout itself) is sent the KILL (9) signal (128+9)
  -    the exit status of COMMAND otherwise
";

static TIMEOUT_OPTS: [OptSpec; 7] = [
    opt(
        Some('k'),
        Some("kill-after"),
        HasArg::Required,
        "kill-after",
    ),
    opt(Some('s'), Some("signal"), HasArg::Required, "signal"),
    opt(Some('v'), Some("verbose"), HasArg::No, "verbose"),
    opt(None, Some("foreground"), HasArg::No, "foreground"),
    opt(None, Some("preserve-status"), HasArg::No, "preserve-status"),
    opt(None, Some("help"), HasArg::No, "help"),
    opt(None, Some("version"), HasArg::No, "version"),
];

/// State shared with the async-signal handler, which may only touch atomics
/// and async-signal-safe calls.
static TIMEOUT_PID: AtomicI32 = AtomicI32::new(0);
static TIMEOUT_SIGNAL: AtomicI32 = AtomicI32::new(libc::SIGTERM);
static TIMEOUT_KILL_AFTER_NS: AtomicU64 = AtomicU64::new(0);
static TIMEOUT_TIMED_OUT: AtomicBool = AtomicBool::new(false);
static TIMEOUT_FOREGROUND: AtomicBool = AtomicBool::new(false);
/// Preformatted `timeout: sending signal ` and ` to command 'CMD'\n` for -v.
static TIMEOUT_VERBOSE: std::sync::OnceLock<(Vec<u8>, Vec<u8>)> = std::sync::OnceLock::new();

/// Arms the real-time interval timer to deliver SIGALRM after `ns` nanoseconds.
fn arm_timer(ns: u64) {
    let secs = ns / 1_000_000_000;
    let usecs = (ns % 1_000_000_000).div_ceil(1000);
    let value = libc::itimerval {
        it_interval: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: libc::timeval {
            tv_sec: secs.min(i32::MAX as u64) as libc::time_t,
            tv_usec: usecs as libc::suseconds_t,
        },
    };
    // SAFETY: setitimer only reads `value`; it is a plain system call.
    unsafe {
        libc::setitimer(libc::ITIMER_REAL, &value, std::ptr::null_mut());
    }
}

fn duration_to_ns(secs: f64) -> u64 {
    if secs <= 0.0 {
        0
    } else {
        // Round up so tiny positive durations still time out.
        (secs * 1e9).ceil().min(u64::MAX as f64).max(1.0) as u64
    }
}

/// Signal handler: on SIGALRM send the configured signal (then arm the
/// `--kill-after` timer); forward any other caught signal to the command.
extern "C" fn timeout_on_signal(sig: libc::c_int) {
    let mut sig = sig;
    if sig == libc::SIGALRM {
        TIMEOUT_TIMED_OUT.store(true, Ordering::SeqCst);
        sig = TIMEOUT_SIGNAL.load(Ordering::SeqCst);
    }
    let pid = TIMEOUT_PID.load(Ordering::SeqCst);
    // SAFETY: only async-signal-safe calls (setitimer, write, signal, kill, _exit).
    unsafe {
        if pid <= 0 {
            // Not exec'd yet (we are the forked child): just go.
            libc::_exit(128 + sig);
        }
        let kill_after = TIMEOUT_KILL_AFTER_NS.swap(0, Ordering::SeqCst);
        if kill_after != 0 {
            TIMEOUT_SIGNAL.store(libc::SIGKILL, Ordering::SeqCst);
            arm_timer(kill_after);
        }
        if let Some((head, tail)) = TIMEOUT_VERBOSE.get() {
            let mut buf = [0u8; 16];
            let name = sig_name_into(sig, &mut buf);
            for part in [head.as_slice(), name, tail.as_slice()] {
                libc::write(libc::STDERR_FILENO, part.as_ptr().cast(), part.len());
            }
        }
        libc::kill(pid, sig);
        if !TIMEOUT_FOREGROUND.load(Ordering::SeqCst) {
            // Also signal the whole process group we lead, ignoring it ourselves.
            libc::signal(sig, libc::SIG_IGN);
            libc::kill(0, sig);
            if sig != libc::SIGKILL && sig != libc::SIGCONT {
                libc::kill(pid, libc::SIGCONT);
                libc::signal(libc::SIGCONT, libc::SIG_IGN);
                libc::kill(0, libc::SIGCONT);
            }
        }
    }
}

pub fn run_timeout(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let usage = |msg: String| Ok(usage_error("timeout", msg, EXIT_CANCELED));
    let mut opts = GetOpt::new(args.into_iter().skip(1).collect(), &TIMEOUT_OPTS, false);
    let mut kill_after = 0f64;
    let mut term_signal = libc::SIGTERM;
    let mut verbose = false;
    let mut foreground = false;
    let mut preserve_status = false;
    while let Some(item) = opts.next_opt() {
        let (key, value) = match item {
            Ok(kv) => kv,
            Err(msg) => return usage(msg),
        };
        let value = value.unwrap_or_default();
        match key {
            "kill-after" => match parse_interval(&value) {
                Some(secs) => kill_after = secs,
                None => return usage(format!("invalid time interval {}", gnu_quote(&value))),
            },
            "signal" => match parse_signal(&value) {
                Some(sig) => term_signal = sig,
                None => return usage(format!("{}: invalid signal", gnu_quote(&value))),
            },
            "verbose" => verbose = true,
            "foreground" => foreground = true,
            "preserve-status" => preserve_status = true,
            "help" => return print_help(TIMEOUT_HELP),
            _ => {
                println!("timeout (GNU/Dogfish) {}", env!("CARGO_PKG_VERSION"));
                return Ok(0);
            }
        }
    }
    let operands = opts.into_operands();
    if operands.len() < 2 {
        return Ok(try_help("timeout", EXIT_CANCELED));
    }
    let Some(limit) = parse_interval(&operands[0]) else {
        return usage(format!("invalid time interval {}", gnu_quote(&operands[0])));
    };
    let command = &operands[1..];
    ctx.telemetry
        .set_strategy(format!("timeout {:?}", duration_from_secs(limit)));

    TIMEOUT_SIGNAL.store(term_signal, Ordering::SeqCst);
    TIMEOUT_KILL_AFTER_NS.store(duration_to_ns(kill_after), Ordering::SeqCst);
    TIMEOUT_FOREGROUND.store(foreground, Ordering::SeqCst);
    if verbose {
        let _ = TIMEOUT_VERBOSE.set((
            b"timeout: sending signal ".to_vec(),
            format!(" to command {}\n", gnu_quote(&command[0])).into_bytes(),
        ));
    }
    // SAFETY: process-wide signal setup before any child exists; the handler
    // only uses async-signal-safe operations.
    unsafe {
        if !foreground {
            // Lead a new process group so the signal reaches all descendants.
            libc::setpgid(0, 0);
        }
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = timeout_on_signal as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut action.sa_mask);
        for sig in [
            libc::SIGALRM,
            libc::SIGINT,
            libc::SIGQUIT,
            libc::SIGHUP,
            libc::SIGTERM,
            term_signal,
        ] {
            if sig != 0 && sig != libc::SIGKILL && sig != libc::SIGSTOP {
                libc::sigaction(sig, &action, std::ptr::null_mut());
            }
        }
        // Allow running in the background while the command reads the TTY.
        libc::signal(libc::SIGTTIN, libc::SIG_IGN);
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
    }

    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..]);
    // SAFETY: the closure runs in the forked child and only calls signal(2).
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGTTIN, libc::SIG_DFL);
            libc::signal(libc::SIGTTOU, libc::SIG_DFL);
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let status = if e.kind() == io::ErrorKind::NotFound {
                EXIT_ENOENT
            } else {
                EXIT_CANNOT_INVOKE
            };
            eprintln!(
                "timeout: failed to run command {}: {}",
                gnu_quote(&command[0]),
                gnu_io_error(&e)
            );
            return Ok(status);
        }
    };
    TIMEOUT_PID.store(child.id() as i32, Ordering::SeqCst);
    let limit_ns = duration_to_ns(limit);
    if limit_ns > 0 {
        arm_timer(limit_ns);
    }

    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("timeout: error waiting for command: {}", gnu_io_error(&e));
            return Ok(EXIT_CANCELED);
        }
    };
    let timed_out = TIMEOUT_TIMED_OUT.load(Ordering::SeqCst);
    let mut code = match (status.code(), status.signal()) {
        (Some(code), _) => code,
        (None, Some(sig)) => {
            if status.core_dumped() {
                eprintln!("timeout: the monitored command dumped core");
            }
            // SAFETY: getpid/getpgrp have no preconditions.
            let leader = unsafe { libc::getpid() == libc::getpgrp() };
            if !timed_out && leader {
                // Exit with the signal the command got, without dumping core.
                // SAFETY: resetting our own disposition and re-raising the signal.
                unsafe {
                    let no_core = libc::rlimit {
                        rlim_cur: 0,
                        rlim_max: 0,
                    };
                    libc::setrlimit(libc::RLIMIT_CORE, &no_core);
                    libc::signal(sig, libc::SIG_DFL);
                    let mut set: libc::sigset_t = std::mem::zeroed();
                    libc::sigemptyset(&mut set);
                    libc::sigaddset(&mut set, sig);
                    libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
                    libc::raise(sig);
                }
            }
            if timed_out && sig == libc::SIGKILL {
                preserve_status = true;
            }
            128 + sig
        }
        (None, None) => EXIT_CANCELED,
    };
    if timed_out && !preserve_status {
        code = 124;
    }
    Ok(code)
}

// ---------------------------------------------------------------------------
// 11. date — GNU date strings (`-d`), strftime with GNU flags, -I/-R/--rfc-3339
// ---------------------------------------------------------------------------
unsafe extern "C" {
    /// POSIX `tzset(3)`; the `libc` crate does not bind it on every Unix.
    fn tzset();
}

/// A point in time: seconds since the epoch plus nanoseconds (always >= 0).
#[derive(Clone, Copy)]
struct Stamp {
    sec: i64,
    ns: u32,
}

impl Stamp {
    fn now() -> Self {
        Self::from_system_time(SystemTime::now())
    }

    fn from_system_time(t: SystemTime) -> Self {
        match t.duration_since(UNIX_EPOCH) {
            Ok(d) => Self {
                sec: d.as_secs() as i64,
                ns: d.subsec_nanos(),
            },
            Err(e) => {
                let d = e.duration();
                let (sec, ns) = (-(d.as_secs() as i64), d.subsec_nanos());
                if ns == 0 {
                    Self { sec, ns: 0 }
                } else {
                    Self {
                        sec: sec - 1,
                        ns: 1_000_000_000 - ns,
                    }
                }
            }
        }
    }
}

/// Broken-down local time with the zone details strftime needs.
struct LocalTime {
    tm: libc::tm,
    zone: Vec<u8>,
}

fn local_time(t: i64) -> Option<LocalTime> {
    let tt = t as libc::time_t;
    // SAFETY: `tm` is a valid out-pointer; tm_zone, when set, points at static
    // zone-name storage that is copied before any other time call.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&tt, &mut tm).is_null() {
            return None;
        }
        let zone = if tm.tm_zone.is_null() {
            Vec::new()
        } else {
            CStr::from_ptr(tm.tm_zone).to_bytes().to_vec()
        };
        Some(LocalTime { tm, zone })
    }
}

fn utc_time(t: i64) -> Option<LocalTime> {
    let tt = t as libc::time_t;
    // SAFETY: `tm` is a valid out-pointer.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::gmtime_r(&tt, &mut tm).is_null() {
            return None;
        }
        Some(LocalTime {
            tm,
            zone: b"UTC".to_vec(),
        })
    }
}

/// Normalizes `tm` like `mktime` (local time) or `timegm` (`utc`) and returns
/// the time, or `None` if it is not representable.
fn make_time(tm: &mut libc::tm, utc: bool) -> Option<i64> {
    tm.tm_wday = -1;
    // SAFETY: `tm` is a valid, initialized struct tm.
    let t = unsafe {
        if utc {
            libc::timegm(tm)
        } else {
            libc::mktime(tm)
        }
    };
    // -1 is a valid time; mktime reports failure by leaving tm_wday alone.
    (tm.tm_wday >= 0).then_some(t as i64)
}

const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// Days from the start of `yday`'s ISO week-numbering year (gnulib's
/// `iso_week_days`); negative when the day belongs to the previous year.
fn iso_week_days(yday: i64, wday: i64) -> i64 {
    const BIG_MULTIPLE_OF_7: i64 = (366 / 7 + 2) * 7;
    yday - (yday - wday + 4 + BIG_MULTIPLE_OF_7) % 7 + 4 - 1
}

/// Renders `fmt` like GNU `date`'s strftime: the C-locale conversions plus
/// GNU's `%N`, `%q`, `%P`, `%s`, `%:z`.., and the `_ - 0 + ^ #` flags, field
/// widths and the `E`/`O` modifiers.
fn format_time(fmt: &[u8], lt: &LocalTime, ns: u32, epoch: i64, upcase: bool) -> Vec<u8> {
    let tm = &lt.tm;
    let year = i64::from(tm.tm_year) + 1900;
    let mut out = Vec::new();
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i] != b'%' {
            out.push(fmt[i]);
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        let mut pad: u8 = 0;
        let mut to_upper = upcase;
        let mut change_case = false;
        while let Some(&f) = fmt.get(i) {
            match f {
                b'_' | b'-' | b'+' | b'0' => pad = f,
                b'^' => to_upper = true,
                b'#' => change_case = true,
                _ => break,
            }
            i += 1;
        }
        let mut width: i64 = -1;
        if fmt.get(i).is_some_and(u8::is_ascii_digit) {
            width = 0;
            while let Some(d @ b'0'..=b'9') = fmt.get(i).copied() {
                width = (width * 10 + i64::from(d - b'0')).min(i32::MAX as i64);
                i += 1;
            }
        }
        let modifier = match fmt.get(i) {
            Some(&m @ (b'E' | b'O')) => {
                i += 1;
                m
            }
            _ => 0,
        };
        let mut colons = 0;
        while fmt.get(i + colons) == Some(&b':') {
            colons += 1;
        }
        let conv = fmt.get(i + colons).copied();
        // A conversion GNU does not know is copied through literally.
        let bad = |out: &mut Vec<u8>, end: usize| {
            // GNU pads the copied spec as if its leading '%' took no room.
            let spec = &fmt[start..end.min(fmt.len())];
            let width = if width < 0 { width } else { width + 1 };
            push_padded(out, spec, width, pad, false, false);
        };
        if colons > 0 && (conv != Some(b'z') || colons > 3) {
            i += colons;
            bad(&mut out, i + 1);
            i += 1;
            continue;
        }
        i += colons;
        let Some(conv) = conv else {
            // A trailing '%' spec (GNU extension) is copied, padded normally.
            push_padded(&mut out, &fmt[start..], width, pad, false, false);
            break;
        };
        i += 1;
        let e_bad = b"aAbhBdDeFgGHIjklmMNSUVwW";
        let o_bad = b"aAcDFqxXY";
        if (modifier == b'E' && e_bad.contains(&conv))
            || (modifier == b'O' && o_bad.contains(&conv))
        {
            bad(&mut out, i);
            continue;
        }
        let hour12 = if tm.tm_hour % 12 == 0 {
            12
        } else {
            tm.tm_hour % 12
        };
        let num = |out: &mut Vec<u8>, digits: i64, value: i64, space_pad: bool| {
            let pad = if space_pad && pad == 0 { b'_' } else { pad };
            push_number(out, digits, value, pad, width, false, 0);
        };
        let year_sign = |out: &mut Vec<u8>, digits: i64, value: i64| {
            let limit = if digits == 2 { 99 } else { 9999 };
            let sign = pad == b'+' && (value > limit || digits < width);
            push_number(out, digits, value, pad, width, sign, 0);
        };
        let text = |out: &mut Vec<u8>, s: &[u8], upper: bool, lower: bool| {
            push_padded(out, s, width, pad, upper, lower);
        };
        let sub = |out: &mut Vec<u8>, subfmt: &str, upper: bool| {
            let rendered = format_time(subfmt.as_bytes(), lt, ns, epoch, upper);
            push_padded(out, &rendered, width, pad, false, false);
        };
        match conv {
            b'%' => {
                if pad != 0 || width >= 0 || modifier != 0 || to_upper || change_case {
                    bad(&mut out, i);
                } else {
                    out.push(b'%');
                }
            }
            b'a' | b'A' => {
                let name = WEEKDAYS[tm.tm_wday.rem_euclid(7) as usize];
                let name = if conv == b'a' { &name[..3] } else { name };
                text(&mut out, name.as_bytes(), to_upper || change_case, false);
            }
            b'b' | b'h' | b'B' => {
                let name = MONTHS[tm.tm_mon.rem_euclid(12) as usize];
                let name = if conv == b'B' { name } else { &name[..3] };
                text(&mut out, name.as_bytes(), to_upper || change_case, false);
            }
            b'c' => sub(&mut out, "%a %b %e %H:%M:%S %Y", to_upper),
            b'C' => {
                let century = year.div_euclid(100);
                year_sign(&mut out, 2, century);
            }
            b'd' => num(&mut out, 2, tm.tm_mday.into(), false),
            b'D' | b'x' => sub(&mut out, "%m/%d/%y", to_upper),
            b'e' => num(&mut out, 2, tm.tm_mday.into(), true),
            b'F' => {
                let (year_pad, year_width) = if pad == 0 && width < 0 {
                    ('+', 4)
                } else {
                    (if pad == 0 { '0' } else { pad as char }, (width - 6).max(0))
                };
                let subfmt = format!("%{year_pad}{year_width}Y-%m-%d");
                let rendered = format_time(subfmt.as_bytes(), lt, ns, epoch, to_upper);
                out.extend_from_slice(&rendered);
            }
            b'g' | b'G' | b'V' => {
                let yday = i64::from(tm.tm_yday);
                let wday = i64::from(tm.tm_wday);
                let mut adjust = 0;
                let mut days = iso_week_days(yday, wday);
                if days < 0 {
                    adjust = -1;
                    days = iso_week_days(yday + 365 + i64::from(is_leap(year - 1)), wday);
                } else {
                    let d = iso_week_days(yday - (365 + i64::from(is_leap(year))), wday);
                    if d >= 0 {
                        adjust = 1;
                        days = d;
                    }
                }
                match conv {
                    b'g' => year_sign(&mut out, 2, (year + adjust).rem_euclid(100)),
                    b'G' => year_sign(&mut out, 4, year + adjust),
                    _ => num(&mut out, 2, days / 7 + 1, false),
                }
            }
            b'H' => num(&mut out, 2, tm.tm_hour.into(), false),
            b'I' => num(&mut out, 2, hour12.into(), false),
            b'j' => num(&mut out, 3, i64::from(tm.tm_yday) + 1, false),
            b'k' => num(&mut out, 2, tm.tm_hour.into(), true),
            b'l' => num(&mut out, 2, hour12.into(), true),
            b'm' => num(&mut out, 2, i64::from(tm.tm_mon) + 1, false),
            b'M' => num(&mut out, 2, tm.tm_min.into(), false),
            b'n' => text(&mut out, b"\n", false, false),
            b't' => text(&mut out, b"\t", false, false),
            b'N' => {
                let width = if width <= 0 { 9 } else { width };
                let mut n = ns;
                let mut ndigs = 9;
                while width < ndigs || (ndigs > 1 && n.is_multiple_of(10)) {
                    ndigs -= 1;
                    n /= 10;
                }
                let digits = format!("{:0w$}", n, w = ndigs as usize);
                out.extend_from_slice(digits.as_bytes());
                if pad != b'-' {
                    let fill = if pad == b'_' { b' ' } else { b'0' };
                    out.extend(std::iter::repeat_n(fill, (width - ndigs) as usize));
                }
            }
            b'p' | b'P' => {
                let ampm: &[u8] = if tm.tm_hour >= 12 { b"PM" } else { b"AM" };
                let lower = conv == b'P' || change_case;
                text(&mut out, ampm, to_upper && !lower, lower);
            }
            b'q' => num(&mut out, 1, i64::from(tm.tm_mon) / 3 + 1, false),
            b'r' => sub(&mut out, "%I:%M:%S %p", to_upper),
            b'R' => sub(&mut out, "%H:%M", to_upper),
            b's' => push_number(&mut out, 1, epoch, pad, width, false, 0),
            b'S' => num(&mut out, 2, tm.tm_sec.into(), false),
            b'T' | b'X' => sub(&mut out, "%H:%M:%S", to_upper),
            b'u' => num(&mut out, 1, i64::from((tm.tm_wday + 6) % 7 + 1), false),
            b'U' => num(
                &mut out,
                2,
                i64::from((tm.tm_yday - tm.tm_wday + 7) / 7),
                false,
            ),
            b'w' => num(&mut out, 1, tm.tm_wday.into(), false),
            b'W' => num(
                &mut out,
                2,
                i64::from((tm.tm_yday - (tm.tm_wday + 6) % 7 + 7) / 7),
                false,
            ),
            b'y' => num(&mut out, 2, year.rem_euclid(100), false),
            b'Y' => year_sign(&mut out, 4, year),
            b'z' => {
                // `c_long` is narrower than i64 on 32-bit targets.
                #[allow(clippy::useless_conversion)]
                let off = i64::from(tm.tm_gmtoff);
                let negative = off < 0;
                let a = off.unsigned_abs() as i64;
                let (h, m, s) = (a / 3600, a / 60 % 60, a % 60);
                let colons = match colons {
                    3 if s != 0 => 2,
                    3 if m != 0 => 1,
                    c => c,
                };
                let (digits, value, mask) = match colons {
                    0 => (5, h * 100 + m, 0),
                    1 => (6, h * 100 + m, 0o4),
                    2 => (9, h * 10000 + m * 100 + s, 0o24),
                    _ => (3, h, 0),
                };
                let value = if negative { -value } else { value };
                push_number(&mut out, digits, value, pad, width, true, mask);
            }
            b'Z' => {
                let lower = change_case;
                text(&mut out, &lt.zone, to_upper && !lower, lower);
            }
            _ => bad(&mut out, i),
        }
    }
    out
}

/// Appends `s` padded on the left to `width` (zeros for the `0`/`+` flags,
/// spaces otherwise; no padding with `-`), optionally case-converted.
fn push_padded(out: &mut Vec<u8>, s: &[u8], width: i64, pad: u8, upper: bool, lower: bool) {
    if pad != b'-' && width > s.len() as i64 {
        let fill = if pad == b'0' || pad == b'+' {
            b'0'
        } else {
            b' '
        };
        out.extend(std::iter::repeat_n(fill, width as usize - s.len()));
    }
    out.extend(s.iter().map(|c| {
        if lower {
            c.to_ascii_lowercase()
        } else if upper {
            c.to_ascii_uppercase()
        } else {
            *c
        }
    }));
}

/// gnulib nstrftime's `do_number`: renders `value` with `digits` default
/// width, the pad flag, an optional forced sign, and colons inserted where
/// `colon_mask` bits are set (for `%:z`).
fn push_number(
    out: &mut Vec<u8>,
    digits: i64,
    value: i64,
    pad: u8,
    width: i64,
    always_sign: bool,
    colon_mask: u32,
) {
    let negative = value < 0;
    let mut v = value.unsigned_abs();
    let mut mask = colon_mask;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        if mask & 1 != 0 {
            buf.push(b':');
        }
        mask >>= 1;
        buf.push(b'0' + (v % 10) as u8);
        v /= 10;
        if v == 0 && mask == 0 {
            break;
        }
    }
    buf.reverse();
    let pad = if pad == 0 { b'0' } else { pad };
    let mut width = if width < 0 { digits } else { width };
    let sign = if negative {
        Some(b'-')
    } else if always_sign {
        Some(b'+')
    } else {
        None
    };
    if let Some(sign) = sign {
        let shortage = width - 1 - buf.len() as i64;
        if pad == b'_' && shortage > 0 {
            out.extend(std::iter::repeat_n(b' ', shortage as usize));
            width -= shortage;
        }
        out.push(sign);
        width -= 1;
    }
    push_padded(out, &buf, width, pad, false, false);
}

// --- date string parsing (a port of the essentials of gnulib parse-datetime) ---

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Unit {
    Year,
    Month,
    /// Days, with a multiplier (week = 7, fortnight = 14).
    Day(i64),
    Hour,
    Minute,
    Second,
}

#[derive(Clone, PartialEq, Debug)]
enum DateTok {
    /// Unsigned / signed integers, with their digit counts.
    UNum(i64, usize),
    SNum(i64, usize),
    /// Decimal seconds `S.FFF` (signed when written with a sign).
    Decimal(i64, i64, bool),
    Month(i32),
    Weekday(i32),
    Meridian(bool),
    Unit(Unit),
    Ordinal(i64),
    DayShift(i64),
    Ago(i64),
    Zone(i64),
    DayZone(i64),
    Dst,
    T,
    Char(u8),
}

const ZONE_TABLE: &[(&str, i64, bool)] = &[
    ("GMT", 0, false),
    ("UT", 0, false),
    ("UTC", 0, false),
    ("WET", 0, false),
    ("WEST", 0, true),
    ("BST", 0, true),
    ("ART", -3 * 3600, false),
    ("BRT", -3 * 3600, false),
    ("BRST", -3 * 3600, true),
    ("NST", -(3 * 3600 + 1800), false),
    ("NDT", -(3 * 3600 + 1800), true),
    ("AST", -4 * 3600, false),
    ("ADT", -4 * 3600, true),
    ("CLT", -4 * 3600, false),
    ("CLST", -4 * 3600, true),
    ("EST", -5 * 3600, false),
    ("EDT", -5 * 3600, true),
    ("CST", -6 * 3600, false),
    ("CDT", -6 * 3600, true),
    ("MST", -7 * 3600, false),
    ("MDT", -7 * 3600, true),
    ("PST", -8 * 3600, false),
    ("PDT", -8 * 3600, true),
    ("AKST", -9 * 3600, false),
    ("AKDT", -9 * 3600, true),
    ("HST", -10 * 3600, false),
    ("HAST", -10 * 3600, false),
    ("HADT", -10 * 3600, true),
    ("SST", -12 * 3600, false),
    ("WAT", 3600, false),
    ("CET", 3600, false),
    ("CEST", 3600, true),
    ("MET", 3600, false),
    ("MEZ", 3600, false),
    ("MEST", 3600, true),
    ("MESZ", 3600, true),
    ("EET", 2 * 3600, false),
    ("EEST", 2 * 3600, true),
    ("CAT", 2 * 3600, false),
    ("SAST", 2 * 3600, false),
    ("EAT", 3 * 3600, false),
    ("MSK", 3 * 3600, false),
    ("MSD", 3 * 3600, true),
    ("IST", 5 * 3600 + 1800, false),
    ("SGT", 8 * 3600, false),
    ("KST", 9 * 3600, false),
    ("JST", 9 * 3600, false),
    ("GST", 10 * 3600, false),
    ("NZST", 12 * 3600, false),
    ("NZDT", 12 * 3600, true),
];

fn lookup_zone(word: &str) -> Option<DateTok> {
    ZONE_TABLE
        .iter()
        .find(|(name, _, _)| *name == word)
        .map(|&(_, off, dst)| {
            if dst {
                DateTok::DayZone(off)
            } else {
                DateTok::Zone(off)
            }
        })
}

fn lookup_date_word(word: &str) -> Option<DateTok> {
    match word {
        "AM" | "A.M." => return Some(DateTok::Meridian(false)),
        "PM" | "P.M." => return Some(DateTok::Meridian(true)),
        _ => {}
    }
    let abbrev = word.len() == 3 || (word.len() == 4 && word.ends_with('.'));
    let matches = |full: &str| {
        let full = full.to_ascii_uppercase();
        if abbrev {
            full.starts_with(&word[..3])
        } else {
            full == word
        }
    };
    if let Some(m) = MONTHS.iter().position(|m| matches(m)) {
        return Some(DateTok::Month(m as i32 + 1));
    }
    if word == "SEPT" {
        return Some(DateTok::Month(9));
    }
    if let Some(d) = WEEKDAYS.iter().position(|d| matches(d)) {
        return Some(DateTok::Weekday(d as i32));
    }
    match word {
        "TUES" => return Some(DateTok::Weekday(2)),
        "WEDNES" => return Some(DateTok::Weekday(3)),
        "THUR" | "THURS" => return Some(DateTok::Weekday(4)),
        _ => {}
    }
    if let Some(z) = lookup_zone(word) {
        return Some(z);
    }
    if word == "DST" {
        return Some(DateTok::Dst);
    }
    let unit = |w: &str| match w {
        "YEAR" => Some(Unit::Year),
        "MONTH" => Some(Unit::Month),
        "FORTNIGHT" => Some(Unit::Day(14)),
        "WEEK" => Some(Unit::Day(7)),
        "DAY" => Some(Unit::Day(1)),
        "HOUR" => Some(Unit::Hour),
        "MINUTE" | "MIN" => Some(Unit::Minute),
        "SECOND" | "SEC" => Some(Unit::Second),
        _ => None,
    };
    if let Some(u) = unit(word).or_else(|| unit(word.strip_suffix('S')?)) {
        return Some(DateTok::Unit(u));
    }
    let relative = match word {
        "TOMORROW" => Some(DateTok::DayShift(1)),
        "YESTERDAY" => Some(DateTok::DayShift(-1)),
        "TODAY" | "NOW" => Some(DateTok::DayShift(0)),
        "LAST" => Some(DateTok::Ordinal(-1)),
        "THIS" => Some(DateTok::Ordinal(0)),
        "NEXT" | "FIRST" => Some(DateTok::Ordinal(1)),
        "THIRD" => Some(DateTok::Ordinal(3)),
        "FOURTH" => Some(DateTok::Ordinal(4)),
        "FIFTH" => Some(DateTok::Ordinal(5)),
        "SIXTH" => Some(DateTok::Ordinal(6)),
        "SEVENTH" => Some(DateTok::Ordinal(7)),
        "EIGHTH" => Some(DateTok::Ordinal(8)),
        "NINTH" => Some(DateTok::Ordinal(9)),
        "TENTH" => Some(DateTok::Ordinal(10)),
        "ELEVENTH" => Some(DateTok::Ordinal(11)),
        "TWELFTH" => Some(DateTok::Ordinal(12)),
        "AGO" => Some(DateTok::Ago(-1)),
        "HENCE" => Some(DateTok::Ago(1)),
        _ => None,
    };
    if relative.is_some() {
        return relative;
    }
    if let [c] = word.as_bytes() {
        // Military time zones; "T" doubles as the ISO 8601 date/time separator.
        let hours = match c {
            b'A'..=b'I' => i64::from(c - b'A') + 1,
            b'K'..=b'M' => i64::from(c - b'K') + 10,
            b'N'..=b'Y' => -(i64::from(c - b'N') + 1),
            b'Z' => 0,
            _ => return None,
        };
        return Some(if *c == b'T' {
            DateTok::T
        } else {
            DateTok::Zone(hours * 3600)
        });
    }
    if word.contains('.') {
        return lookup_zone(&word.replace('.', ""));
    }
    None
}

fn lex_date(s: &str) -> Option<Vec<DateTok>> {
    let b = s.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() || c == b'-' || c == b'+' {
            let mut sign = 0i64;
            if c == b'-' || c == b'+' {
                sign = if c == b'-' { -1 } else { 1 };
                i += 1;
                while b.get(i).is_some_and(u8::is_ascii_whitespace) {
                    i += 1;
                }
                if !b.get(i).is_some_and(u8::is_ascii_digit) {
                    // A sign not followed by a number is ignored.
                    continue;
                }
            }
            let start = i;
            let mut value: i64 = 0;
            while let Some(&d) = b.get(i).filter(|d| d.is_ascii_digit()) {
                value = value.checked_mul(10)?.checked_add(i64::from(d - b'0'))?;
                i += 1;
            }
            let digits = i - start;
            if matches!(b.get(i), Some(b'.' | b',')) && b.get(i + 1).is_some_and(u8::is_ascii_digit)
            {
                i += 1;
                let mut ns = 0i64;
                let mut places = 0;
                while let Some(&d) = b.get(i).filter(|d| d.is_ascii_digit()) {
                    if places < 9 {
                        ns = ns * 10 + i64::from(d - b'0');
                        places += 1;
                    }
                    i += 1;
                }
                ns *= 10i64.pow(9 - places);
                let (mut sec, mut ns) = (value, ns);
                if sign < 0 {
                    sec = -sec;
                    if ns != 0 {
                        sec -= 1;
                        ns = 1_000_000_000 - ns;
                    }
                }
                toks.push(DateTok::Decimal(sec, ns, sign != 0));
            } else if sign == 0 {
                toks.push(DateTok::UNum(value, digits));
            } else {
                toks.push(DateTok::SNum(sign * value, digits));
            }
            continue;
        }
        if c.is_ascii_alphabetic() {
            let start = i;
            while b
                .get(i)
                .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'.')
            {
                i += 1;
            }
            let word = s[start..i].to_ascii_uppercase();
            toks.push(lookup_date_word(&word)?);
            continue;
        }
        if c == b'(' {
            let mut depth = 0;
            while i < b.len() {
                match b[i] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                i += 1;
                if depth == 0 {
                    break;
                }
            }
            continue;
        }
        if !c.is_ascii() {
            return None;
        }
        toks.push(DateTok::Char(c));
        i += 1;
    }
    Some(toks)
}

/// Parser state, mirroring gnulib's `parser_control`.
#[derive(Default)]
struct DateParse {
    year: i64,
    year_digits: usize,
    month: i64,
    day: i64,
    hour: i64,
    minutes: i64,
    seconds: i64,
    ns: i64,
    /// None = 24-hour clock; Some(true) = PM.
    meridian: Option<bool>,
    day_ordinal: i64,
    day_number: i64,
    time_zone: i64,
    local_isdst: i32,
    rel_year: i64,
    rel_month: i64,
    rel_day: i64,
    rel_hour: i64,
    rel_minutes: i64,
    rel_seconds: i64,
    rel_ns: i64,
    times_seen: u32,
    dates_seen: u32,
    days_seen: u32,
    zones_seen: u32,
    dsts_seen: u32,
    rels_seen: bool,
}

impl DateParse {
    fn apply_rel(&mut self, unit: Unit, n: i64, ns: i64) -> Option<()> {
        let add = |field: &mut i64, v: i64| -> Option<()> {
            *field = field.checked_add(v)?;
            Some(())
        };
        match unit {
            Unit::Year => add(&mut self.rel_year, n)?,
            Unit::Month => add(&mut self.rel_month, n)?,
            Unit::Day(mult) => add(&mut self.rel_day, n.checked_mul(mult)?)?,
            Unit::Hour => add(&mut self.rel_hour, n)?,
            Unit::Minute => add(&mut self.rel_minutes, n)?,
            Unit::Second => {
                add(&mut self.rel_seconds, n)?;
                add(&mut self.rel_ns, ns)?;
            }
        }
        self.rels_seen = true;
        Some(())
    }

    fn set_time(&mut self, h: i64, m: i64, s: i64, ns: i64, meridian: Option<bool>) {
        self.hour = h;
        self.minutes = m;
        self.seconds = s;
        self.ns = ns;
        self.meridian = meridian;
    }

    /// A bare number: a year, a YYYYMMDD date, or an HHMM time.
    fn digits_to_date_time(&mut self, value: i64, digits: usize) {
        if self.dates_seen > 0
            && self.year_digits == 0
            && !self.rels_seen
            && (self.times_seen > 0 || digits > 2)
        {
            self.year = value;
            self.year_digits = digits;
        } else if digits > 4 {
            self.dates_seen += 1;
            self.day = value % 100;
            self.month = (value / 100) % 100;
            self.year = value / 10000;
            self.year_digits = digits - 4;
        } else {
            self.times_seen += 1;
            if digits <= 2 {
                self.set_time(value, 0, 0, 0, None);
            } else {
                self.set_time(value / 100, value % 100, 0, 0, None);
            }
        }
    }

    /// `[+-]hh[mm]` or `[+-]hh:mm` as a UTC offset in seconds.
    fn zone_hhmm(&mut self, value: i64, digits: usize, minutes: Option<i64>) -> Option<()> {
        let n_minutes = match minutes {
            None => {
                let v = if digits <= 2 { value * 100 } else { value };
                (v / 100) * 60 + v % 100
            }
            Some(mm) => value * 60 + if value < 0 { -mm } else { mm },
        };
        if n_minutes.abs() > 24 * 60 {
            return None;
        }
        self.time_zone = n_minutes * 60;
        Some(())
    }
}

/// A cursor over date tokens, with the grammar's productions as methods.
struct DateParser<'a> {
    toks: &'a [DateTok],
    pos: usize,
    pc: DateParse,
}

impl DateParser<'_> {
    fn peek(&self, k: usize) -> Option<&DateTok> {
        self.toks.get(self.pos + k)
    }

    fn next_if(&mut self, f: impl Fn(&DateTok) -> bool) -> Option<DateTok> {
        let t = self.peek(0).filter(|t| f(t))?.clone();
        self.pos += 1;
        Some(t)
    }

    fn is_char(t: &DateTok, c: u8) -> bool {
        *t == DateTok::Char(c)
    }

    /// `: mm` after a zone offset, if present.
    fn colon_minutes(&mut self) -> Option<Option<i64>> {
        if self.peek(0).is_some_and(|t| Self::is_char(t, b':')) {
            if let Some(DateTok::UNum(mm, _)) = self.peek(1).cloned() {
                self.pos += 2;
                return Some(Some(mm));
            }
            return None;
        }
        Some(None)
    }

    fn zone_offset(&mut self) -> Option<bool> {
        let Some(DateTok::SNum(v, digits)) = self.peek(0).cloned() else {
            return Some(false);
        };
        self.pos += 1;
        let mm = self.colon_minutes()?;
        self.pc.zones_seen += 1;
        self.pc.zone_hhmm(v, digits, mm)?;
        Some(true)
    }

    /// After `hh:mm`: optional `:ss[.fff]`, then a meridian or a zone offset.
    fn finish_time(&mut self, h: i64, m: i64, iso_only: bool) -> Option<()> {
        let (mut s, mut ns) = (0, 0);
        if self.peek(0).is_some_and(|t| Self::is_char(t, b':')) {
            match self.peek(1).cloned() {
                Some(DateTok::UNum(v, _)) => (s, ns) = (v, 0),
                Some(DateTok::Decimal(v, f, false)) => (s, ns) = (v, f),
                _ => return None,
            }
            self.pos += 2;
        }
        if !iso_only && let Some(DateTok::Meridian(pm)) = self.peek(0).cloned() {
            self.pos += 1;
            self.pc.set_time(h, m, s, ns, Some(pm));
            return Some(());
        }
        self.pc.set_time(h, m, s, ns, None);
        self.zone_offset()?;
        Some(())
    }

    fn unit_after(&self, k: usize) -> Option<Unit> {
        match self.peek(k) {
            Some(DateTok::Unit(u)) => Some(*u),
            _ => None,
        }
    }

    /// `relunit [ago]` where the multiplier (and fraction) are already known.
    fn rel(&mut self, unit: Unit, n: i64, ns: i64) -> Option<()> {
        let sign = match self.peek(0) {
            Some(DateTok::Ago(s)) => {
                let s = *s;
                self.pos += 1;
                s
            }
            _ => 1,
        };
        self.pc.apply_rel(unit, n.checked_mul(sign)?, ns * sign)
    }

    fn item(&mut self) -> Option<()> {
        let tok = self.peek(0)?.clone();
        self.pos += 1;
        match tok {
            DateTok::UNum(n, digits) => self.number_item(n, digits),
            DateTok::SNum(n, _) => {
                let unit = self.unit_after(0)?;
                self.pos += 1;
                self.rel(unit, n, 0)
            }
            DateTok::Decimal(s, ns, _) => {
                if self.unit_after(0) != Some(Unit::Second) {
                    return None;
                }
                self.pos += 1;
                self.rel(Unit::Second, s, ns)
            }
            DateTok::Month(m) => {
                self.pc.dates_seen += 1;
                self.pc.month = m.into();
                match (self.peek(0).cloned(), self.peek(1).cloned()) {
                    (Some(DateTok::SNum(d, _)), Some(DateTok::SNum(y, yd))) => {
                        self.pos += 2;
                        self.pc.day = -d;
                        self.pc.year = -y;
                        self.pc.year_digits = yd;
                    }
                    (Some(DateTok::UNum(d, _)), next) => {
                        self.pos += 1;
                        self.pc.day = d;
                        if next.is_some_and(|t| Self::is_char(&t, b','))
                            && let Some(DateTok::UNum(y, yd)) = self.peek(1).cloned()
                        {
                            self.pos += 2;
                            self.pc.year = y;
                            self.pc.year_digits = yd;
                        }
                    }
                    _ => return None,
                }
                Some(())
            }
            DateTok::Weekday(d) => {
                self.pc.days_seen += 1;
                self.pc.day_ordinal = 0;
                self.pc.day_number = d.into();
                self.next_if(|t| Self::is_char(t, b','));
                Some(())
            }
            DateTok::Ordinal(n) => match self.peek(0).cloned()? {
                DateTok::Weekday(d) => {
                    self.pos += 1;
                    self.pc.days_seen += 1;
                    self.pc.day_ordinal = n;
                    self.pc.day_number = d.into();
                    Some(())
                }
                DateTok::Unit(u) => {
                    self.pos += 1;
                    self.rel(u, n, 0)
                }
                _ => None,
            },
            DateTok::Unit(u) => self.rel(u, 1, 0),
            DateTok::DayShift(n) => self.pc.apply_rel(Unit::Day(1), n, 0),
            DateTok::Zone(off) => {
                self.pc.zones_seen += 1;
                self.pc.time_zone = off;
                self.zone_suffix(off)
            }
            DateTok::DayZone(off) => {
                self.pc.zones_seen += 1;
                self.pc.time_zone = off + 3600;
                Some(())
            }
            _ => None,
        }
    }

    /// What may follow a zone name: `DST`, a relative offset, or `+hh[:mm]`.
    fn zone_suffix(&mut self, off: i64) -> Option<()> {
        match self.peek(0).cloned() {
            Some(DateTok::Dst) => {
                self.pos += 1;
                self.pc.time_zone = off + 3600;
            }
            Some(DateTok::SNum(n, digits)) => {
                self.pos += 1;
                if let Some(unit) = self.unit_after(0) {
                    self.pos += 1;
                    self.pc.apply_rel(unit, n, 0)?;
                } else {
                    let mm = self.colon_minutes()?;
                    self.pc.zone_hhmm(n, digits, mm)?;
                    self.pc.time_zone = self.pc.time_zone.checked_add(off)?;
                }
            }
            _ => {}
        }
        Some(())
    }

    fn number_item(&mut self, n: i64, digits: usize) -> Option<()> {
        let next = self.peek(0).cloned();
        match next {
            Some(DateTok::Char(b':')) => {
                let Some(DateTok::UNum(m, _)) = self.peek(1).cloned() else {
                    return None;
                };
                self.pos += 2;
                self.pc.times_seen += 1;
                self.finish_time(n, m, false)
            }
            Some(DateTok::Meridian(pm)) => {
                self.pos += 1;
                self.pc.times_seen += 1;
                self.pc.set_time(n, 0, 0, 0, Some(pm));
                Some(())
            }
            Some(DateTok::Char(b'/')) => {
                let Some(DateTok::UNum(second, _)) = self.peek(1).cloned() else {
                    return None;
                };
                self.pos += 2;
                self.pc.dates_seen += 1;
                if self.peek(0).is_some_and(|t| Self::is_char(t, b'/')) {
                    let Some(DateTok::UNum(third, third_digits)) = self.peek(1).cloned() else {
                        return None;
                    };
                    self.pos += 2;
                    if digits >= 4 {
                        (self.pc.year, self.pc.year_digits) = (n, digits);
                        (self.pc.month, self.pc.day) = (second, third);
                    } else {
                        (self.pc.month, self.pc.day) = (n, second);
                        (self.pc.year, self.pc.year_digits) = (third, third_digits);
                    }
                } else {
                    (self.pc.month, self.pc.day) = (n, second);
                }
                Some(())
            }
            Some(DateTok::Month(m)) => {
                self.pos += 1;
                self.pc.dates_seen += 1;
                self.pc.day = n;
                self.pc.month = m.into();
                match self.peek(0).cloned() {
                    Some(DateTok::SNum(y, yd)) => {
                        self.pos += 1;
                        (self.pc.year, self.pc.year_digits) = (-y, yd);
                    }
                    Some(DateTok::UNum(y, yd)) => {
                        self.pos += 1;
                        (self.pc.year, self.pc.year_digits) = (y, yd);
                    }
                    _ => {}
                }
                Some(())
            }
            Some(DateTok::SNum(m, _)) => {
                if let Some(unit) = self.unit_after(1) {
                    // "YYYYMMDD +N days": a number, then a relative offset.
                    self.pos += 2;
                    self.pc.digits_to_date_time(n, digits);
                    return self.pc.apply_rel(unit, m, 0);
                }
                if let Some(DateTok::SNum(d, _)) = self.peek(1).cloned() {
                    // ISO 8601 YYYY-MM-DD, optionally followed by Thh:mm[:ss].
                    self.pos += 2;
                    self.pc.dates_seen += 1;
                    (self.pc.year, self.pc.year_digits) = (n, digits);
                    (self.pc.month, self.pc.day) = (-m, -d);
                    if self.peek(0) == Some(&DateTok::T)
                        && let Some(DateTok::UNum(h, _)) = self.peek(1).cloned()
                    {
                        self.pos += 2;
                        self.pc.times_seen += 1;
                        return self.iso_time(h);
                    }
                    return Some(());
                }
                // hh followed by a zone offset.
                self.pc.times_seen += 1;
                self.pc.set_time(n, 0, 0, 0, None);
                self.zone_offset()?;
                Some(())
            }
            Some(DateTok::Weekday(d)) => {
                self.pos += 1;
                self.pc.days_seen += 1;
                self.pc.day_ordinal = n;
                self.pc.day_number = d.into();
                Some(())
            }
            Some(DateTok::Unit(u)) => {
                self.pos += 1;
                self.rel(u, n, 0)
            }
            _ => {
                self.pc.digits_to_date_time(n, digits);
                Some(())
            }
        }
    }

    /// The time after an ISO 8601 `T`: `hh:mm[:ss]` or `hh` plus a zone offset.
    fn iso_time(&mut self, h: i64) -> Option<()> {
        if self.peek(0).is_some_and(|t| Self::is_char(t, b':')) {
            let Some(DateTok::UNum(m, _)) = self.peek(1).cloned() else {
                return None;
            };
            self.pos += 2;
            return self.finish_time(h, m, true);
        }
        self.pc.set_time(h, 0, 0, 0, None);
        self.zone_offset()?.then_some(())
    }
}

/// Parses a GNU date string relative to `now`; `None` means "invalid date".
fn parse_date(input: &str, now: Stamp) -> Option<Stamp> {
    let toks = lex_date(input)?;
    if let Some(DateTok::Char(b'@')) = toks.first() {
        return match toks[1..] {
            [DateTok::UNum(s, _)] | [DateTok::SNum(s, _)] => Some(Stamp { sec: s, ns: 0 }),
            [DateTok::Decimal(s, ns, _)] => Some(Stamp {
                sec: s,
                ns: ns as u32,
            }),
            _ => None,
        };
    }
    let now_tm = local_time(now.sec)?.tm;
    let mut parser = DateParser {
        toks: &toks,
        pos: 0,
        pc: DateParse {
            year: i64::from(now_tm.tm_year) + 1900,
            month: i64::from(now_tm.tm_mon) + 1,
            day: now_tm.tm_mday.into(),
            hour: now_tm.tm_hour.into(),
            minutes: now_tm.tm_min.into(),
            seconds: now_tm.tm_sec.into(),
            ns: now.ns.into(),
            local_isdst: now_tm.tm_isdst,
            ..DateParse::default()
        },
    };
    while parser.pos < toks.len() {
        parser.item()?;
    }
    let pc = parser.pc;
    if [
        pc.times_seen,
        pc.dates_seen,
        pc.days_seen,
        pc.dsts_seen,
        pc.zones_seen,
    ]
    .iter()
    .any(|&n| n > 1)
    {
        return None;
    }
    let year = if pc.year_digits == 2 && pc.year >= 0 {
        pc.year + if pc.year < 69 { 2000 } else { 1900 }
    } else {
        pc.year
    };
    let mut tm: libc::tm = now_tm;
    let narrow = |v: i64| -> Option<libc::c_int> { v.try_into().ok() };
    tm.tm_year = narrow(year.checked_sub(1900)?)?;
    tm.tm_mon = narrow(pc.month - 1)?;
    tm.tm_mday = narrow(pc.day)?;
    let mut ns = pc.ns;
    if pc.times_seen > 0 || (pc.rels_seen && pc.dates_seen == 0 && pc.days_seen == 0) {
        let hour = match pc.meridian {
            None => (0..=23).contains(&pc.hour).then_some(pc.hour),
            Some(pm) => (1..=12)
                .contains(&pc.hour)
                .then_some(pc.hour % 12 + if pm { 12 } else { 0 }),
        }?;
        tm.tm_hour = narrow(hour)?;
        tm.tm_min = narrow(pc.minutes)?;
        tm.tm_sec = narrow(pc.seconds)?;
    } else {
        tm.tm_hour = 0;
        tm.tm_min = 0;
        tm.tm_sec = 0;
        ns = 0;
    }
    tm.tm_isdst = if pc.dates_seen + pc.days_seen + pc.times_seen > 0 {
        -1
    } else {
        pc.local_isdst
    };
    // With an explicit zone, the fields are wall-clock time in that zone.
    let utc = pc.zones_seen > 0;
    let tm0 = tm;
    let mut start = make_time(&mut tm, utc)?;
    let same = |a: &libc::tm, b: &libc::tm| {
        (
            a.tm_sec, a.tm_min, a.tm_hour, a.tm_mday, a.tm_mon, a.tm_year,
        ) == (
            b.tm_sec, b.tm_min, b.tm_hour, b.tm_mday, b.tm_mon, b.tm_year,
        )
    };
    if !same(&tm0, &tm) {
        return None;
    }
    if pc.days_seen > 0 && pc.dates_seen == 0 {
        let wday = i64::from(tm.tm_wday);
        let ordinal_weeks = pc.day_ordinal - i64::from(pc.day_ordinal > 0 && wday != pc.day_number);
        let incr = ordinal_weeks * 7 + (pc.day_number - wday + 7).rem_euclid(7);
        tm.tm_mday = narrow(i64::from(tm.tm_mday) + incr)?;
        tm.tm_isdst = -1;
        start = make_time(&mut tm, utc)?;
    }
    if pc.rel_year != 0 || pc.rel_month != 0 || pc.rel_day != 0 {
        tm.tm_year = narrow(i64::from(tm.tm_year) + pc.rel_year)?;
        tm.tm_mon = narrow(i64::from(tm.tm_mon) + pc.rel_month)?;
        tm.tm_mday = narrow(i64::from(tm.tm_mday) + pc.rel_day)?;
        tm.tm_hour = tm0.tm_hour;
        tm.tm_min = tm0.tm_min;
        tm.tm_sec = tm0.tm_sec;
        tm.tm_isdst = tm0.tm_isdst;
        start = make_time(&mut tm, utc)?;
    }
    if utc {
        start = start.checked_sub(pc.time_zone)?;
    }
    let sum_ns = ns + pc.rel_ns;
    let norm_ns = sum_ns.rem_euclid(1_000_000_000);
    let carry = (sum_ns - norm_ns) / 1_000_000_000;
    let sec = start
        .checked_add(pc.rel_hour.checked_mul(3600)?)?
        .checked_add(pc.rel_minutes.checked_mul(60)?)?
        .checked_add(pc.rel_seconds)?
        .checked_add(carry)?;
    Some(Stamp {
        sec,
        ns: norm_ns as u32,
    })
}

const DATE_HELP: &str = "\
Usage: date [OPTION]... [+FORMAT]
Display date and time in the given FORMAT.

  -d, --date=STRING          display time described by STRING, not 'now'
  -f, --file=DATEFILE        like --date; once for each line of DATEFILE
  -I[FMT], --iso-8601[=FMT]  output date/time in ISO 8601 format.
                               FMT='date' for date only (the default),
                               'hours', 'minutes', 'seconds', or 'ns'
                               for date and time to the indicated precision.
  -R, --rfc-email            output date and time in RFC 5322 format.
      --rfc-3339=FMT         output date/time in RFC 3339 format.
                               FMT='date', 'seconds', or 'ns'
  -r, --reference=FILE       display the last modification time of FILE
  -u, --utc, --universal     print Coordinated Universal Time (UTC)
  -J, --json                 print the time as a JSON object
      --help        display this help and exit
      --version     output version information and exit

FORMAT controls the output, with the usual strftime conversions (%a %b %c
%d %F %H %j %m %M %N %s %S %T %u %V %Y %z %Z ...) and the flags
'-' (no padding), '_' (pad with spaces), '0' (pad with zeros),
'+' (pad with zeros, sign years), '^' (upper case) and '#' (opposite case).
";

static DATE_OPTS: [OptSpec; 15] = [
    opt(Some('d'), Some("date"), HasArg::Required, "date"),
    opt(Some('f'), Some("file"), HasArg::Required, "file"),
    opt(Some('I'), Some("iso-8601"), HasArg::Optional, "iso-8601"),
    opt(Some('r'), Some("reference"), HasArg::Required, "reference"),
    opt(Some('R'), Some("rfc-email"), HasArg::No, "rfc-email"),
    opt(None, Some("rfc-822"), HasArg::No, "rfc-email"),
    opt(None, Some("rfc-2822"), HasArg::No, "rfc-email"),
    opt(None, Some("rfc-3339"), HasArg::Required, "rfc-3339"),
    opt(Some('s'), Some("set"), HasArg::Required, "set"),
    opt(Some('u'), Some("utc"), HasArg::No, "utc"),
    opt(None, Some("universal"), HasArg::No, "utc"),
    opt(None, Some("debug"), HasArg::No, "debug"),
    opt(Some('J'), Some("json"), HasArg::No, "json"),
    opt(None, Some("help"), HasArg::No, "help"),
    opt(None, Some("version"), HasArg::No, "version"),
];

const ISO_8601_FORMATS: [(&str, &str); 5] = [
    ("hours", "%Y-%m-%dT%H%:z"),
    ("minutes", "%Y-%m-%dT%H:%M%:z"),
    ("date", "%Y-%m-%d"),
    ("seconds", "%Y-%m-%dT%H:%M:%S%:z"),
    ("ns", "%Y-%m-%dT%H:%M:%S,%N%:z"),
];

const RFC_3339_FORMATS: [(&str, &str); 3] = [
    ("date", "%Y-%m-%d"),
    ("seconds", "%Y-%m-%d %H:%M:%S%:z"),
    ("ns", "%Y-%m-%d %H:%M:%S.%N%:z"),
];

/// Formats one time with `fmt`, or reports it as out of range.
fn render_date(stamp: Stamp, fmt: &str) -> Option<Vec<u8>> {
    let lt = local_time(stamp.sec)?;
    let mut line = format_time(fmt.as_bytes(), &lt, stamp.ns, stamp.sec, false);
    line.push(b'\n');
    Some(line)
}

pub fn run_date(args: Vec<String>, _ctx: &DogfishContext) -> Result<i32> {
    let usage = |msg: String| Ok(usage_error("date", msg, 1));
    let mut opts = GetOpt::new(args.into_iter().skip(1).collect(), &DATE_OPTS, true);
    let mut date_str: Option<String> = None;
    let mut date_file: Option<String> = None;
    let mut reference: Option<String> = None;
    let mut set_str: Option<String> = None;
    let mut formats: Vec<String> = Vec::new();
    let mut utc = false;
    let mut json = false;
    while let Some(item) = opts.next_opt() {
        let (key, value) = match item {
            Ok(kv) => kv,
            Err(msg) => return usage(msg),
        };
        match key {
            "date" => date_str = value,
            "file" => date_file = value,
            "reference" => reference = value,
            "set" => set_str = value,
            "utc" => utc = true,
            "json" => json = true,
            "debug" => {}
            "rfc-email" => formats.push("%a, %d %b %Y %H:%M:%S %z".to_string()),
            "iso-8601" => {
                let names = ISO_8601_FORMATS.map(|(n, _)| n);
                let spec = value.as_deref().unwrap_or("date");
                match argmatch("date", "--iso-8601", spec, &names, 1) {
                    Ok(i) => formats.push(ISO_8601_FORMATS[i].1.to_string()),
                    Err(status) => return Ok(status),
                }
            }
            "rfc-3339" => {
                let names = RFC_3339_FORMATS.map(|(n, _)| n);
                let spec = value.as_deref().unwrap_or_default();
                match argmatch("date", "--rfc-3339", spec, &names, 1) {
                    Ok(i) => formats.push(RFC_3339_FORMATS[i].1.to_string()),
                    Err(status) => return Ok(status),
                }
            }
            "help" => return print_help(DATE_HELP),
            _ => {
                println!("date (GNU/Dogfish) {}", env!("CARGO_PKG_VERSION"));
                return Ok(0);
            }
        }
    }
    let operands = opts.into_operands();
    let sources = [&date_str, &date_file, &reference]
        .iter()
        .filter(|s| s.is_some())
        .count();
    if sources > 1 {
        return usage("the options to specify dates for printing are mutually exclusive".into());
    }
    if set_str.is_some() && sources > 0 {
        return usage("the options to print and set the time may not be used together".into());
    }
    if operands.len() > 1 {
        return usage(format!("extra operand {}", gnu_quote(&operands[1])));
    }
    if let Some(op) = operands.first() {
        match op.strip_prefix('+') {
            Some(f) => formats.push(f.to_string()),
            None if sources > 0 || set_str.is_some() => {
                return usage(format!(
                    "the argument {} lacks a leading '+';\n\
                     when using an option to specify date(s), any non-option\n\
                     argument must be a format string beginning with '+'",
                    gnu_quote(op)
                ));
            }
            None => set_str = Some(op.clone()),
        }
    }
    if formats.len() > 1 {
        eprintln!("date: multiple output formats specified");
        return Ok(1);
    }
    if set_str.is_some() {
        eprintln!("date: setting the system clock is not supported");
        return Ok(1);
    }
    if utc {
        // SAFETY: date is single-threaded; GNU likewise switches TZ to UTC0
        // before parsing so that -d strings are read as UTC too.
        unsafe {
            std::env::set_var("TZ", "UTC0");
        }
    }
    // SAFETY: tzset only re-reads TZ into libc's zone state.
    unsafe { tzset() };
    let fmt = formats
        .pop()
        .unwrap_or_else(|| "%a %b %e %H:%M:%S %Z %Y".to_string());

    let now = Stamp::now();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let emit = |out: &mut io::StdoutLock<'_>, stamp: Stamp| -> Result<bool> {
        if json {
            let iso = |s: Stamp| {
                render_date(s, "%Y-%m-%dT%H:%M:%S%:z")
                    .map(|v| String::from_utf8_lossy(v.trim_ascii_end()).into_owned())
            };
            let obj = serde_json::json!({
                "unix": stamp.sec,
                "unix_ms": stamp.sec.saturating_mul(1000) + i64::from(stamp.ns / 1_000_000),
                "utc": utc_time(stamp.sec).map(|lt| {
                    let s = format_time(b"%Y-%m-%dT%H:%M:%S%:z", &lt, stamp.ns, stamp.sec, false);
                    String::from_utf8_lossy(&s).into_owned()
                }),
                "local": iso(stamp),
                "format": render_date(stamp, &fmt)
                    .map(|v| String::from_utf8_lossy(v.trim_ascii_end()).into_owned()),
            });
            writeln!(out, "{}", serde_json::to_string_pretty(&obj)?)?;
            return Ok(true);
        }
        match render_date(stamp, &fmt) {
            Some(line) => {
                ignore_broken_pipe(out.write_all(&line))?;
                Ok(true)
            }
            None => {
                eprintln!("date: time {} is out of range", stamp.sec);
                Ok(false)
            }
        }
    };

    if let Some(path) = &date_file {
        let content = if path == "-" {
            let mut buf = Vec::new();
            io::stdin().read_to_end(&mut buf).map(|_| buf)
        } else {
            std::fs::read(path)
        };
        let content = match content {
            Ok(c) => c,
            Err(e) => {
                eprintln!("date: {}: {}", gnu_quote_file(path), gnu_io_error(&e));
                return Ok(1);
            }
        };
        let mut status = 0;
        let text = String::from_utf8_lossy(&content);
        let body = text.strip_suffix('\n').unwrap_or(&text);
        for line in body.split('\n').filter(|_| !content.is_empty()) {
            match parse_date(line, now) {
                Some(stamp) => {
                    if !emit(&mut out, stamp)? {
                        status = 1;
                    }
                }
                None => {
                    eprintln!("date: invalid date {}", gnu_quote(line));
                    status = 1;
                }
            }
        }
        return Ok(status);
    }

    let stamp = if let Some(path) = &reference {
        match std::fs::metadata(path).and_then(|m| m.modified()) {
            Ok(t) => Stamp::from_system_time(t),
            Err(e) => {
                eprintln!("date: {}: {}", gnu_quote_file(path), gnu_io_error(&e));
                return Ok(1);
            }
        }
    } else if let Some(d) = &date_str {
        match parse_date(d, now) {
            Some(s) => s,
            None => {
                eprintln!("date: invalid date {}", gnu_quote(d));
                return Ok(1);
            }
        }
    } else {
        now
    };
    Ok(if emit(&mut out, stamp)? { 0 } else { 1 })
}

// ---------------------------------------------------------------------------
// 12. tee — copy standard input to standard output and files
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "tee",
    about = "Copy standard input to each FILE, and also to standard output",
    disable_help_flag = true,
    infer_long_args = true
)]
struct TeeArgs {
    /// Append to the given FILEs, do not overwrite
    #[arg(short = 'a', long = "append")]
    append: bool,

    /// Ignore interrupt signals
    #[arg(short = 'i', long = "ignore-interrupts")]
    ignore_interrupts: bool,

    /// Operate in a more appropriate MODE with pipes (same as --output-error=warn-nopipe)
    #[arg(short = 'p')]
    nopipe: bool,

    /// Set behavior on write error: warn, warn-nopipe, exit, exit-nopipe
    #[arg(
        long = "output-error",
        value_name = "MODE",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "warn-nopipe"
    )]
    output_error: Option<String>,

    /// Display this help and exit
    #[arg(long, action = ArgAction::Help)]
    help: Option<bool>,

    /// Output file paths
    files: Vec<String>,
}

/// GNU tee's `--output-error` modes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputError {
    /// No option: SIGPIPE kills tee, other errors are diagnosed.
    Sigpipe,
    Warn,
    WarnNopipe,
    Exit,
    ExitNopipe,
}

/// One destination of `tee`; `None` once it has failed and been dropped.
struct TeeOutput {
    name: String,
    sink: Option<Box<dyn Write>>,
}

pub fn run_tee(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = TeeArgs::try_parse_from(args)?;
    let mut mode = if parsed.nopipe {
        OutputError::WarnNopipe
    } else {
        OutputError::Sigpipe
    };
    if let Some(m) = &parsed.output_error {
        let modes = ["warn", "warn-nopipe", "exit", "exit-nopipe"];
        mode = match argmatch("tee", "--output-error", m, &modes, 1) {
            Ok(0) => OutputError::Warn,
            Ok(1) => OutputError::WarnNopipe,
            Ok(2) => OutputError::Exit,
            Ok(_) => OutputError::ExitNopipe,
            Err(status) => return Ok(status),
        };
    }
    // SAFETY: changing this process's signal dispositions before any I/O.
    unsafe {
        if parsed.ignore_interrupts {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
        }
        if mode != OutputError::Sigpipe {
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        }
    }

    let mut status = 0;
    let mut outputs = vec![TeeOutput {
        name: "standard output".to_string(),
        sink: Some(Box::new(io::stdout())),
    }];
    for path in &parsed.files {
        let opened = OpenOptions::new()
            .create(true)
            .write(true)
            .append(parsed.append)
            .truncate(!parsed.append)
            .open(path);
        match opened {
            Ok(f) => outputs.push(TeeOutput {
                name: path.clone(),
                sink: Some(Box::new(f)),
            }),
            Err(e) => {
                eprintln!("tee: {}: {}", gnu_quote_file(path), gnu_io_error(&e));
                status = 1;
            }
        }
    }

    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut buf = vec![0u8; DEFAULT_BUF_SIZE];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("tee: read error: {}", gnu_io_error(&e));
                status = 1;
                break;
            }
        };
        ctx.telemetry.add_read(n as u64);
        for out in &mut outputs {
            let Some(sink) = out.sink.as_mut() else {
                continue;
            };
            // Unbuffered like GNU: every chunk reaches every output at once.
            let res = sink.write_all(&buf[..n]).and_then(|()| sink.flush());
            match res {
                Ok(()) => ctx.telemetry.add_written(n as u64),
                Err(e) => {
                    out.sink = None;
                    let pipe = e.kind() == io::ErrorKind::BrokenPipe;
                    let quiet =
                        pipe && matches!(mode, OutputError::WarnNopipe | OutputError::ExitNopipe);
                    if !quiet {
                        eprintln!("tee: {}: {}", gnu_quote_file(&out.name), gnu_io_error(&e));
                        status = 1;
                        if matches!(mode, OutputError::Exit | OutputError::ExitNopipe) {
                            return Ok(1);
                        }
                    }
                }
            }
        }
        if outputs.iter().all(|o| o.sink.is_none()) {
            break;
        }
    }
    Ok(status)
}

// ---------------------------------------------------------------------------
// 17. watch — Execute a program periodically, showing fullscreen output with diff/timer
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "watch",
    about = "Execute a program periodically, showing output fullscreen"
)]
struct WatchArgs {
    /// Specify update interval (e.g. 2s, 500ms, or 1.5)
    #[arg(short = 'n', long = "interval", default_value = "2s")]
    interval: String,

    ///Iterations limit (useful for scripts/testing; 0 = infinite)
    #[arg(short = 'c', long = "count", default_value_t = 0)]
    count: usize,

    #[arg(required = true, trailing_var_arg = true)]
    command: Vec<String>,
}

pub fn run_watch(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = WatchArgs::try_parse_from(args)?;
    let interval = parse_interval(&parsed.interval)
        .map(duration_from_secs)
        .ok_or_else(|| anyhow!("invalid interval {}", gnu_quote(&parsed.interval)))?;
    let cmd_str = parsed.command.join(" ");
    let st = ctx.style();
    let mut iter = 0usize;

    loop {
        iter += 1;
        let start = std::time::Instant::now();
        let output = Command::new("sh").arg("-c").arg(&cmd_str).output()?;
        let elapsed = start.elapsed();

        if ctx.stdout_is_tty {
            print!("\x1b[2J\x1b[H"); // Clear screen & home cursor
        }
        let now = Stamp::now();
        let clock = local_time(now.sec)
            .map(|lt| format_time(b"%H:%M:%S", &lt, now.ns, now.sec, false))
            .unwrap_or_default();
        println!(
            "{} {}   {}   {}",
            st.bold_cyan(&format!("Every {:?}:", interval)),
            st.bold(&cmd_str),
            st.dim(&format!(
                "[last run: {}]",
                dogfish_core::format_duration_human(elapsed)
            )),
            st.yellow(&String::from_utf8_lossy(&clock))
        );
        println!("{}", st.dim(&"─".repeat(72)));
        io::stdout().write_all(&output.stdout)?;
        io::stderr().write_all(&output.stderr)?;
        io::stdout().flush()?;

        if parsed.count > 0 && iter >= parsed.count {
            break;
        }
        thread::sleep(interval);
    }
    Ok(0)
}
