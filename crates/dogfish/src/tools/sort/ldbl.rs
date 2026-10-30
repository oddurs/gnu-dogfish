//! glibc `strtold` for the x86 80-bit extended format, as `sort -g` uses it.
//!
//! GNU `general_numcompare` converts each key with `strtold` and compares the
//! `long double` results. On x86-64 that is the 80-bit extended format: a
//! 64-bit significand and a 15-bit exponent, so `LDBL_MAX` (1.19e+4932) and
//! `LDBL_MIN` (3.36e-4932) are ordinary values, and two keys that differ in
//! their 19th or 20th significant digit still compare unequal. This module
//! rounds a decimal or hexadecimal number to that format exactly as glibc
//! does (round to nearest, ties to even, with gradual underflow) and returns
//! the result as one integer that orders like GNU's comparison:
//!
//! - no number at all (a conversion error) first,
//! - then NaNs, `-nan` before `nan` (GNU orders NaNs by their `%Lf`
//!   spelling),
//! - then the numbers by value, with `-0` equal to `0`.

/// The key of a string `strtold` does not convert.
pub(super) const NO_NUMBER: i128 = i128::MIN;
const NEG_NAN: i128 = i128::MIN + 1;
const POS_NAN: i128 = i128::MIN + 2;

/// Biased exponent of infinity, and the bias.
const EXP_INF: i128 = 0x7fff;
const BIAS: i64 = 16383;
/// Exponent of the least significant bit of the smallest subnormal.
const MIN_LSB: i64 = -16445;

/// Significant decimal digits kept: enough for every halfway case of the
/// format (about 11,500 digits), the rest only decide a sticky bit.
const MAX_DIGITS: usize = 12_000;

/// C `isspace` in the C locale.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Converts the longest prefix of `s` that `strtold` accepts. Returns the
/// ordering key and the bytes consumed (0, with [`NO_NUMBER`], when no number
/// starts `s`).
pub(super) fn strtold_key(s: &[u8]) -> (i128, usize) {
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
    let rest = &s[i..];
    let starts_ci = |p: &[u8]| rest.len() >= p.len() && rest[..p.len()].eq_ignore_ascii_case(p);
    if starts_ci(b"inf") {
        let len = if starts_ci(b"infinity") { 8 } else { 3 };
        let inf = (EXP_INF << 64) | (1 << 63);
        return (if neg { -inf } else { inf }, i + len);
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
        return (if neg { NEG_NAN } else { POS_NAN }, end);
    }
    let hex_at = |j: usize| s.get(j).is_some_and(u8::is_ascii_hexdigit);
    if rest.len() > 2
        && rest[0] == b'0'
        && (rest[1] | 0x20) == b'x'
        && (hex_at(i + 2) || (s.get(i + 2) == Some(&b'.') && hex_at(i + 3)))
    {
        return hex(s, i + 2, neg);
    }
    decimal(s, i, neg)
}

/// A hexadecimal significand from `s[start..]` (after `0x`), with an
/// optional binary exponent.
fn hex(s: &[u8], start: usize, neg: bool) -> (i128, usize) {
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
    let key = round(m, exp.saturating_add(e), sticky);
    (if neg { -key } else { key }, end)
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

/// A decimal number at `s[start..]`.
fn decimal(s: &[u8], start: usize, neg: bool) -> (i128, usize) {
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
        return (NO_NUMBER, 0);
    }
    let (e, end) = exponent(s, j, b'e');
    if digits.is_empty() {
        return (0, end);
    }
    if sticky {
        // A nonzero digit beyond those kept: one more digit 1 rounds the
        // same way as the whole tail.
        digits.push(1);
        exp10 -= 1;
    }
    let exp10 = exp10.saturating_add(e);
    let key = decimal_value(&digits, exp10);
    (if neg { -key } else { key }, end)
}

/// The key of `digits × 10^exp10` (no leading zero digit, at least one
/// digit).
fn decimal_value(digits: &[u8], exp10: i64) -> i128 {
    let magnitude = exp10.saturating_add(digits.len() as i64);
    if magnitude > 4934 {
        return round(1, 1 << 20, false); // infinity
    }
    if magnitude < -4952 {
        return 0; // below half the smallest subnormal
    }
    if digits.len() <= 19 {
        let m = digits.iter().fold(0u64, |v, &d| v * 10 + u64::from(d));
        if (0..=19).contains(&exp10) {
            return round(u128::from(m) * 10u128.pow(exp10 as u32), 0, false);
        }
        if (-18..0).contains(&exp10) {
            let p = 10u128.pow(exp10.unsigned_abs() as u32);
            let s = 127 - (64 - m.leading_zeros());
            let n = u128::from(m) << s;
            return round(n / p, -i64::from(s), n % p != 0);
        }
    }
    let mut d = Big::from_digits(digits);
    if exp10 >= 0 {
        d.mul_pow10(exp10 as u64);
        let (m, e, sticky) = d.top128();
        return round(m, e, sticky);
    }
    let mut p = Big::from_digits(&[1]);
    p.mul_pow10(exp10.unsigned_abs());
    // Scale so that the quotient has 68 or 69 bits.
    let sh = 68 - (d.bits() as i64 - p.bits() as i64);
    if sh > 0 {
        d.shl(sh as u64);
    } else {
        p.shl(sh.unsigned_abs());
    }
    let (q, rem_nonzero) = d.div_small_quotient(&p);
    round(q, -sh, rem_nonzero)
}

