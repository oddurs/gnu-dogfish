//! printf — GNU-compatible format engine

use std::ffi::OsString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;

use anyhow::Result;
use dogfish_core::{DogfishContext, gnu_io_error, gnu_quote, gnu_quote_file, stdout};

use crate::common::escapes::unescape_echo_bytes;
use crate::common::getopt::{lossy_all, print_help, print_version};
use crate::common::locale::utf8_locale;
use crate::common::printf_format::{PrintfFlags, format_float, pad_field_into};
use crate::common::strtod::strtod_prefix;

/// State shared while rendering: diagnostics flip the exit status to 1.
struct PrintfRun<'a> {
    args: &'a [String],
    next: usize,
    failed: bool,
    /// A conversion was too wide for C's printf (`EOVERFLOW`): GNU prints
    /// nothing for it and reports a write error when it exits.
    overflow: bool,
}

/// Field widths and precisions from this up make glibc's printf fail.
const PRINTF_FIELD_MAX: usize = 2_147_483_646;

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
        if let Some(code) = self.char_constant(raw) {
            return i128::from(code);
        }
        let t = raw.trim_start();
        let b = t.as_bytes();
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
        if let Some(code) = self.char_constant(raw) {
            return f64::from(code);
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

    /// GNU's character constants: `'c` or `"c` is the code of `c` (a whole
    /// UTF-8 character in a UTF-8 locale, else one byte), with a warning
    /// when more characters follow. `None` when `raw` is no such constant.
    fn char_constant(&mut self, raw: &str) -> Option<u32> {
        let b = raw.as_bytes();
        if b.len() < 2 || !matches!(b[0], b'\'' | b'"') {
            return None;
        }
        let rest = &raw[1..];
        let (code, len) = match rest.chars().next() {
            Some(c) if utf8_locale() => (c as u32, c.len_utf8()),
            _ => (u32::from(b[1]), 1),
        };
        let after = &b[1 + len..];
        if !after.is_empty() && std::env::var_os("POSIXLY_CORRECT").is_none() {
            let mut msg = b"printf: warning: ".to_vec();
            msg.extend_from_slice(after);
            msg.extend_from_slice(
                b": character(s) following character constant have been ignored\n",
            );
            let _ = io::stderr().write_all(&msg);
        }
        Some(code)
    }

    fn complain(&mut self, raw: &str, what: &str) {
        eprintln!("printf: {}: {what}", gnu_quote(raw));
        self.failed = true;
    }
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
/// Each piece of output is also recorded as the stdio call GNU makes for it
/// (`putchar` for text and escapes, one `printf` per conversion), for the
/// diagnostic of a write error.
fn printf_once(
    fmt: &[u8],
    run: &mut PrintfRun<'_>,
    out: &mut Vec<u8>,
) -> std::result::Result<bool, ()> {
    let mut i = 0;
    while i < fmt.len() {
        let ch = fmt[i];
        if ch == b'\\' {
            let mark = out.len();
            i += 1;
            let Some(&e) = fmt.get(i) else {
                out.push(b'\\');
                stdout::gnu_putchars(1);
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
                    // \x takes 1 or 2 digits; \u and \U exactly 4 and 8.
                    if i == st || (e != b'x' && i - st < max) {
                        eprintln!("printf: missing hexadecimal number in escape");
                        return Err(());
                    }
                    let v = fmt[st..i].iter().fold(0u32, |v, &d| {
                        v.wrapping_mul(16)
                            .wrapping_add(char::from(d).to_digit(16).unwrap_or(0))
                    });
                    if e != b'x' && (0xd800..=0xdfff).contains(&v) {
                        eprintln!(
                            "printf: invalid universal character name \\{}{:0w$x}",
                            e as char,
                            v,
                            w = max
                        );
                        return Err(());
                    }
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
            stdout::gnu_putchars(out.len() - mark);
            continue;
        }
        if ch != b'%' {
            out.push(ch);
            stdout::gnu_putchars(1);
            i += 1;
            continue;
        }
        let spec_start = i;
        i += 1;
        if fmt.get(i) == Some(&b'%') {
            out.push(b'%');
            stdout::gnu_putchars(1);
            i += 1;
            continue;
        }
        // %b and %q take no flags, width or precision (as in GNU).
        if let Some(&c @ (b'b' | b'q')) = fmt.get(i) {
            i += 1;
            if let Some(a) = run.take() {
                if c == b'q' {
                    let quoted = gnu_quote_file(a);
                    stdout::gnu_fwrite(quoted.len());
                    out.extend_from_slice(quoted.as_bytes());
                } else {
                    let (bytes, stop) = unescape_echo_bytes(a.as_bytes(), true);
                    stdout::gnu_putchars(bytes.len());
                    out.extend(bytes);
                    if stop {
                        return Ok(false);
                    }
                }
            }
            continue;
        }
        let mut flags = PrintfFlags::default();
        // GNU's `ok` table: conversions each flag (and a precision) allows.
        let mut bad_for: Vec<u8> = Vec::new();
        while let Some(&f) = fmt.get(i) {
            match f {
                b'-' => flags.left = true,
                b'0' => {
                    flags.zero = true;
                    bad_for.extend_from_slice(b"cs");
                }
                b'+' => flags.plus = true,
                b' ' => flags.space = true,
                b'#' => {
                    flags.alt = true;
                    bad_for.extend_from_slice(b"cdisu");
                }
                // Thousands grouping: nothing to group in GNU's C locale.
                b'\'' | b'I' => bad_for.extend_from_slice(b"aAceEosxX"),
                _ => break,
            }
            i += 1;
        }
        let mut width = 0usize;
        if fmt.get(i) == Some(&b'*') {
            i += 1;
            let arg = run.take();
            let w = run.int_arg(arg, true);
            // GNU: the width must fit an int (no 2 GiB padding).
            if !(i128::from(i32::MIN)..=i128::from(i32::MAX)).contains(&w) {
                eprintln!(
                    "printf: invalid field width: {}",
                    gnu_quote(arg.unwrap_or_default())
                );
                return Err(());
            }
            if w < 0 {
                flags.left = true;
            }
            width = w.unsigned_abs() as usize;
        } else {
            while let Some(d @ b'0'..=b'9') = fmt.get(i).copied() {
                width = width
                    .saturating_mul(10)
                    .saturating_add(usize::from(d - b'0'));
                i += 1;
            }
        }
        let mut prec: Option<usize> = None;
        if fmt.get(i) == Some(&b'.') {
            i += 1;
            bad_for.push(b'c');
            if fmt.get(i) == Some(&b'*') {
                i += 1;
                let arg = run.take();
                let p = run.int_arg(arg, true);
                // A negative precision is as if omitted; one beyond INT_MAX
                // is an error.
                if p > i128::from(i32::MAX) {
                    eprintln!(
                        "printf: invalid precision: {}",
                        gnu_quote(arg.unwrap_or_default())
                    );
                    return Err(());
                }
                prec = (p >= 0).then_some(p as usize);
            } else {
                let mut p = 0usize;
                while let Some(d @ b'0'..=b'9') = fmt.get(i).copied() {
                    p = p.saturating_mul(10).saturating_add(usize::from(d - b'0'));
                    i += 1;
                }
                prec = Some(p);
            }
        }
        let overflow = width >= PRINTF_FIELD_MAX || prec.is_some_and(|p| p >= PRINTF_FIELD_MAX);
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
        if bad_for.contains(&conv) || !b"aAcdeEfFgGiosuxX".contains(&conv) {
            eprintln!(
                "printf: {}: invalid conversion specification",
                String::from_utf8_lossy(&fmt[spec_start..i])
            );
            return Err(());
        }
        if overflow {
            // Found by fuzzing: this was allocated (a 2^41-byte width aborted).
            // GNU's printf(3) fails with EOVERFLOW, printing nothing and
            // setting the stream's error flag.
            run.take();
            run.overflow = true;
            stdout::gnu_ferror();
            continue;
        }
        // The field's text, and whether zero padding applies to it.
        let (body, zero_ok): (Vec<u8>, bool) = match conv {
            b'd' | b'i' | b'o' | b'u' | b'x' | b'X' => {
                let a = run.take();
                let v = run.int_arg(a, matches!(conv, b'd' | b'i'));
                (
                    format_int(v, conv, prec, flags).into_bytes(),
                    prec.is_none(),
                )
            }
            b'f' | b'F' | b'e' | b'E' | b'g' | b'G' => {
                let a = run.take();
                let v = run.float_arg(a);
                let (s, finite) = format_float(v, conv, prec, flags);
                (s.into_bytes(), finite)
            }
            b's' => {
                let a = run.take().unwrap_or("").as_bytes();
                let a = match prec {
                    Some(p) => &a[..p.min(a.len())],
                    None => a,
                };
                (a.to_vec(), false)
            }
            b'c' => {
                let a = run.take().unwrap_or("");
                (vec![a.as_bytes().first().copied().unwrap_or(0)], false)
            }
            _ => {
                eprintln!(
                    "printf: {}: invalid conversion specification",
                    String::from_utf8_lossy(&fmt[spec_start..i])
                );
                return Err(());
            }
        };
        // A field too wide for memory fails as GNU's printf(3) does.
        let mark = out.len();
        if out.try_reserve(width.max(body.len())).is_err() {
            eprintln!(
                "printf: cannot perform formatted output: {}",
                gnu_io_error(&io::Error::from_raw_os_error(libc::ENOMEM))
            );
            return Err(());
        }
        pad_field_into(out, &body, width, flags, zero_ok);
        stdout::gnu_fwrite(out.len() - mark);
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

pub fn run_printf(raw: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    // Like GNU, a lone `--help` or `--version` (exactly) is the only option.
    if raw.len() == 2 {
        if raw[1] == "--help" {
            return Ok(print_help(PRINTF_HELP));
        }
        if raw[1] == "--version" {
            return Ok(print_version("printf"));
        }
    }
    // A leading "--" ends (the absent) options, as with getopt.
    let first = if raw.get(1).is_some_and(|a| a == "--") {
        2
    } else {
        1
    };
    if raw.len() <= first {
        eprintln!("printf: missing operand\nTry 'printf --help' for more information.");
        return Ok(1);
    }
    let args = lossy_all(raw.clone());
    // The format is used byte for byte.
    let fmt = raw[first].as_bytes();
    let mut run = PrintfRun {
        args: &args[first + 1..],
        next: 0,
        failed: false,
        overflow: false,
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

    // Unlike GNU, nothing was written yet: a write error is reported at
    // exit, as GNU's `close_stdout` would after the calls recorded above.
    let mut out = stdout::stdout();
    out.write_all(&output)?;
    ctx.telemetry.add_written(output.len() as u64);
    if run.overflow {
        // GNU's `close_stdout` sees the error flag: a bare `write error`,
        // or one with the reason when the final flush fails too, as does
        // closing a closed descriptor.
        out.flush()?;
        if stdout::was_closed() {
            let e = io::Error::from_raw_os_error(libc::EBADF);
            eprintln!("printf: write error: {}", gnu_io_error(&e));
        } else {
            eprintln!("printf: write error");
        }
        return Ok(1);
    }
    Ok(if ok && !run.failed { 0 } else { 1 })
}

/// Entry points for the fuzz targets in `fuzz/` (only with the `fuzzing` feature).
#[cfg(feature = "fuzzing")]
#[allow(dead_code)]
pub mod fuzz {
    use super::*;

    /// Renders `fmt` with `args` as `printf FORMAT ARGS...` would, without
    /// writing it anywhere.
    pub fn render(fmt: &[u8], args: &[String]) -> Vec<u8> {
        let mut run = PrintfRun {
            args,
            next: 0,
            failed: false,
            overflow: false,
        };
        let mut out = Vec::new();
        loop {
            let before = run.next;
            match printf_once(fmt, &mut run, &mut out) {
                Ok(true) => {}
                Ok(false) | Err(()) => break,
            }
            if run.next >= run.args.len() || run.next == before {
                break;
            }
        }
        out
    }
}
