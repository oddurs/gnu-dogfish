//! seq — GNU number scanning, widths, formats and arbitrary-size integers

use std::ffi::OsString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;

use anyhow::Result;
use dogfish_core::{DEFAULT_BUF_SIZE, DogfishContext, RecordWriter, gnu_quote, stdout};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, Order, getopt_error, lossy, lossy_all, print_help, print_version,
    usage_error,
};
use crate::common::json_opt::{check_json_version, json_long, json_short};
use crate::common::ldbl::{Float, FloatSpec, Format, LongDouble, Native, format_into, strtold};
use crate::common::printf_format::PrintfFlags;

/// GNU's `xstrtold` with `cl_strtold`: the whole of `s` is a number, which
/// may underflow to zero but not overflow (or underflow to a subnormal).
fn xstrtold(s: &[u8]) -> Option<LongDouble> {
    let r = strtold::<Native>(s);
    let ok = r.len > 0 && r.len == s.len() && !(r.erange && !r.value.is_zero());
    ok.then_some(r.value)
}

/// The largest step GNU's `seq_fast` counts by.
const SEQ_FAST_STEP_LIMIT: u32 = 200;

const SEQ_HELP: &str = "\
Usage: seq [OPTION]... LAST
  or:  seq [OPTION]... FIRST LAST
  or:  seq [OPTION]... FIRST INCREMENT LAST
Print numbers from FIRST to LAST, in steps of INCREMENT.

  -f, --format=FORMAT      use printf style floating-point FORMAT
  -s, --separator=STRING   use STRING to separate numbers (default: \\n)
  -w, --equal-width        equalize width by padding with leading zeroes
  -J, --json[=VERSION]     print each number as an NDJSON record: value
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

/// GNU's `long_options` and `"+f:s:w"`, plus the Dogfish `-J, --json`.
const SEQ_OPTS: &[OptSpec<&str>] = &[
    OptSpec::both(b'w', "equal-width", HasArg::No, "equal-width"),
    OptSpec::both(b'f', "format", HasArg::Required, "format"),
    OptSpec::both(b's', "separator", HasArg::Required, "separator"),
    OptSpec::long("help", HasArg::No, "help"),
    OptSpec::long("version", HasArg::No, "version"),
    json_short("json"),
    json_long("json"),
];

/// A scanned `seq` operand: its value and the output width and precision
/// GNU derives from how it was written.
#[derive(Clone, Copy)]
struct SeqOperand {
    value: LongDouble,
    width: usize,
    /// Fraction digits; `None` means "unknown" (hex or non-finite input).
    precision: Option<usize>,
    /// The exact value when it is an integer that fits `i64` (see
    /// [`operand_int`]).
    int: Option<i64>,
}

/// The exact integer an operand names, if it fits `i64`: as written for
/// plain decimal integers, so that it does not depend on how precise
/// `long double` is, else the value when that is an integer exact in the
/// format. `-0` is not one (it prints as "-0").
///
/// Where `long double` has 64 bits of significand every such integer is
/// exact, so this is GNU's value; where it is `double` (Apple silicon),
/// `-9223372036854775807` is still that number, not -2^63.
fn operand_int<F: Format>(text: &str, value: Float<F>) -> Option<i64> {
    if value.is_zero() && value.is_sign_negative() {
        return None;
    }
    text.parse::<i64>().ok().or_else(|| value.exact_i64())
}

/// GNU's `scan_arg`.
fn scan_seq_operand(arg: &str) -> std::result::Result<SeqOperand, String> {
    let Some(value) = xstrtold(arg.as_bytes()) else {
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
    Ok(SeqOperand {
        value,
        width: width.max(0) as usize,
        precision,
        int: operand_int(s, value),
    })
}

/// A validated `-f` format: the text around its single floating-point
/// directive, and the directive itself.
struct SeqFormat {
    prefix: Vec<u8>,
    spec: FloatSpec,
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
            spec: FloatSpec {
                conv,
                flags,
                width,
                prec: precision,
            },
            suffix: unescape_pct(suffix),
        })
    }

    /// Appends `v` as `printf (fmt, v)` prints it.
    fn render_into(&self, out: &mut Vec<u8>, v: LongDouble) {
        out.extend_from_slice(&self.prefix);
        format_into(out, v, &self.spec);
        out.extend_from_slice(&self.suffix);
    }

    /// `v` as the format's directive alone prints it.
    fn render_number(&self, v: LongDouble) -> Vec<u8> {
        let mut out = Vec::new();
        format_into(&mut out, v, &self.spec);
        out
    }
}

