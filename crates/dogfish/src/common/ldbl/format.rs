//! glibc's `printf` conversions of a `long double`: `%f %e %g` (and `%F %E
//! %G`) as `printf_fp` produces them, `%a %A` as `printf_fphex` does, with
//! the flags, width and precision of [`FloatSpec`].
//!
//! Digits are exact: rounding to the precision is to nearest, ties to even
//! on the exact binary value, as glibc rounds in the default mode.

use super::big::Big;
use super::{Float, Format};
use crate::common::printf_format::{PrintfFlags, pad_field_into};

/// One floating-point conversion: `%[flags][width][.prec]conv`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FloatSpec {
    /// One of `aAeEfFgG`.
    pub(crate) conv: u8,
    pub(crate) flags: PrintfFlags,
    pub(crate) width: usize,
    pub(crate) prec: Option<usize>,
}

impl Default for FloatSpec {
    fn default() -> Self {
        FloatSpec {
            conv: b'g',
            flags: PrintfFlags::default(),
            width: 0,
            prec: None,
        }
    }
}

/// Appends `x` converted by `spec` to `out`.
pub(crate) fn format_into<F: Format>(out: &mut Vec<u8>, x: Float<F>, spec: &FloatSpec) {
    let mark = out.len();
    let flags = spec.flags;
    if x.neg {
        out.push(b'-');
    } else if flags.plus {
        out.push(b'+');
    } else if flags.space {
        out.push(b' ');
    }
    let upper = spec.conv.is_ascii_uppercase();
    let finite = x.is_finite();
    if !finite {
        let s: &[u8] = match (x.is_nan(), upper) {
            (true, false) => b"nan",
            (true, true) => b"NAN",
            (false, false) => b"inf",
            (false, true) => b"INF",
        };
        out.extend_from_slice(s);
    } else if spec.conv | 0x20 == b'a' {
        fphex(out, x, spec);
    } else {
        let (m, e) = x.parts();
        fp(out, m, e, spec);
    }
    let len = out.len() - mark;
    if len < spec.width {
        // Pad the body in place, as `pad_field_into` pads it.
        let body = out.split_off(mark);
        pad_field_into(out, &body, spec.width, flags, finite);
    }
}

/// The integer digits of a value: values 0 to 9, most significant first.
enum IntDigits {
    Small([u8; 40], usize),
    Big(Vec<u8>),
}

impl IntDigits {
    fn as_slice(&self) -> &[u8] {
        match self {
            IntDigits::Small(buf, len) => &buf[40 - len..],
            IntDigits::Big(v) => v,
        }
    }
}

/// The fraction `f / 2^s` of a value, below 1.
enum Frac {
    Small(u128, u32),
    Big(Big, u64),
}

impl Frac {
    fn is_zero(&self) -> bool {
        match self {
            Frac::Small(f, _) => *f == 0,
            Frac::Big(f, _) => f.is_zero(),
        }
    }

    /// The next decimal digit.
    fn digit(&mut self) -> u8 {
        match self {
            Frac::Small(f, s) => {
                // f < 2^s with s <= 124, so 10f fits.
                let t = *f * 10;
                *f = t & ((1u128 << *s) - 1);
                (t >> *s) as u8
            }
            Frac::Big(f, s) => {
                f.mul_add(10, 0);
                f.take_high(*s) as u8
            }
        }
    }

    /// Multiplies by `10^k`, skipping `k` digits known to be zero.
    fn skip(&mut self, k: u64) {
        match self {
            Frac::Small(f, s) => {
                for _ in 0..k {
                    *f *= 10;
                }
                debug_assert!(*f >> *s == 0, "skipped a nonzero digit");
            }
            Frac::Big(f, s) => {
                f.mul_pow10(k);
                let high = f.take_high(*s);
                debug_assert_eq!(high, 0, "skipped a nonzero digit");
            }
        }
    }
}

/// The decimal digits of a finite value `m × 2^e ≥ 0`, one position at a
/// time: what glibc's `hack_digit` produces.
struct Digits {
    int: IntDigits,
    /// The lowest position (0 for units) of a nonzero integer digit.
    int_low_nonzero: Option<i64>,
    frac: Frac,
    /// The position of the next digit [`Digits::next`] returns.
    pos: i64,
    /// The position of the next digit `frac` yields; positions above it
    /// (and below 0) were skipped as zeros.
    frac_pos: i64,
    /// The digit at `frac_pos`, when already taken from `frac`.
    pending: Option<u8>,
}