/// Rounds `m × 2^e` (plus a nonzero tail below it when `sticky`) to the
/// extended format; returns the magnitude's key.
fn round(m: u128, e: i64, sticky: bool) -> i128 {
    if m == 0 {
        return 0;
    }
    let bits = i64::from(128 - m.leading_zeros());
    let top = e.saturating_add(bits - 1);
    let mut lsb = (top - 63).max(MIN_LSB);
    let shift = lsb.saturating_sub(e);
    let mut mant: u128 = if shift <= 0 {
        m << shift.unsigned_abs()
    } else {
        let mant = if shift >= 128 { 0 } else { m >> shift };
        let (half, low) = if shift > 128 {
            (false, m != 0)
        } else {
            let h = shift - 1;
            ((m >> h) & 1 == 1, m & ((1u128 << h) - 1) != 0)
        };
        if half && (sticky || low || mant & 1 == 1) {
            mant + 1
        } else {
            mant
        }
    };
    if mant == 1 << 64 {
        mant = 1 << 63;
        lsb += 1;
    }
    if mant >> 63 == 0 {
        return mant as i128; // subnormal (biased exponent 0)
    }
    let biased = i128::from(lsb + 63 + BIAS);
    if biased >= EXP_INF {
        return (EXP_INF << 64) | (1 << 63);
    }
    (biased << 64) | mant as i128
}

/// A minimal unsigned big integer (little-endian 32-bit limbs) for the
/// conversions the fast paths cannot do exactly.
struct Big(Vec<u32>);

impl Big {
    fn from_digits(digits: &[u8]) -> Self {
        let mut b = Big(vec![0]);
        for chunk in digits.chunks(9) {
            let v = chunk.iter().fold(0u32, |v, &d| v * 10 + u32::from(d));
            b.mul_add(10u32.pow(chunk.len() as u32), v);
        }
        b
    }

    fn mul_add(&mut self, mul: u32, add: u32) {
        let mut carry = u64::from(add);
        for limb in &mut self.0 {
            let v = u64::from(*limb) * u64::from(mul) + carry;
            *limb = v as u32;
            carry = v >> 32;
        }
        if carry != 0 {
            self.0.push(carry as u32);
        }
    }

    fn mul_pow10(&mut self, mut k: u64) {
        while k >= 9 {
            self.mul_add(1_000_000_000, 0);
            k -= 9;
        }
        self.mul_add(10u32.pow(k as u32), 0);
    }

    fn bits(&self) -> u64 {
        let mut n = self.0.len();
        while n > 0 && self.0[n - 1] == 0 {
            n -= 1;
        }
        if n == 0 {
            return 0;
        }
        (n as u64 - 1) * 32 + u64::from(32 - self.0[n - 1].leading_zeros())
    }

    fn bit(&self, i: u64) -> bool {
        self.0
            .get((i / 32) as usize)
            .is_some_and(|l| (l >> (i % 32)) & 1 == 1)
    }

    fn shl(&mut self, n: u64) {
        let limbs = (n / 32) as usize;
        let bits = (n % 32) as u32;
        if bits != 0 {
            let mut carry = 0u32;
            for limb in &mut self.0 {
                let v = *limb;
                *limb = (v << bits) | carry;
                carry = v >> (32 - bits);
            }
            if carry != 0 {
                self.0.push(carry);
            }
        }
        if limbs != 0 {
            self.0.splice(0..0, std::iter::repeat_n(0, limbs));
        }
    }

    fn shr1(&mut self) {
        let mut carry = 0u32;
        for limb in self.0.iter_mut().rev() {
            let v = *limb;
            *limb = (v >> 1) | (carry << 31);
            carry = v & 1;
        }
    }