/// GNU's `get_default_format`: `%.{prec}Lf` (or `%0{width}.{prec}Lf` with
/// `-w`), or `%Lg` when some operand has no fixed precision (hexadecimal or
/// infinite input).
fn seq_default_format(
    first: SeqOperand,
    step: SeqOperand,
    last: SeqOperand,
    equal_width: bool,
) -> SeqFormat {
    let mut fmt = SeqFormat {
        prefix: Vec::new(),
        spec: FloatSpec::default(),
        suffix: Vec::new(),
    };
    let (Some(fp), Some(sp), Some(lp)) = (first.precision, step.precision, last.precision) else {
        return fmt;
    };
    let prec = fp.max(sp);
    fmt.spec.conv = b'f';
    fmt.spec.prec = Some(prec);
    if equal_width {
        let first_width = first.width + (prec - fp) + usize::from(fp == 0 && prec > 0);
        let mut last_width = (last.width + prec).saturating_sub(lp);
        if lp > 0 && prec == 0 {
            last_width = last_width.saturating_sub(1);
        }
        if lp == 0 && prec > 0 {
            last_width += 1;
        }
        fmt.spec.flags.zero = true;
        fmt.spec.width = first_width.max(last_width);
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

/// GNU's `seq_fast`: counts from the digit string `cur` to `end` (forever
/// when `None`, GNU's `inf`) by `inc` (1 to 200) in decimal text, so numbers
/// of any size are exact. Numbers
/// that differ only in the last digit form a run: `end` is compared once a
/// run, and each number is one fixed-size copy of the run's line (separator
/// and digits) with the last digit patched in. Output goes to the stdout
/// writer in large blocks. Returns how many numbers were written; a closed pipe
/// ends the output quietly.
fn seq_fast(mut cur: Vec<u8>, end: Option<&[u8]>, inc: u32, sep: &[u8]) -> io::Result<u64> {
    let mut out = stdout::stdout();
    let mut buf = vec![0u8; FAST_BLOCK + FAST_LINE];
    let mut pos = 0;
    let mut count = 0u64;
    let mut line = [0u8; FAST_LINE];
    macro_rules! flush {
        ($bytes:expr) => {
            out.write_all($bytes)?
        };
    }
    while end.is_none_or(|end| decimal_le(&cur, end)) {
        let last = cur.len() - 1;
        // Bounded by 9, or by the last number's last digit once all other
        // digits match it.
        let prefix_below = end.is_none_or(|end| cur.len() < end.len() || cur[..last] < end[..last]);
        let max_digit = match end {
            Some(end) if !prefix_below => u32::from(end[last] - b'0'),
            _ => 9,
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
                // End the write at a line break, so that the line-buffered
                // writer passes it straight through instead of holding back
                // (and later writing on its own) a partial line.
                let cut = memchr::memrchr(b'\n', &buf[..pos]).map_or(pos, |i| i + 1);
                flush!(&buf[..cut]);
                buf.copy_within(cut..pos, 0);
                pos -= cut;
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

pub fn run_seq(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let mut opts = Getopt::with_order(
        args.into_iter().skip(1).collect(),
        SEQ_OPTS,
        Order::StopAtOperand,
    );
    let mut format: Option<String> = None;
    let mut separator = "\n".to_string();
    let mut equal_width = false;
    let mut json = false;
    loop {
        // A negative number is an operand, and ends option processing.
        if let Some(a) = opts.peek().map(OsStrExt::as_bytes)
            && a.len() > 1
            && a[0] == b'-'
            && (a[1] == b'.' || a[1].is_ascii_digit())
        {
            break;
        }
        let Some(item) = opts.next() else { break };
        let (key, v) = match item {
            Ok((key, v)) => (key, v.as_deref().map(lossy)),
            Err(e) => return Ok(getopt_error("seq", &e)),
        };
        match key {
            "format" => format = v,
            "separator" => separator = v.unwrap_or_default(),
            "equal-width" => equal_width = true,
            "json" => {
                if let Err(code) = check_json_version("seq", v.as_deref().map(std::ffi::OsStr::new))
                {
                    return Ok(code);
                }
                json = true;
            }
            "help" => return Ok(print_help(SEQ_HELP)),
            "version" => return Ok(print_version("seq")),
            _ => unreachable!("every seq option is handled"),
        }
    }
    let nums = lossy_all(opts.operands());
    match nums.len() {
        0 => return Ok(usage_error("seq", "missing operand")),
        1..=3 => {}
        _ => {
            return Ok(usage_error(
                "seq",
                format!("extra operand {}", gnu_quote(&nums[3])),
            ));
        }
    }
    // As GNU: the format first, then the fast path, then the operands.
    let user_format = match format.as_deref().map(SeqFormat::parse).transpose() {
        Ok(f) => f,
        Err(msg) => {
            eprintln!("seq: {msg}");
            return Ok(1);
        }
    };
    if user_format.is_some() && equal_width {
        return Ok(usage_error(
            "seq",
            "format string may not be specified when printing equal width strings",
        ));
    }
    let sep = separator.as_bytes();
    let fast_ok = user_format.is_none() && !equal_width && sep.len() == 1 && !json;

    // GNU's first `seq_fast` attempt: operands written as digits only, a step
    // of at most 200, counted in decimal text.
    if fast_ok && nums.iter().all(|n| all_digits(n)) {
        let step = match nums.len() {
            3 => xstrtold(nums[1].as_bytes())
                .and_then(LongDouble::exact_i64)
                .filter(|n| (1..=i64::from(SEQ_FAST_STEP_LIMIT)).contains(n)),
            _ => Some(1),
        };
        if let Some(step) = step {
            let trim = |n: &str| {
                let t = n.trim_start_matches('0');
                if t.is_empty() {
                    b"0".to_vec()
                } else {
                    t.as_bytes().to_vec()
                }
            };
            let cur = if nums.len() == 1 {
                b"1".to_vec()
            } else {
                trim(&nums[0])
            };
            let end = trim(&nums[nums.len() - 1]);
            if decimal_le(&cur, &end) {
                let count = seq_fast(cur, Some(&end), step as u32, sep)?;
                ctx.telemetry.add_items(count);
                return Ok(0);
            }
        }
    }

    // GNU scans LAST, then FIRST and LAST, then the increment (rejecting 0)
    // before the last operand.
    let one = SeqOperand {
        value: LongDouble::from_u64(1),
        width: 1,
        precision: Some(0),
        int: Some(1),
    };
    let mut scanned = Vec::with_capacity(nums.len());
    for (i, n) in nums.iter().enumerate() {
        match scan_seq_operand(n) {
            Ok(s) => scanned.push(s),
            Err(msg) => return Ok(usage_error("seq", msg)),
        }
        if i == 1 && nums.len() == 3 && scanned[1].value.is_zero() {
            return Ok(usage_error(
                "seq",
                format!("invalid Zero increment value: {}", gnu_quote(n)),
            ));
        }
    }
    let (first, step, last) = match scanned.as_slice() {
        [l] => (one, one, *l),
        [f, l] => (*f, one, *l),
        [f, s, l] => (*f, *s, *l),
        _ => unreachable!(),
    };
    let zero = LongDouble::zero(false);
    let negative_step = step.value < zero;
    let out_of_range = |x: LongDouble| {
        if negative_step {
            x < last.value
        } else {
            last.value < x
        }
    };
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout::stdout());

    if json {
        return seq_json(&mut out, first, step, last, out_of_range);
    }

    // GNU's second attempt, for integers written otherwise (`1e2`) or an
    // infinite LAST: counts from their "%.0Lf" spellings.
    let small_step = step
        .value
        .exact_i64()
        .filter(|n| (1..=i64::from(SEQ_FAST_STEP_LIMIT)).contains(n));
    if let Some(inc) = small_step
        && fast_ok
        && [first, step, last].iter().all(|o| o.precision == Some(0))
        && first.value.is_finite()
        && zero <= first.value
        && zero <= last.value
    {
        let spell = |v: LongDouble| {
            let mut s = Vec::new();
            let spec = FloatSpec {
                conv: b'f',
                prec: Some(0),
                ..FloatSpec::default()
            };
            format_into(&mut s, v, &spec);
            s
        };
        let cur = spell(first.value);
        let end = last.value.is_finite().then(|| spell(last.value));
        if cur[0] != b'-'
            && end.as_ref().is_none_or(|e| e[0] != b'-')
            && end.as_deref().is_none_or(|e| decimal_le(&cur, e))
        {
            let count = seq_fast(cur, end.as_deref(), inc as u32, sep)?;
            ctx.telemetry.add_items(count);
            return Ok(0);
        }
    }

    let default_format = user_format.is_none();
    let fmt = user_format.unwrap_or_else(|| seq_default_format(first, step, last, equal_width));
    if out_of_range(first.value) {
        return Ok(0);
    }

    // Integers in the default "%.0Lf" format are exact in an 80-bit `long
    // double` (they fit i64), so they need no floating point at all; where
    // `long double` is narrower they are counted exactly all the same,
    // rather than as GNU's rounding would (`seq -9223372036854775807 -1
    // -9223372036854775808` repeating -2^63 a thousand times).
    let ints = (first.int, step.int, last.int);
    if let (true, b'f', Some(0), (Some(mut cur), Some(inc), Some(end))) =
        (default_format, fmt.spec.conv, fmt.spec.prec, ints)
    {
        let width = if fmt.spec.flags.zero {
            fmt.spec.width
        } else {
            0
        };
        let mut count = 0u64;
        let mut itoa_buf = itoa::Buffer::new();
        while (inc > 0 && cur <= end) || (inc < 0 && cur >= end) {
            if count > 0 {
                out.write_all(sep)?;
            }
            if width > 0 {
                out.write_all(format!("{:0width$}", cur).as_bytes())?;
            } else {
                out.write_all(itoa_buf.format(cur).as_bytes())?;
            }
            count += 1;
            // Past i64::MAX or MIN is past LAST too.
            match cur.checked_add(inc) {
                Some(next) => cur = next,
                None => break,
            }
        }
        ctx.telemetry.add_items(count);
        out.write_all(b"\n")?;
        out.flush()?;
        return Ok(0);
    }

    // GNU's `print_numbers`: FIRST + i × STEP in `long double`, so that
    // errors do not accumulate.
    let one = LongDouble::from_u64(1);
    let mut line = Vec::with_capacity(64);
    let mut x = first.value;
    let mut i = one;
    let mut past_last = false;
    loop {
        let previous = x;
        line.clear();
        fmt.render_into(&mut line, x);
        out.write_all(&line)?;
        ctx.telemetry.add_items(1);
        if past_last {
            break;
        }
        x = first.value.add(i.mul(step.value));
        i = i.add(one);
        past_last = out_of_range(x);
        if past_last {
            // Print one more number when it prints as LAST and differs from
            // the previous one, so that rounding error does not drop the
            // endpoint ("seq 0 0.000001 0.000003").
            let shown = fmt.render_number(x);
            let equals_last = xstrtold(&shown).is_some_and(|v| v == last.value);
            if !(equals_last && shown != fmt.render_number(previous)) {
                break;
            }
        }
        out.write_all(sep)?;
    }
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(0)
}

/// `seq -J`: one record per number, streamed (`seq -J inf` runs as long as
/// `seq inf`): integers exactly when all three operands are, else floating
/// point, which ends at the first value that is not finite.
fn seq_json(
    out: &mut impl Write,
    first: SeqOperand,
    step: SeqOperand,
    last: SeqOperand,
    out_of_range: impl Fn(LongDouble) -> bool,
) -> Result<i32> {
    let int = |o: SeqOperand| o.int.filter(|_| o.precision == Some(0));
    let mut w = RecordWriter::new(&mut *out);
    if let (Some(mut cur), Some(inc), Some(end)) = (int(first), int(step), int(last)) {
        while (inc > 0 && cur <= end) || (inc < 0 && cur >= end) {
            w.record(|r| {
                r.i64("value", cur);
            })?;
            match cur.checked_add(inc) {
                Some(next) => cur = next,
                None => break,
            }
        }
    } else {
        let one = LongDouble::from_u64(1);
        let mut x = first.value;
        let mut i = one;
        while x.is_finite() && !out_of_range(x) {
            w.record(|r| {
                r.f64("value", x.to_f64());
            })?;
            x = first.value.add(i.mul(step.value));
            i = i.add(one);
        }
    }
    w.flush()?;
    drop(w);
    out.flush()?;
    Ok(0)
}

/// seq's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("seq", SEQ_HELP, SEQ_OPTS)
}

/// Entry points for the fuzz targets in `fuzz/` (only with the `fuzzing` feature).
#[cfg(feature = "fuzzing")]
#[allow(dead_code)]
pub mod fuzz {
    use super::*;

    /// A `seq` operand.
    pub fn operand(arg: &str) {
        if let Ok(op) = scan_seq_operand(arg)
            && let Some(i) = op.int
        {
            assert_eq!(op.value.to_f64(), i as f64, "{arg:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::ldbl::{Binary64, X87};

    fn int<F: Format>(text: &str) -> Option<i64> {
        operand_int(text, strtold::<F>(text.as_bytes()).value)
    }

    #[test]
    fn integers_are_exact_whatever_long_double_is() {
        for text in [
            "-9223372036854775807",
            "9223372036854775807",
            "-9223372036854775808",
        ] {
            let n = text.parse::<i64>().ok();
            assert_eq!(int::<X87>(text), n);
            // A double rounds these to 2^63.
            assert_eq!(int::<Binary64>(text), n);
        }
        assert_eq!(int::<Binary64>("9007199254740993"), Some(9007199254740993));
        assert_eq!(int::<X87>("1e2"), Some(100));
        assert_eq!(int::<X87>("0x10"), Some(16));
        assert_eq!(int::<X87>("007"), Some(7));
        assert_eq!(int::<X87>("-0"), None);
        assert_eq!(int::<X87>("1.5"), None);
        assert_eq!(int::<X87>("9223372036854775808"), None);
        // Written as an exponent, the double's value is all there is.
        assert_eq!(
            int::<Binary64>("9.007199254740993e15"),
            Some(9007199254740992)
        );
    }
}