impl Digits {
    fn new(m: u128, e: i64) -> Self {
        let bits = i64::from(128 - m.leading_zeros());
        let (int, frac) = if e >= 0 {
            let frac = Frac::Small(0, 0);
            if bits + e <= 127 {
                (small_decimal(m << e), frac)
            } else {
                let mut b = Big::from_u128(m);
                b.shl(e as u64);
                (IntDigits::Big(b.into_decimal()), frac)
            }
        } else {
            let s = e.unsigned_abs();
            let int = if s < 128 { m >> s } else { 0 };
            let f = if s < 128 { m & ((1u128 << s) - 1) } else { m };
            let frac = if s <= 124 {
                Frac::Small(f, s as u32)
            } else {
                Frac::Big(Big::from_u128(f), s)
            };
            (small_decimal(int), frac)
        };
        let digits = int.as_slice();
        let int_low_nonzero = digits.iter().rev().position(|&d| d != 0).map(|p| p as i64);
        Digits {
            int,
            int_low_nonzero,
            frac,
            pos: 0,
            frac_pos: -1,
            pending: None,
        }
    }

    fn int_len(&self) -> i64 {
        self.int.as_slice().len() as i64
    }

    /// The position of the most significant digit (the decimal exponent);
    /// the value must not be zero. Call before taking digits.
    fn msd(&mut self) -> i64 {
        if self.int_len() > 0 {
            return self.int_len() - 1;
        }
        // Below 1: skip the zeros the bit length guarantees, then look.
        let (b, s) = match &self.frac {
            Frac::Small(f, s) => (i64::from(128 - f.leading_zeros()), i64::from(*s)),
            Frac::Big(f, s) => (f.bits() as i64, *s as i64),
        };
        // The value is below 2^(b - s), so its leading digit is at or below
        // position floor((b - s) log10 2).
        let upper = ((b - s) as f64 * std::f64::consts::LOG10_2).floor() as i64 + 1;
        if upper < -1 {
            self.frac.skip((-1 - upper) as u64);
            self.frac_pos = upper;
        }
        loop {
            let d = self.frac.digit();
            if d != 0 {
                self.pending = Some(d);
                return self.frac_pos;
            }
            self.frac_pos -= 1;
        }
    }

    /// Starts the digits at position `pos`.
    fn seek(&mut self, pos: i64) {
        self.pos = pos;
    }

    fn next(&mut self) -> u8 {
        let k = self.pos;
        self.pos -= 1;
        if k >= 0 {
            let digits = self.int.as_slice();
            return if k < digits.len() as i64 {
                digits[digits.len() - 1 - k as usize]
            } else {
                0
            };
        }
        if k > self.frac_pos {
            return 0;
        }
        self.frac_pos -= 1;
        match self.pending.take() {
            Some(d) => d,
            None => self.frac.digit(),
        }
    }

    /// Whether every digit from the next one on is zero.
    fn rest_is_zero(&self) -> bool {
        !self.int_low_nonzero.is_some_and(|z| z <= self.pos)
            && self.pending.is_none()
            && self.frac.is_zero()
    }
}

fn small_decimal(mut n: u128) -> IntDigits {
    let mut buf = [0u8; 40];
    let mut len = 0;
    while n != 0 {
        len += 1;
        buf[40 - len] = (n % 10) as u8;
        n /= 10;
    }
    IntDigits::Small(buf, len)
}

/// Whether to round away from zero, to nearest with ties to even (glibc's
/// `round_away` in the default rounding mode, which ignores the sign).
fn round_away(last_odd: bool, half: bool, more: bool) -> bool {
    half && (last_odd || more)
}