    fn cmp(&self, other: &Big) -> std::cmp::Ordering {
        let n = self.0.len().max(other.0.len());
        for i in (0..n).rev() {
            let a = self.0.get(i).copied().unwrap_or(0);
            let b = other.0.get(i).copied().unwrap_or(0);
            if a != b {
                return a.cmp(&b);
            }
        }
        std::cmp::Ordering::Equal
    }

    fn sub_assign(&mut self, other: &Big) {
        let mut borrow = 0i64;
        for i in 0..self.0.len() {
            let v = i64::from(self.0[i]) - i64::from(other.0.get(i).copied().unwrap_or(0)) - borrow;
            borrow = i64::from(v < 0);
            self.0[i] = v.rem_euclid(1 << 32) as u32;
        }
    }

    fn is_zero(&self) -> bool {
        self.0.iter().all(|&l| l == 0)
    }

    /// `self / d` for a quotient below 2^127, and whether a remainder is left.
    fn div_small_quotient(mut self, d: &Big) -> (u128, bool) {
        let t = self.bits().saturating_sub(d.bits()).min(126);
        let mut dd = Big(d.0.clone());
        dd.shl(t);
        let mut q: u128 = 0;
        for i in (0..=t).rev() {
            if self.cmp(&dd) != std::cmp::Ordering::Less {
                self.sub_assign(&dd);
                q |= 1 << i;
            }
            dd.shr1();
        }
        (q, !self.is_zero())
    }

    /// The leading 128 bits as `m × 2^e`, and whether any bit below them is set.
    fn top128(&self) -> (u128, i64, bool) {
        let bits = self.bits();
        let drop = bits.saturating_sub(128);
        let mut m: u128 = 0;
        for i in (drop..bits).rev() {
            m = (m << 1) | u128::from(self.bit(i));
        }
        let sticky = (0..drop).any(|i| self.bit(i));
        (m, drop as i64, sticky)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> i128 {
        strtold_key(s.as_bytes()).0
    }

    /// The 80-bit encoding (biased exponent, significand) of `s`.
    fn parts(s: &str) -> (i128, u64) {
        let k = key(s);
        (k >> 64, k as u64)
    }

    #[test]
    fn exact_values() {
        assert_eq!(parts("1"), (16383, 1 << 63));
        assert_eq!(parts("2"), (16384, 1 << 63));
        assert_eq!(parts("0.5"), (16382, 1 << 63));
        assert_eq!(parts("0x1p-1"), (16382, 1 << 63));
        assert_eq!(parts("1.5"), (16383, 3 << 62));
        assert_eq!(key("0"), 0);
        assert_eq!(key("-0"), 0);
        assert_eq!(key("0e999999"), 0);
    }

    #[test]
    fn rounding_matches_glibc() {
        // 0.1L = 0xc.ccccccccccccccdp-7
        assert_eq!(parts("0.1"), (16379, 0xcccc_cccc_cccc_cccd));
        assert_eq!(parts("1e-10"), parts("0.0000000001"));
        // LDBL_MAX and LDBL_MIN as getlimits prints them.
        assert_eq!(parts("1.189731495357231765e+4932"), (0x7ffe, u64::MAX));
        assert_eq!(parts("3.3621031431120935063e-4932"), (1, 1 << 63));
        // The smallest subnormal, and half of it (ties to even: zero).
        assert_eq!(key("3.6451995318824746025e-4951"), 1);
        assert_eq!(key("1.8225997659412373012e-4951"), 0);
        assert_eq!(parts("1e5000").0, EXP_INF);
        assert_eq!(key("1e-5000"), 0);
    }

    #[test]
    fn digits_beyond_double_precision_count() {
        assert!(key("1.0000000000000000001") > key("1"));
        assert!(key("1.00000000000000000000001") == key("1"));
        assert!(
            key("123456789012345678901234567890") < key("123456789012345678901234567891e0") + 1
        );
    }

    #[test]
    fn order_and_prefixes() {
        assert_eq!(strtold_key(b"abc"), (NO_NUMBER, 0));
        assert_eq!(strtold_key(b" 1.2ignore").1, 4);
        assert_eq!(strtold_key(b" 1.1e4ignore").1, 6);
        assert_eq!(strtold_key(b"1e").1, 1);
        assert_eq!(strtold_key(b"0x").1, 1);
        assert_eq!(strtold_key(b"-infinity").1, 9);
        assert_eq!(strtold_key(b"nan(123)x").1, 8);
        assert!(NO_NUMBER < key("-nan") && key("-nan") < key("nan"));
        assert!(key("nan") < key("-inf") && key("-inf") < key("-1e4000"));
        assert!(key("-1") < key("-1e-4940") && key("-1e-4940") < key("0"));
        assert!(key("1e4000") < key("inf"));
    }
}
