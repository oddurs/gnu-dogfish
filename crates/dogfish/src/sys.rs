use std::ffi::CStr;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Local, Utc};
use clap::Parser;
use dogfish_core::{
    ColumnAlign, DEFAULT_BUF_SIZE, DogfishContext, Table, io::ignore_broken_pipe,
    parse_duration_str,
};
use serde::Serialize;

// ---------------------------------------------------------------------------
// 1. true & false
// ---------------------------------------------------------------------------
pub fn run_true(_args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    ctx.telemetry.set_strategy("immediate exit(0)");
    Ok(0)
}

pub fn run_false(_args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    ctx.telemetry.set_strategy("immediate exit(1)");
    Ok(1)
}

// ---------------------------------------------------------------------------
// 2. yes — Page-aligned 64 KiB vector I/O
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "yes",
    about = "Repeatedly output a line with all specified STRING(s), or 'y'"
)]
struct YesArgs {
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
pub fn run_echo(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
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

    /// Parses an integer argument like GNU (`'c` char codes, 0x hex, 0 octal).
    fn int_arg(&mut self, s: Option<&str>) -> i128 {
        let Some(raw) = s else { return 0 };
        let t = raw.trim_start();
        let b = t.as_bytes();
        if let [b'\'' | b'"', rest @ ..] = b {
            return std::str::from_utf8(rest)
                .ok()
                .and_then(|r| r.chars().next())
                .map(|c| c as i128)
                .unwrap_or_else(|| rest.first().copied().unwrap_or(0) as i128);
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
        let value = i128::from_str_radix(&digits[..valid], radix).unwrap_or(0);
        let value = if neg { -value } else { value };
        if valid == 0 && !(radix == 8 && digits.is_empty()) {
            eprintln!("printf: '{}': expected a numeric value", raw);
            self.failed = true;
        } else if valid < digits.len() {
            eprintln!("printf: '{}': value not completely converted", raw);
            self.failed = true;
        }
        value
    }

    fn float_arg(&mut self, s: Option<&str>) -> f64 {
        let Some(raw) = s else { return 0.0 };
        let t = raw.trim();
        if let [b'\'' | b'"', rest @ ..] = t.as_bytes() {
            return rest.first().copied().unwrap_or(0) as f64;
        }
        match t.parse::<f64>() {
            Ok(v) => v,
            Err(_) => {
                // Accept the longest numeric prefix, reporting the rest.
                let prefix_len = (1..=t.len())
                    .rev()
                    .find(|&n| t.is_char_boundary(n) && t[..n].parse::<f64>().is_ok())
                    .unwrap_or(0);
                if prefix_len == 0 {
                    eprintln!("printf: '{}': expected a numeric value", raw);
                } else {
                    eprintln!("printf: '{}': value not completely converted", raw);
                }
                self.failed = true;
                t[..prefix_len].parse().unwrap_or(0.0)
            }
        }
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
            let w = run.int_arg(w);
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
                let p = run.int_arg(p);
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
        while matches!(
            fmt.get(i),
            Some(b'h' | b'l' | b'L' | b'q' | b'j' | b'z' | b't')
        ) {
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
                let v = run.int_arg(a);
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

pub fn run_printf(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    if args.len() < 2 {
        eprintln!("printf: missing operand\nTry 'printf --help' for more information.");
        return Ok(1);
    }
    let fmt = args[1].as_bytes();
    let mut run = PrintfRun {
        args: &args[2..],
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
        if run.next >= run.args.len() || run.next == before {
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
// 5. sleep — Supports human durations (`500ms`, `1.5s`, `1m30s`)
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "sleep",
    about = "Pause for NUMBER seconds or human duration (e.g. 500ms, 1m30s)"
)]
struct SleepArgs {
    #[arg(required = true)]
    durations: Vec<String>,
}

pub fn run_sleep(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = SleepArgs::try_parse_from(args)?;
    let mut total = Duration::ZERO;
    for d in &parsed.durations {
        total += parse_duration_str(d).map_err(|e| anyhow!(e))?;
    }
    ctx.telemetry.set_strategy(format!("sleep {:?}", total));
    thread::sleep(total);
    Ok(0)
}

// ---------------------------------------------------------------------------
// 6. seq — Fast buffered numeric sequence generator
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "seq", about = "Print a sequence of numbers")]
#[command(allow_hyphen_values = true)]
struct SeqArgs {
    /// Use STRING to separate numbers (default: \n)
    #[arg(short = 's', long = "separator", default_value = "\n")]
    separator: String,

    /// Equalize width by padding with leading zeroes
    #[arg(short = 'w', long = "equal-width")]
    equal_width: bool,

    /// Emit JSON array
    #[arg(short = 'J', long = "json")]
    json: bool,

    #[arg(required = true)]
    nums: Vec<String>,
}

pub fn run_seq(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = SeqArgs::try_parse_from(args)?;
    let (first, step, last) = match parsed.nums.len() {
        1 => (1.0, 1.0, parsed.nums[0].parse::<f64>()?),
        2 => (
            parsed.nums[0].parse::<f64>()?,
            1.0,
            parsed.nums[1].parse::<f64>()?,
        ),
        3 => (
            parsed.nums[0].parse::<f64>()?,
            parsed.nums[1].parse::<f64>()?,
            parsed.nums[2].parse::<f64>()?,
        ),
        _ => return Err(anyhow!("seq expects 1, 2, or 3 numeric arguments")),
    };

    if step == 0.0 {
        return Err(anyhow!("seq: increment must not be zero"));
    }

    let is_int = parsed
        .nums
        .iter()
        .all(|n| !n.contains('.') && n.parse::<i64>().is_ok());

    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());

    if parsed.json {
        let mut vals = Vec::new();
        let mut i = 0_i64;
        loop {
            let v = first + (i as f64) * step;
            if (step > 0.0 && v > last + 1e-12) || (step < 0.0 && v < last - 1e-12) {
                break;
            }
            vals.push(if is_int {
                serde_json::json!(v.round() as i64)
            } else {
                serde_json::json!(v)
            });
            i += 1;
        }
        serde_json::to_writer(&mut out, &vals)?;
        writeln!(out)?;
        out.flush()?;
        return Ok(0);
    }

    if is_int {
        let mut cur = first as i64;
        let inc = step as i64;
        let end = last as i64;
        let width = if parsed.equal_width {
            parsed.nums.iter().map(|s| s.len()).max().unwrap_or(0)
        } else {
            0
        };
        let mut first_item = true;
        let mut itoa_buf = itoa::Buffer::new();
        let sep_bytes = parsed.separator.as_bytes();
        while (inc > 0 && cur <= end) || (inc < 0 && cur >= end) {
            if !first_item {
                ignore_broken_pipe(out.write_all(sep_bytes))?;
            }
            first_item = false;
            let res = if width > 0 {
                write!(out, "{:0width$}", cur, width = width)
            } else {
                out.write_all(itoa_buf.format(cur).as_bytes())
            };
            if let Err(e) = res {
                if e.kind() == io::ErrorKind::BrokenPipe {
                    return Ok(0);
                }
                return Err(e.into());
            }
            ctx.telemetry.add_items(1);
            cur += inc;
        }
        if !first_item {
            ignore_broken_pipe(out.write_all(b"\n"))?;
        }
    } else {
        let prec = parsed
            .nums
            .iter()
            .filter_map(|s| s.split('.').nth(1).map(|p| p.len()))
            .max()
            .unwrap_or(0);
        let mut i = 0_i64;
        let mut first_item = true;
        loop {
            let v = first + (i as f64) * step;
            if (step > 0.0 && v > last + 1e-9) || (step < 0.0 && v < last - 1e-9) {
                break;
            }
            if !first_item {
                ignore_broken_pipe(out.write_all(parsed.separator.as_bytes()))?;
            }
            first_item = false;
            let res = write!(out, "{:.prec$}", v, prec = prec);
            if let Err(e) = res {
                if e.kind() == io::ErrorKind::BrokenPipe {
                    return Ok(0);
                }
                return Err(e.into());
            }
            ctx.telemetry.add_items(1);
            i += 1;
        }
        if !first_item {
            ignore_broken_pipe(out.write_all(b"\n"))?;
        }
    }

    ignore_broken_pipe(out.flush())?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// 7. whoami
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "whoami", about = "Print effective user name")]
struct WhoamiArgs {
    #[arg(short = 'J', long = "json")]
    json: bool,
}

pub fn run_whoami(args: Vec<String>, _ctx: &DogfishContext) -> Result<i32> {
    let parsed = WhoamiArgs::try_parse_from(args)?;
    let uid = unsafe { libc::geteuid() };
    let username = get_username_by_uid(uid).unwrap_or_else(|| uid.to_string());
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
#[command(name = "id", about = "Print user and group identity information")]
struct IdArgs {
    /// Print only the effective user ID
    #[arg(short = 'u', long = "user")]
    user: bool,

    /// Print only the effective group ID
    #[arg(short = 'g', long = "group")]
    group: bool,

    /// Print all group IDs
    #[arg(short = 'G', long = "groups")]
    groups: bool,

    /// Print a name instead of a number (with -ugG)
    #[arg(short = 'n', long = "name")]
    name: bool,

    /// Emit structured JSON
    #[arg(short = 'J', long = "json")]
    json: bool,
}

#[derive(Serialize)]
struct IdRecord {
    uid: u32,
    user: String,
    gid: u32,
    group: String,
    groups: Vec<(u32, String)>,
}

pub fn run_id(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = IdArgs::try_parse_from(args)?;
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    let user = get_username_by_uid(uid).unwrap_or_else(|| uid.to_string());
    let group = get_groupname_by_gid(gid).unwrap_or_else(|| gid.to_string());

    let mut gids = vec![0 as libc::gid_t; 64];
    let ngroups = unsafe { libc::getgroups(gids.len() as i32, gids.as_mut_ptr()) };
    if ngroups >= 0 {
        gids.truncate(ngroups as usize);
    } else {
        gids.clear();
    }
    if !gids.contains(&gid) {
        gids.insert(0, gid);
    }

    let groups_info: Vec<(u32, String)> = gids
        .into_iter()
        .map(|g| {
            let gname = get_groupname_by_gid(g).unwrap_or_else(|| g.to_string());
            (g, gname)
        })
        .collect();

    if parsed.json {
        let rec = IdRecord {
            uid,
            user,
            gid,
            group,
            groups: groups_info,
        };
        println!("{}", serde_json::to_string_pretty(&rec)?);
        return Ok(0);
    }

    if parsed.user {
        if parsed.name {
            println!("{}", user);
        } else {
            println!("{}", uid);
        }
        return Ok(0);
    }

    if parsed.group {
        if parsed.name {
            println!("{}", group);
        } else {
            println!("{}", gid);
        }
        return Ok(0);
    }

    if parsed.groups {
        let parts: Vec<String> = groups_info
            .iter()
            .map(|(g, gn)| {
                if parsed.name {
                    gn.clone()
                } else {
                    g.to_string()
                }
            })
            .collect();
        println!("{}", parts.join(" "));
        return Ok(0);
    }

    let st = ctx.style();
    let grp_strs: Vec<String> = groups_info
        .iter()
        .map(|(g, gn)| format!("{}({})", st.bold_cyan(&g.to_string()), st.green(gn)))
        .collect();
    println!(
        "uid={}({}) gid={}({}) groups={}",
        st.bold_cyan(&uid.to_string()),
        st.bold_green(&user),
        st.bold_cyan(&gid.to_string()),
        st.bold_green(&group),
        grp_strs.join(",")
    );
    Ok(0)
}

// ---------------------------------------------------------------------------
// 9. uname — With `--json` and visual card mode
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "uname", about = "Print system information")]
struct UnameArgs {
    /// Print all information
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
    /// Emit structured JSON
    #[arg(short = 'J', long = "json")]
    json: bool,
}

#[derive(Serialize)]
struct UnameInfo {
    sysname: String,
    nodename: String,
    release: String,
    version: String,
    machine: String,
}

pub fn run_uname(args: Vec<String>, _ctx: &DogfishContext) -> Result<i32> {
    let parsed = UnameArgs::try_parse_from(args)?;
    let info = get_uname_info()?;

    if parsed.json {
        println!("{}", serde_json::to_string_pretty(&info)?);
        return Ok(0);
    }

    let any_specific = parsed.all
        || parsed.sysname
        || parsed.nodename
        || parsed.release
        || parsed.version
        || parsed.machine;

    let mut parts = Vec::new();
    if !any_specific || parsed.all || parsed.sysname {
        parts.push(info.sysname);
    }
    if parsed.all || parsed.nodename {
        parts.push(info.nodename);
    }
    if parsed.all || parsed.release {
        parts.push(info.release);
    }
    if parsed.all || parsed.version {
        parts.push(info.version);
    }
    if parsed.all || parsed.machine {
        parts.push(info.machine);
    }

    println!("{}", parts.join(" "));
    Ok(0)
}

fn get_uname_info() -> Result<UnameInfo> {
    unsafe {
        let mut uts: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut uts) != 0 {
            return Err(io::Error::last_os_error().into());
        }
        let cstr = |ptr: *const libc::c_char| -> String {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        };
        Ok(UnameInfo {
            sysname: cstr(uts.sysname.as_ptr()),
            nodename: cstr(uts.nodename.as_ptr()),
            release: cstr(uts.release.as_ptr()),
            version: cstr(uts.version.as_ptr()),
            machine: cstr(uts.machine.as_ptr()),
        })
    }
}

