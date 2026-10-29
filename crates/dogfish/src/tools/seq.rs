//! seq — GNU number scanning, widths, formats and arbitrary-size integers

use std::io::{self, Write};

use anyhow::Result;
use dogfish_core::{DEFAULT_BUF_SIZE, DogfishContext, gnu_quote, io::ignore_broken_pipe};

use crate::common::getopt::{GetOpt, HasArg, OptSpec, opt, print_help, usage_error};
use crate::common::output::raw_stdout;
use crate::common::printf_format::{PrintfFlags, format_float, pad_field};
use crate::common::strtod::strtod_prefix;

/// Parses `s` as a whole C floating-point number (see [`strtod_prefix`]).
fn strtod_full(s: &str) -> Option<f64> {
    strtod_prefix(s).and_then(|(v, n)| (n == s.len()).then_some(v))
}

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

/// Bytes handed to `write` at a time by [`seq_fast`].
const FAST_BLOCK: usize = 128 * 1024;

/// The longest separator-and-number line [`seq_fast`] copies with one
/// fixed-size store; longer ones are copied piece by piece.
const FAST_LINE: usize = 32;

/// GNU's `seq_fast`: counts from the digit string `cur` to `end` by `inc`
/// (1 to 200) in decimal text, so numbers of any size are exact. Numbers
/// that differ only in the last digit form a run: `end` is compared once a
/// run, and each number is one fixed-size copy of the run's line (separator
/// and digits) with the last digit patched in. Output goes straight to fd 1
/// in large blocks. Returns how many numbers were written; a closed pipe
/// ends the output quietly.
fn seq_fast(mut cur: Vec<u8>, end: &[u8], inc: u32, sep: &[u8]) -> io::Result<u64> {
    let mut out = raw_stdout();
    let mut buf = vec![0u8; FAST_BLOCK + FAST_LINE];
    let mut pos = 0;
    let mut count = 0u64;
    let mut line = [0u8; FAST_LINE];
    macro_rules! flush {
        ($bytes:expr) => {
            match out.write_all($bytes) {
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(count),
                r => r?,
            }
        };
    }
    while decimal_le(&cur, end) {
        let last = cur.len() - 1;
        // Bounded by 9, or by the last number's last digit once all other
        // digits match it.
        let prefix_below = cur.len() < end.len() || cur[..last] < end[..last];
        let max_digit = if prefix_below {
            9
        } else {
            u32::from(end[last] - b'0')
        };
        let mut digit = u32::from(cur[last] - b'0');
        let len = sep.len() + cur.len();
        let fixed = len <= FAST_LINE;
        if fixed {
            line[..sep.len()].copy_from_slice(sep);
            line[sep.len()..len].copy_from_slice(&cur);
        }
        while digit <= max_digit {
            let d = b'0' + digit as u8;
            // No separator before the first number.
            if fixed && count > 0 {
                line[len - 1] = d;
                buf[pos..pos + FAST_LINE].copy_from_slice(&line);
                pos += len;
            } else {
                cur[last] = d;
                let sep = if count > 0 { sep } else { &[][..] };
                if pos + sep.len() + cur.len() > FAST_BLOCK {
                    flush!(&buf[..pos]);
                    pos = 0;
                }
                if sep.len() + cur.len() > FAST_BLOCK {
                    flush!(sep);
                    flush!(&cur);
                } else {
                    buf[pos..pos + sep.len()].copy_from_slice(sep);
                    pos += sep.len();
                    buf[pos..pos + cur.len()].copy_from_slice(&cur);
                    pos += cur.len();
                }
            }
            count += 1;
            digit += inc;
            if pos >= FAST_BLOCK {
                flush!(&buf[..pos]);
                pos = 0;
            }
        }
        if !prefix_below {
            break;
        }
        // The next number is this run's with last digit 0, plus `digit`.
        cur[last] = b'0';
        decimal_add(&mut cur, digit);
    }
    if count > 0 {
        buf[pos] = b'\n';
        pos += 1;
    }
    flush!(&buf[..pos]);
    Ok(count)
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
    if let (None, false, true, Some((cur, end))) = (&user_format, equal_width, small_step, fast) {
        let inc = step.value as u32;
        let count = seq_fast(cur, &end, inc, sep)?;
        ctx.telemetry.add_items(count);
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
