//! printf — GNU-compatible format engine

use std::io::Write;

use anyhow::Result;
use dogfish_core::{DogfishContext, gnu_quote, gnu_quote_file, io::ignore_broken_pipe, stdout};

use crate::common::escapes::unescape_echo_bytes;
use crate::common::legacy_getopt::print_help;
use crate::common::printf_format::{PrintfFlags, format_float, pad_field};
use crate::common::strtod::strtod_prefix;

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

    let mut out = stdout::stdout();
    ignore_broken_pipe(out.write_all(&output))?;
    ctx.telemetry.add_written(output.len() as u64);
    Ok(if ok && !run.failed { 0 } else { 1 })
}