// ---------------------------------------------------------------------------
// 10. env — Supports NDJSON / table view, `-0`, `-i`, `-u`, and command execution
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "env",
    about = "Run a program in a modified environment, or inspect environment variables"
)]
struct EnvArgs {
    /// Start with an empty environment
    #[arg(short = 'i', long = "ignore-environment")]
    ignore_env: bool,

    /// Remove variable from the environment
    #[arg(short = 'u', long = "unset")]
    unset: Vec<String>,

    /// End each output line with NUL, not newline
    #[arg(short = '0', long = "null")]
    null: bool,

    /// Emit JSON / NDJSON objects (`{"key": ..., "value": ...}`)
    #[arg(short = 'J', long = "json")]
    json: bool,

    /// Pretty table view on TTY
    #[arg(short = 't', long = "table")]
    table: bool,

    /// KEY=VALUE assignments followed by optional COMMAND [ARGS]...
    #[arg(trailing_var_arg = true)]
    rest: Vec<String>,
}

#[derive(Serialize)]
struct EnvRecord {
    key: String,
    value: String,
}

pub fn run_env(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = EnvArgs::try_parse_from(args)?;
    let mut env_map: std::collections::BTreeMap<String, String> = if parsed.ignore_env {
        std::collections::BTreeMap::new()
    } else {
        std::env::vars().collect()
    };

    for u in &parsed.unset {
        env_map.remove(u);
    }

    let mut cmd_start = None;
    for (i, item) in parsed.rest.iter().enumerate() {
        if let Some((k, v)) = item.split_once('=') {
            if !k.is_empty() {
                env_map.insert(k.to_string(), v.to_string());
                continue;
            }
        }
        cmd_start = Some(i);
        break;
    }

    if let Some(idx) = cmd_start {
        let cmd_name = &parsed.rest[idx];
        let cmd_args = &parsed.rest[idx + 1..];
        let status = Command::new(cmd_name)
            .args(cmd_args)
            .env_clear()
            .envs(&env_map)
            .status()
            .with_context(|| format!("failed to execute '{}'", cmd_name))?;
        return Ok(status.code().unwrap_or(1));
    }

    let records: Vec<EnvRecord> = env_map
        .into_iter()
        .map(|(key, value)| EnvRecord { key, value })
        .collect();

    if parsed.json {
        dogfish_core::emit_objects_or_table(&records, ctx, true, false)?;
        return Ok(0);
    }

    if parsed.table {
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
        return Ok(0);
    }

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    let term = if parsed.null { "\0" } else { "\n" };
    for r in &records {
        write!(out, "{}={}{}", r.key, r.value, term)?;
    }
    out.flush()?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// 11. date — Supports `--iso-8601`, `--rfc-3339`, `--utc`, `-r`, `--json`, `+FORMAT`
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "date", about = "Display or format the system date and time")]
struct DateArgs {
    /// Print or set Coordinated Universal Time (UTC)
    #[arg(short = 'u', long = "utc", visible_alias = "universal")]
    utc: bool,

