//! glibc `strtold`: the longest prefix of a string that is a C
//! floating-point number, rounded to a [`Format`] exactly as glibc rounds
//! (to nearest, ties to even, with gradual underflow), and whether glibc
//! reports `ERANGE` for it.

use super::big::Big;
use super::{Float, Format, Status};

/// Significant decimal digits kept: enough for every halfway case of the
/// formats (about 11,600 digits for binary128), the rest only decide a
/// sticky bit.
const MAX_DIGITS: usize = 12_000;

/// What [`strtold`] converted.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Strtold<F: Format> {
    pub(crate) value: Float<F>,
    /// The bytes converted: 0 when no number starts the string (and the
    /// value is 0).
    pub(crate) len: usize,
    /// glibc sets `errno` to `ERANGE`: the number overflowed, or underflowed
    /// with a loss of precision.
    pub(crate) erange: bool,
}

/// C `isspace` in the C locale.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Converts the longest prefix of `s` that `strtold` accepts: leading white
/// space, a sign, decimal or hexadecimal digits with an optional fraction and
/// exponent, or `inf`, `infinity`, `nan`, `nan(chars)`.
pub(crate) fn strtold<F: Format>(s: &[u8]) -> Strtold<F> {
    let mut i = s.iter().take_while(|&&c| is_space(c)).count();
    let neg = match s.get(i) {
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
    let done = |value, len| Strtold {
        value,
        len,
        erange: false,
    };
    let rest = &s[i..];
    let starts_ci = |p: &[u8]| rest.len() >= p.len() && rest[..p.len()].eq_ignore_ascii_case(p);
    if starts_ci(b"inf") {
        let len = if starts_ci(b"infinity") { 8 } else { 3 };
        return done(Float::infinity(neg), i + len);
    }
    if starts_ci(b"nan") {
        let mut end = i + 3;
        if s.get(end) == Some(&b'(') {
            let n = s[end + 1..]
                .iter()
                .take_while(|c| c.is_ascii_alphanumeric() || **c == b'_')
                .count();
            if s.get(end + 1 + n) == Some(&b')') {
                end += n + 2;
            }
        }
        return done(Float::nan(neg), end);
    }
    let hex_at = |j: usize| s.get(j).is_some_and(u8::is_ascii_hexdigit);
    let (rounded, len) = if rest.len() > 2
        && rest[0] == b'0'
        && (rest[1] | 0x20) == b'x'
        && (hex_at(i + 2) || (s.get(i + 2) == Some(&b'.') && hex_at(i + 3)))
    {
        hex(s, i + 2, neg)
    } else {
        match decimal(s, i, neg) {
            Some(r) => r,
            None => return done(Float::zero(false), 0),
        }
    };
    let (value, status) = rounded;
    Strtold {
        value,
        len,
        erange: status.erange(),
    }
}

/// A hexadecimal significand from `s[start..]` (after `0x`), with an
/// optional binary exponent.
fn hex<F: Format>(s: &[u8], start: usize, neg: bool) -> ((Float<F>, Status), usize) {
    let mut j = start;
    // The leading 124 significant bits, then a sticky bit for the rest.
    let mut m: u128 = 0;
    let mut exp: i64 = 0;
    let mut sticky = false;
    let mut seen_dot = false;
    loop {
        match s.get(j) {
            Some(&c) if c.is_ascii_hexdigit() => {
                let d = u128::from((c as char).to_digit(16).unwrap_or(0));
                if m >> 120 == 0 {
                    m = (m << 4) | d;
                    if seen_dot {
                        exp -= 4;
                    }
                } else {
                    sticky |= d != 0;
                    if !seen_dot {
                        exp += 4;
                    }
                }
            }
            Some(b'.') if !seen_dot => seen_dot = true,
            _ => break,
        }
        j += 1;
    }
    let (e, end) = exponent(s, j, b'p');
    (Float::round(neg, m, exp.saturating_add(e), sticky), end)
}

/// The value of an exponent part (`e-12`, `p+3`) at `s[j..]`, saturated, and
/// where the number ends: an exponent marker without digits is not part of it.
fn exponent(s: &[u8], j: usize, marker: u8) -> (i64, usize) {
    if s.get(j).map(|c| c | 0x20) != Some(marker) {
        return (0, j);
    }
    let mut k = j + 1;
    let eneg = s.get(k) == Some(&b'-');
    if matches!(s.get(k), Some(b'+' | b'-')) {
        k += 1;
    }
    let digits = s[k.min(s.len())..]
        .iter()
        .take_while(|c| c.is_ascii_digit())
        .count();
    if digits == 0 {
        return (0, j);
    }
    let mut e: i64 = 0;
    for &c in &s[k..k + digits] {
        e = e.saturating_mul(10).saturating_add(i64::from(c - b'0'));
    }
    // Far beyond any finite or subnormal value either way.
    let e = e.min(1 << 40);
    (if eneg { -e } else { e }, k + digits)
}

/// A decimal number at `s[start..]`, or `None` without a digit.
fn decimal<F: Format>(s: &[u8], start: usize, neg: bool) -> Option<((Float<F>, Status), usize)> {
    let mut j = start;
    let mut digits: Vec<u8> = Vec::new();
    // Value = digits × 10^exp10; leading zeros are not kept.
    let mut exp10: i64 = 0;
    let mut any = false;
    let mut sticky = false;
    let mut seen_dot = false;
    loop {
        match s.get(j) {
            Some(&c) if c.is_ascii_digit() => {
                any = true;
                if digits.is_empty() && c == b'0' {
                    if seen_dot {
                        exp10 -= 1;
                    }
                } else if digits.len() < MAX_DIGITS {
                    digits.push(c - b'0');
                    if seen_dot {
                        exp10 -= 1;
                    }
                } else {
                    sticky |= c != b'0';
                    if !seen_dot {
                        exp10 += 1;
                    }
                }
            }
            Some(b'.') if !seen_dot => seen_dot = true,
            _ => break,
        }
        j += 1;
    }
    if !any {
        return None;
    }
    let (e, end) = exponent(s, j, b'e');
    if digits.is_empty() {
        return Some(((Float::zero(neg), Status::default()), end));
    }
    if sticky {
        // A nonzero digit beyond those kept: one more digit 1 rounds the
        // same way as the whole tail.
        digits.push(1);
        exp10 -= 1;
    }
    let exp10 = exp10.saturating_add(e);
    Some((decimal_value(neg, &digits, exp10), end))
}

/// `±digits × 10^exp10` (no leading zero digit, at least one digit), rounded.
fn decimal_value<F: Format>(neg: bool, digits: &[u8], exp10: i64) -> (Float<F>, Status) {
    let magnitude = exp10.saturating_add(digits.len() as i64);
    if magnitude > F::MAX_MAG {
        return Float::round(neg, 1, 1 << 20, false); // overflow
    }
    if magnitude < F::MIN_MAG {
        // Below half the smallest subnormal.
        return Float::round(neg, 0, 0, true);
    }
    if digits.len() <= 19 {
        let m = digits.iter().fold(0u64, |v, &d| v * 10 + u64::from(d));
        if (0..=19).contains(&exp10) {
            return Float::round(neg, u128::from(m) * 10u128.pow(exp10 as u32), 0, false);
        }
        // The quotient has at least 68 bits, enough to round 64.
        if F::P <= 64 && (-18..0).contains(&exp10) {
            let p = 10u128.pow(exp10.unsigned_abs() as u32);
            let s = 127 - (64 - m.leading_zeros());
            let n = u128::from(m) << s;
            return Float::round(neg, n / p, -i64::from(s), n % p != 0);
        }
    }
    let mut d = Big::from_digits(digits);
    if exp10 >= 0 {
        d.mul_pow10(exp10 as u64);
        let (m, e, sticky) = d.top128();
        return Float::round(neg, m, e, sticky);
    }
    let mut p = Big::from_digits(&[1]);
    p.mul_pow10(exp10.unsigned_abs());
    // Scale so that the quotient has P + 4 or P + 5 bits.
    let sh = i64::from(F::P) + 4 - (d.bits() as i64 - p.bits() as i64);
    if sh > 0 {
        d.shl(sh as u64);
    } else {
        p.shl(sh.unsigned_abs());
    }
    let (q, rem_nonzero) = d.div_small_quotient(&p);
    Float::round(neg, q, -sh, rem_nonzero)
}

#[cfg(test)]
mod tests {
    use super::super::{Binary64, Binary128, X87};
    use super::*;

    fn parse<F: Format>(s: &str) -> Strtold<F> {
        strtold::<F>(s.as_bytes())
    }

    /// The 80-bit encoding (biased exponent, significand) of `s`.
    fn parts(s: &str) -> (i32, u128) {
        parse::<X87>(s).value.encoding()
    }

    #[test]
    fn exact_values() {
        assert_eq!(parts("1"), (16383, 1 << 63));
        assert_eq!(parts("2"), (16384, 1 << 63));
        assert_eq!(parts("0.5"), (16382, 1 << 63));
        assert_eq!(parts("0x1p-1"), (16382, 1 << 63));
        assert_eq!(parts("1.5"), (16383, 3 << 62));
        assert_eq!(parts("0"), (0, 0));
        assert!(parse::<X87>("-0").value.is_sign_negative());
        assert_eq!(parts("0e999999"), (0, 0));
        assert!(!parse::<X87>("0e999999").erange);
    }

    #[test]
    fn rounding_matches_glibc() {
        // 0.1L = 0xc.ccccccccccccccdp-7
        assert_eq!(parts("0.1"), (16379, 0xcccc_cccc_cccc_cccd));
        assert_eq!(parts("1e-10"), parts("0.0000000001"));
        // LDBL_MAX and LDBL_MIN as getlimits prints them.
        assert_eq!(
            parts("1.189731495357231765e+4932"),
            (0x7ffe, u128::from(u64::MAX))
        );
        assert_eq!(parts("3.3621031431120935063e-4932"), (1, 1 << 63));
        // The smallest subnormal, and half of it (ties to even: zero).
        assert_eq!(parts("3.6451995318824746025e-4951"), (0, 1));
        assert_eq!(parts("1.8225997659412373012e-4951"), (0, 0));
        assert!(parse::<X87>("1e5000").value.is_infinite());
        assert_eq!(parts("1e-5000"), (0, 0));
        // binary128 and double.
        assert_eq!(
            parse::<Binary128>("0.1").value.encoding(),
            (16379, 0x1_9999_9999_9999_9999_9999_9999_999a)
        );
        assert_eq!(
            parse::<Binary64>("0.1").value.to_f64().to_bits(),
            0.1f64.to_bits()
        );
        assert_eq!(parse::<Binary64>("4.9e-324").value.encoding(), (0, 1));
        assert_eq!(
            parse::<Binary64>("1.7976931348623157e308").value.to_f64(),
            f64::MAX
        );
    }

    #[test]
    fn erange_as_glibc_reports_it() {
        let erange = |s: &str| parse::<X87>(s).erange;
        assert!(erange("1e5000"));
        assert!(erange("-1e5000"));
        assert!(erange("0x1p16384"));
        assert!(!erange("1.189731495357231765e+4932"));
        assert!(erange("1e-5000"));
        assert!(erange("1e-4940"));
        assert!(erange("0x1p-16446"));
        // Exact subnormals are not an error.
        assert!(!erange("0x1p-16445"));
        assert!(!erange("0x3p-16440"));
        assert!(!erange("inf") && !erange("nan") && !erange("0"));
        // Tininess after rounding: just below the smallest normal number,
        // rounding up to it, is tiny only if 64 bits hold it exactly.
        assert!(erange("0x1.fffffffffffffffep-16383"));
        assert!(!erange("0x1.ffffffffffffffffp-16383"));
        assert!(!erange("3.3621031431120935062e-4932"));
        assert!(erange("3.362103143112093506e-4932"));
    }

    #[test]
    fn prefixes() {
        let len = |s: &str| parse::<X87>(s).len;
        assert_eq!(len("abc"), 0);
        assert_eq!(len(""), 0);
        assert_eq!(len("-"), 0);
        assert_eq!(len("."), 0);
        assert_eq!(len(" 1.2ignore"), 4);
        assert_eq!(len(" 1.1e4ignore"), 6);
        assert_eq!(len("1e"), 1);
        assert_eq!(len("0x"), 1);
        assert_eq!(len("0x.p1"), 1);
        assert_eq!(len("-infinity"), 9);
        assert_eq!(len("infinit"), 3);
        assert_eq!(len("nan(123)x"), 8);
        assert_eq!(len("nan(12"), 3);
    }
}