/// glibc's `__printf_fp` for `%e %f %g` (either case) of the finite value
/// `m × 2^e`, its sign already written.
fn fp(out: &mut Vec<u8>, m: u128, e: i64, spec: &FloatSpec) {
    let alt = spec.flags.alt;
    let spec_l = spec.conv | 0x20;
    let upper = spec.conv.is_ascii_uppercase();
    let exp_char = if upper { b'E' } else { b'e' };
    let mut g = Digits::new(m, e);
    // The decimal exponent: its magnitude, and whether it is negative.
    let (mut expsign, mut exponent) = if m == 0 {
        (false, 0i64)
    } else {
        let k = g.msd();
        (k < 0, k.abs())
    };
    let prec = spec.prec.map(|p| p as i64);
    let mut is_f;
    let intdig_max: i64;
    let mut fracdig_max: i64;
    let fracdig_min: i64;
    let dig_max: i64;
    let mut significant: bool;
    match spec_l {
        b'e' => {
            is_f = false;
            intdig_max = 1;
            fracdig_max = prec.unwrap_or(6);
            fracdig_min = fracdig_max;
            dig_max = i64::MAX;
            significant = true;
        }
        b'f' => {
            is_f = true;
            fracdig_max = prec.unwrap_or(6);
            fracdig_min = fracdig_max;
            dig_max = i64::MAX;
            significant = true;
            intdig_max = if expsign { 1 } else { exponent + 1 };
        }
        _ => {
            dig_max = match prec {
                None => 6,
                Some(0) => 1,
                Some(p) => p,
            };
            if (!expsign && exponent >= dig_max) || (expsign && exponent > 4) {
                is_f = false;
                fracdig_max = dig_max - 1;
                intdig_max = 1;
            } else {
                is_f = true;
                intdig_max = if expsign { 0 } else { exponent + 1 };
                fracdig_max = dig_max - intdig_max;
            }
            fracdig_min = if alt { fracdig_max } else { 0 };
            significant = false;
        }
    }

    // Two bytes of room in front for rounding up.
    let mut w: Vec<u8> = Vec::with_capacity(24);
    w.extend_from_slice(b"\0\0");
    let mut wstart = 2usize;
    let mut intdig_no: i64 = 0;
    if !expsign || !is_f {
        g.seek(if m == 0 {
            0
        } else {
            exponent * if expsign { -1 } else { 1 }
        });
        while intdig_no < intdig_max {
            intdig_no += 1;
            w.push(b'0' + g.next());
        }
        significant = true;
        if alt || fracdig_min > 0 || (fracdig_max > 0 && !g.rest_is_zero()) {
            w.push(b'.');
        }
    } else {
        // Below 1 in fixed notation: "0." and the digits from 10^-1 on.
        w.extend_from_slice(b"0.");
        g.seek(-1);
    }

    let mut fracdig_no: i64 = 0;
    let mut added_zeros: i64 = 0;
    while fracdig_no < fracdig_min + added_zeros || (fracdig_no < fracdig_max && !g.rest_is_zero())
    {
        fracdig_no += 1;
        let d = g.next();
        w.push(b'0' + d);
        if d != 0 {
            significant = true;
        } else if !significant {
            fracdig_max += 1;
            if fracdig_min > 0 {
                added_zeros += 1;
            }
        }
    }

    // Rounding.
    let last = w.len() - 1;
    let last_digit = if w[last] != b'.' {
        w[last]
    } else {
        w[last - 1]
    };
    let next_digit = g.next();
    let more_bits = if next_digit != 0 && next_digit != 5 {
        true
    } else {
        !g.rest_is_zero()
    };
    if round_away((last_digit - b'0') & 1 == 1, next_digit >= 5, more_bits) {
        let mut wtp = w.len();
        if fracdig_no > 0 {
            let mut removed = 0;
            loop {
                wtp -= 1;
                if w[wtp] != b'.' && w[wtp] == b'9' {
                    w[wtp] = b'0';
                    removed += 1;
                } else {
                    break;
                }
            }
            if removed == fracdig_min && added_zeros > 0 {
                added_zeros -= 1;
            }
            if w[wtp] != b'.' {
                w[wtp] += 1;
            } else if spec_l == b'g' && is_f && alt && wtp == wstart + 1 && w[wstart] == b'0' {
                // The rounded number is 1.0 in "%#g": it must be "1.".
                added_zeros -= 1;
            }
        }
        if fracdig_no == 0 || w[wtp] == b'.' {
            // Round the integer digits.
            if w[wtp - 1] == b'.' {
                wtp -= 1;
            }
            let mut rounded = false;
            while wtp > wstart {
                wtp -= 1;
                if w[wtp] == b'9' {
                    w[wtp] = b'0';
                } else {
                    w[wtp] += 1;
                    rounded = true;
                    break;
                }
            }
            if !rounded {
                // All digits were nines.
                if !is_f {
                    w[wstart] = b'1';
                    exponent += if expsign { -1 } else { 1 };
                    // 0.999e-01 rounds to 1.0e+00, not 1.0e-00.
                    if exponent == 0 {
                        expsign = false;
                    }
                } else if intdig_no == dig_max {
                    // "%g" fits in fixed notation, but not once rounded.
                    wstart -= 1;
                    w[wstart] = b'.';
                    wstart -= 1;
                    w[wstart] = b'1';
                    if alt || fracdig_no > 0 {
                        // Overwrite the old radix character.
                        w[wstart + intdig_no as usize + 2] = b'0';
                        fracdig_no += 1;
                    }
                    fracdig_no += intdig_no;
                    intdig_no = 1;
                    exponent += 1;
                    is_f = false;
                } else {
                    // Another digit before the radix.
                    wstart -= 1;
                    w[wstart] = b'1';
                    intdig_no += 1;
                }
                // Rounding added a digit: drop fractional ones beyond the
                // limit.
                if intdig_no + fracdig_no > dig_max {
                    let cut = intdig_no + fracdig_no - dig_max;
                    w.truncate(w.len() - cut as usize);
                    fracdig_no -= cut;
                }
            }
        }
    }

    // Unneeded trailing zeros, and a radix character with nothing after it.
    while fracdig_no > fracdig_min + added_zeros && w.last() == Some(&b'0') {
        w.pop();
        fracdig_no -= 1;
    }
    if fracdig_no == 0 && !alt && w.last() == Some(&b'.') {
        w.pop();
    }

    if !is_f {
        if expsign && exponent == 4 && spec_l == b'g' {
            // Below 1e-4, but 1e-4 once rounded: fixed notation after all.
            let old_end = w.len();
            let n = old_end - wstart;
            let new_end = if n >= 2 { old_end + 4 } else { old_end + 5 };
            w.resize(new_end.max(wstart + 6), b'0');
            w[wstart..wstart + 6].copy_from_slice(b"0.0001");
            for c in &mut w[wstart + 6..new_end] {
                *c = b'0';
            }
            w.truncate(new_end);
        } else {
            w.push(exp_char);
            w.push(if expsign { b'-' } else { b'+' });
            if exponent < 10 {
                w.push(b'0');
            }
            let mut buf = itoa::Buffer::new();
            w.extend_from_slice(buf.format(exponent).as_bytes());
        }
    }
    out.extend_from_slice(&w[wstart..]);
}