    /// Output ISO 8601 format
    #[arg(short = 'I', long = "iso-8601")]
    iso_8601: bool,

    /// Output RFC 2822 / email format
    #[arg(short = 'R', long = "rfc-email")]
    rfc_email: bool,

    /// Display time described by unix timestamp `@SECONDS` or ISO string
    #[arg(short = 'd', long = "date")]
    date_str: Option<String>,

    /// Emit structured JSON with unix, iso8601, utc, and local representations
    #[arg(short = 'J', long = "json")]
    json: bool,

    /// Optional `+FORMAT` string (e.g. `+%Y-%m-%d %H:%M:%S`)
    format: Option<String>,
}

pub fn run_date(args: Vec<String>, _ctx: &DogfishContext) -> Result<i32> {
    let parsed = DateArgs::try_parse_from(args)?;
    let dt_utc: DateTime<Utc> = if let Some(ref d) = parsed.date_str {
        if let Some(epoch_str) = d.strip_prefix('@') {
            let secs: i64 = epoch_str.parse().context("invalid @epoch timestamp")?;
            DateTime::from_timestamp(secs, 0).ok_or_else(|| anyhow!("timestamp out of range"))?
        } else if let Ok(dt) = DateTime::parse_from_rfc3339(d) {
            dt.with_timezone(&Utc)
        } else {
            return Err(anyhow!(
                "unsupported -d date format '{}'; use @EPOCH or RFC3339",
                d
            ));
        }
    } else {
        Utc::now()
    };

    let dt_local: DateTime<Local> = dt_utc.with_timezone(&Local);

    if parsed.json {
        let obj = serde_json::json!({
            "unix": dt_utc.timestamp(),
            "unix_ms": dt_utc.timestamp_millis(),
            "utc": dt_utc.to_rfc3339(),
            "local": dt_local.to_rfc3339(),
        });
        println!("{}", serde_json::to_string_pretty(&obj)?);
        return Ok(0);
    }

    let fmt_str = if let Some(ref f) = parsed.format {
        f.strip_prefix('+').unwrap_or(f)
    } else if parsed.iso_8601 {
        "%Y-%m-%dT%H:%M:%S%:z"
    } else if parsed.rfc_email {
        "%a, %d %b %Y %H:%M:%S %z"
    } else {
        "%a %b %e %H:%M:%S %Z %Y"
    };

    let formatted = if parsed.utc {
        dt_utc.format(fmt_str).to_string()
    } else {
        dt_local.format(fmt_str).to_string()
    };
    println!("{}", formatted);
    Ok(0)
}

// ---------------------------------------------------------------------------
// 12. tee — Fast multi-file fanout writer
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "tee",
    about = "Read from standard input and write to standard output and files"
)]
struct TeeArgs {
    /// Append to the given FILEs, do not overwrite
    #[arg(short = 'a', long = "append")]
    append: bool,

    /// Output file paths
    files: Vec<String>,
}

pub fn run_tee(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = TeeArgs::try_parse_from(args)?;
    let mut outputs: Vec<std::fs::File> = Vec::with_capacity(parsed.files.len());

    for path in &parsed.files {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .append(parsed.append)
            .truncate(!parsed.append)
            .open(path)
            .with_context(|| format!("tee: cannot open '{}'", path))?;
        outputs.push(file);
    }

    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut buf = vec![0u8; DEFAULT_BUF_SIZE];

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.telemetry.add_read(n as u64);
        ignore_broken_pipe(out.write_all(&buf[..n]))?;
        for f in &mut outputs {
            f.write_all(&buf[..n])?;
        }
        ctx.telemetry.add_written((n * (outputs.len() + 1)) as u64);
    }
    ignore_broken_pipe(out.flush())?;
    for f in &mut outputs {
        f.flush()?;
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// 13. nproc — Print number of available processing units
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "nproc",
    about = "Print the number of processing units available"
)]
struct NprocArgs {
    /// Ignore up to N processing units
    #[arg(long = "ignore", default_value_t = 0)]
    ignore: usize,