/// glibc's `__printf_fphex` for `%a %A` of a finite `x`, its sign already
/// written.
fn fphex<F: Format>(out: &mut Vec<u8>, x: Float<F>, spec: &FloatSpec) {
    let upper = spec.conv == b'A';
    let hex = |d: u8| -> u8 {
        match d {
            0..=9 => b'0' + d,
            _ if upper => b'A' + d - 10,
            _ => b'a' + d - 10,
        }
    };
    let (be, m) = x.encoding();
    let be = i64::from(be);
    // The digits after the leading one, the leading digit, and the binary
    // exponent with its sign.
    let mut num = [0u8; 32];
    let nlen: usize;
    let mut leading: u8;
    let mut exponent: i64;
    let mut expnegative: bool;
    let zero_mantissa: bool;
    if F::EXPLICIT_HEX {
        // The 80-bit format: the top four significand bits lead.
        let n = m as u64;
        zero_mantissa = n == 0;
        nlen = 15;
        leading = hex((n >> 60) as u8);
        for (i, slot) in num[..nlen].iter_mut().enumerate() {
            *slot = hex(((n >> (56 - 4 * i)) & 0xf) as u8);
        }
        let bias = F::BIAS + 3;
        if be == 0 {
            expnegative = !zero_mantissa;
            exponent = if zero_mantissa { 0 } else { F::BIAS + 2 };
        } else if be >= bias {
            expnegative = false;
            exponent = be - bias;
        } else {
            expnegative = true;
            exponent = bias - be;
        }
    } else {
        // An implicit leading bit: "1" (or "0" for subnormals) and the
        // other bits in whole hexadecimal digits.
        let fbits = F::P - 1;
        let n = m & ((1u128 << fbits) - 1);
        zero_mantissa = n == 0;
        nlen = (fbits / 4) as usize;
        leading = if be == 0 { b'0' } else { b'1' };
        for (i, slot) in num[..nlen].iter_mut().enumerate() {
            *slot = hex(((n >> (fbits as usize - 4 - 4 * i)) & 0xf) as u8);
        }
        if be == 0 {
            expnegative = !zero_mantissa;
            exponent = if zero_mantissa { 0 } else { F::BIAS - 1 };
        } else if be >= F::BIAS {
            expnegative = false;
            exponent = be - F::BIAS;
        } else {
            expnegative = true;
            exponent = F::BIAS - be;
        }
    }

    let mut precision: i64 = spec.prec.map_or(-1, |p| p as i64);
    let numend: usize;
    if !zero_mantissa {
        let mut end = nlen;
        while end > 0 && num[end - 1] == b'0' {
            end -= 1;
        }
        numend = end;
        let value = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
        let mut do_round_away = false;
        if precision != -1 && precision < numend as i64 {
            let p = precision as usize;
            let last_digit = if p > 0 { num[p - 1] } else { leading };
            let next = value(num[p]);
            let more_bits = next & 7 != 0 || p + 1 < numend;
            do_round_away = round_away(value(last_digit) & 1 == 1, next >= 8, more_bits);
        }
        if precision == -1 {
            precision = numend as i64;
        } else if do_round_away {
            let mut cnt = precision;
            loop {
                cnt -= 1;
                if cnt < 0 {
                    break;
                }
                let ch = &mut num[cnt as usize];
                if *ch == b'9' {
                    *ch = spec.conv;
                    break;
                } else if ch.to_ascii_lowercase() < b'f' {
                    *ch += 1;
                    break;
                }
                *ch = b'0';
            }
            if cnt < 0 {
                // All digits were f: the leading digit grows.
                if leading == b'9' {
                    leading = spec.conv;
                } else if leading.to_ascii_lowercase() < b'f' {
                    leading += 1;
                } else {
                    leading = b'1';
                    if expnegative {
                        exponent -= 4;
                        if exponent <= 0 {
                            exponent = -exponent;
                            expnegative = false;
                        }
                    } else {
                        exponent += 4;
                    }
                }
            }
        }
    } else {
        if precision == -1 {
            precision = 0;
        }
        numend = 0;
    }

    out.push(b'0');
    out.push(if upper { b'X' } else { b'x' });
    out.push(leading);
    if precision > 0 || spec.flags.alt {
        out.push(b'.');
    }
    if precision > 0 {
        let shown = numend.min(precision as usize);
        out.extend_from_slice(&num[..shown]);
        out.resize(out.len() + (precision as usize - shown), b'0');
    }
    out.push(if upper { b'P' } else { b'p' });
    out.push(if expnegative { b'-' } else { b'+' });
    let mut buf = itoa::Buffer::new();
    out.extend_from_slice(buf.format(exponent).as_bytes());
}

#[cfg(test)]
mod tests {
    use super::super::{Binary64, Binary128, X87, strtold};
    use super::*;

    fn fmt<F: Format>(conv: &str, s: &str) -> String {
        // conv: "[flags][width][.prec]c"
        let b = conv.as_bytes();
        let mut spec = FloatSpec {
            conv: *b.last().unwrap(),
            ..FloatSpec::default()
        };
        let mut i = 0;
        while i < b.len() - 1 {
            match b[i] {
                b'-' => spec.flags.left = true,
                b'0' if spec.width == 0 => spec.flags.zero = true,
                b'+' => spec.flags.plus = true,
                b' ' => spec.flags.space = true,
                b'#' => spec.flags.alt = true,
                _ => break,
            }
            i += 1;
        }
        let rest = std::str::from_utf8(&b[i..b.len() - 1]).unwrap();
        let (w, p) = match rest.split_once('.') {
            Some((w, p)) => (w, Some(p.parse().unwrap_or(0))),
            None => (rest, None),
        };
        spec.width = w.parse().unwrap_or(0);
        spec.prec = p;
        let mut out = Vec::new();
        format_into(&mut out, strtold::<F>(s.as_bytes()).value, &spec);
        String::from_utf8(out).unwrap()
    }

    fn x(conv: &str, s: &str) -> String {
        fmt::<X87>(conv, s)
    }

    #[test]
    fn fixed() {
        assert_eq!(x("f", "0"), "0.000000");
        assert_eq!(x("f", "-0"), "-0.000000");
        assert_eq!(x(".0f", "0.5"), "0");
        assert_eq!(x(".0f", "1.5"), "2");
        assert_eq!(x(".0f", "2.5"), "2");
        assert_eq!(x(".1f", "0.25"), "0.2");
        assert_eq!(x(".1f", "0.35"), "0.3");
        assert_eq!(x(".2f", "99.995"), "100.00");
        assert_eq!(x("#.0f", "3"), "3.");
        assert_eq!(x("09.1f", "-2.5"), "-000002.5");
        assert_eq!(x("-8.2f", "1"), "1.00    ");
        assert_eq!(x(".0f", "1e30"), "1000000000000000000024696061952");
        assert_eq!(x("f", "1e-5000"), "0.000000");
        assert_eq!(x(".20f", "0.1"), "0.10000000000000000000");
        assert_eq!(x(".25f", "0.1"), "0.1000000000000000000013553");
        assert_eq!(x("F", "-inf"), "-INF");
        assert_eq!(x("5f", "nan"), "  nan");
        assert_eq!(x("05f", "-inf"), " -inf");
    }