    #[arg(short = 'J', long = "json")]
    json: bool,
}

pub fn run_nproc(args: Vec<String>, _ctx: &DogfishContext) -> Result<i32> {
    let parsed = NprocArgs::try_parse_from(args)?;
    let total = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let count = total.saturating_sub(parsed.ignore).max(1);
    if parsed.json {
        println!(
            "{}",
            serde_json::json!({ "available_cpus": count, "total_cpus": total })
        );
    } else {
        println!("{}", count);
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// 14. hostname — Print system network hostname
// ---------------------------------------------------------------------------
pub fn run_hostname(_args: Vec<String>, _ctx: &DogfishContext) -> Result<i32> {
    let info = get_uname_info()?;
    println!("{}", info.nodename);
    Ok(0)
}

// ---------------------------------------------------------------------------
// 15. printenv — Print values of all or specified environment variables
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "printenv",
    about = "Print the values of the specified environment VARIABLE(s)"
)]
struct PrintenvArgs {
    #[arg(short = '0', long = "null")]
    null: bool,

    #[arg(short = 'J', long = "json")]
    json: bool,

    vars: Vec<String>,
}

pub fn run_printenv(args: Vec<String>, _ctx: &DogfishContext) -> Result<i32> {
    let parsed = PrintenvArgs::try_parse_from(args)?;
    let term = if parsed.null { "\0" } else { "\n" };

    if parsed.vars.is_empty() {
        let map: std::collections::BTreeMap<String, String> = std::env::vars().collect();
        if parsed.json {
            println!("{}", serde_json::to_string_pretty(&map)?);
        } else {
            for (k, v) in map {
                print!("{}={}{}", k, v, term);
            }
        }
        return Ok(0);
    }

    let mut exit_code = 0;
    for v in &parsed.vars {
        match std::env::var(v) {
            Ok(val) => print!("{}{}", val, term),
            Err(_) => exit_code = 1,
        }
    }
    Ok(exit_code)
}