    #[test]
    fn exponential() {
        assert_eq!(x("e", "0"), "0.000000e+00");
        assert_eq!(x("e", "1e4932"), "1.000000e+4932");
        assert_eq!(x("E", "1.18973149535723176502e+4932"), "1.189731E+4932");
        assert_eq!(x(".3e", "9.9995"), "1.000e+01");
        assert_eq!(x(".0e", "0.95"), "9e-01");
        assert_eq!(x(".0e", "0.96"), "1e+00");
        assert_eq!(x("#.0e", "5"), "5.e+00");
        assert_eq!(x("e", "0x1p-16445"), "3.645200e-4951");
        assert_eq!(
            x(".20e", "3.6451995318824746025e-4951"),
            "3.64519953188247460253e-4951"
        );
        assert_eq!(x("+.1e", "100"), "+1.0e+02");
    }

    #[test]
    fn general() {
        assert_eq!(x("g", "0"), "0");
        assert_eq!(x("#g", "0"), "0.00000");
        assert_eq!(x("g", "100000"), "100000");
        assert_eq!(x("g", "1000000"), "1e+06");
        assert_eq!(x("g", "999999.5"), "1e+06");
        // glibc keeps no zeros here (the digits were integer digits).
        assert_eq!(x("#g", "999999.5"), "1.e+06");
        assert_eq!(x("g", "0.0001"), "0.0001");
        assert_eq!(x("g", "0.0000999999999"), "0.0001");
        assert_eq!(x("#g", "0.0000999999999"), "0.000100000");
        assert_eq!(x("g", "0.00001"), "1e-05");
        assert_eq!(x("g", "1e20"), "1e+20");
        assert_eq!(x(".20g", "0.1"), "0.1");
        assert_eq!(x(".22g", "0.1"), "0.1000000000000000000014");
        assert_eq!(x(".0g", "15"), "2e+01");
        assert_eq!(x("#.0g", "1"), "1.");
        assert_eq!(x("#g", "0.9999999"), "1.00000");
        assert_eq!(x("g", "123.456"), "123.456");
        assert_eq!(x("G", "1e-10"), "1E-10");
        assert_eq!(x("g", "1e-4950"), "1.09356e-4950");
    }

    #[test]
    fn hexadecimal() {
        assert_eq!(x("a", "1"), "0x8p-3");
        assert_eq!(x("a", "0"), "0x0p+0");
        assert_eq!(x("a", "-0"), "-0x0p+0");
        assert_eq!(x("a", "0.1"), "0xc.ccccccccccccccdp-7");
        assert_eq!(x("a", "1e4932"), "0xd.72cb2a95c7ef6cdp+16380");
        assert_eq!(x(".3a", "1.999"), "0xf.fdfp-3");
        assert_eq!(
            x("A", "3.6451995318824746025e-4951"),
            "0X0.000000000000001P-16385"
        );
        assert_eq!(x("#.0a", "1"), "0x8.p-3");
        assert_eq!(x(".0a", "1.5"), "0xcp-3");
        assert_eq!(x(".1a", "1.96875"), "0xf.cp-3");
        assert_eq!(x(".0a", "1.9375"), "0x1p+1");
        assert_eq!(x(".0a", "15.5"), "0x1p+4");
        assert_eq!(x(".0a", "0x1.fp-16385"), "0x2p-16385");
        assert_eq!(x(".2a", "0"), "0x0.00p+0");
        assert_eq!(x("012a", "-1"), "-0x000008p-3");
        assert_eq!(x(".0a", "0x9.8p0"), "0xap+0");
        assert_eq!(x(".0A", "0x9.8p0"), "0XAP+0");
        // Formats with an implicit leading bit.
        assert_eq!(fmt::<Binary64>("a", "1"), "0x1p+0");
        assert_eq!(fmt::<Binary64>("a", "0.1"), "0x1.999999999999ap-4");
        assert_eq!(fmt::<Binary64>(".0a", "1.5"), "0x2p+0");
        assert_eq!(fmt::<Binary64>("a", "4.9e-324"), "0x0.0000000000001p-1022");
        assert_eq!(
            fmt::<Binary128>("a", "0.1"),
            "0x1.999999999999999999999999999ap-4"
        );
    }
}