// ---------------------------------------------------------------------------
// 16. timeout — Run a command with a time limit (e.g. `timeout 500ms slow_cmd`)
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "timeout",
    about = "Start COMMAND, and kill it if still running after DURATION"
)]
struct TimeoutArgs {
    /// Duration before killing command (e.g. 5s, 500ms, 1m)
    duration: String,

    #[arg(required = true, trailing_var_arg = true)]
    command: Vec<String>,
}

pub fn run_timeout(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = TimeoutArgs::try_parse_from(args)?;
    let limit = parse_duration_str(&parsed.duration).map_err(|e| anyhow!(e))?;
    let cmd_name = &parsed.command[0];
    let cmd_args = &parsed.command[1..];

    ctx.telemetry.set_strategy(format!("timeout {:?}", limit));
    let mut child = Command::new(cmd_name)
        .args(cmd_args)
        .spawn()
        .with_context(|| format!("timeout: failed to run '{}'", cmd_name))?;

    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.code().unwrap_or(1));
        }
        if start.elapsed() >= limit {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(124); // Standard GNU timeout exit code
        }
        thread::sleep(Duration::from_millis(5));
    }
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
    let interval = parse_duration_str(&parsed.interval).map_err(|e| anyhow!(e))?;
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
        let now = Local::now().format("%H:%M:%S").to_string();
        println!(
            "{} {}   {}   {}",
            st.bold_cyan(&format!("Every {:?}:", interval)),
            st.bold(&cmd_str),
            st.dim(&format!(
                "[last run: {}]",
                dogfish_core::format_duration_human(elapsed)
            )),
            st.yellow(&now)
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
