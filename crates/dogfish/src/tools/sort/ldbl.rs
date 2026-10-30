//! glibc `strtold` for the x86 80-bit extended format, as `sort -g` uses it.
//!
//! GNU `general_numcompare` converts each key with `strtold` and compares the
//! `long double` results. On x86-64 that is the 80-bit extended format: a
//! 64-bit significand and a 15-bit exponent, so `LDBL_MAX` (1.19e+4932) and
//! `LDBL_MIN` (3.36e-4932) are ordinary values, and two keys that differ in
//! their 19th or 20th significant digit still compare unequal. The
//! conversion is [`crate::common::ldbl::strtold`] (in the 80-bit format on
//! every platform); this module turns its result into one integer that
//! orders like GNU's comparison:
//!
//! - no number at all (a conversion error) first,
//! - then NaNs, `-nan` before `nan` (GNU orders NaNs by their `%Lf`
//!   spelling),
//! - then the numbers by value, with `-0` equal to `0`.

use crate::common::ldbl::{X87, strtold};

/// The key of a string `strtold` does not convert.
pub(super) const NO_NUMBER: i128 = i128::MIN;
const NEG_NAN: i128 = i128::MIN + 1;
const POS_NAN: i128 = i128::MIN + 2;

/// Converts the longest prefix of `s` that `strtold` accepts. Returns the
/// ordering key and the bytes consumed (0, with [`NO_NUMBER`], when no number
/// starts `s`).
pub(super) fn strtold_key(s: &[u8]) -> (i128, usize) {
    let r = strtold::<X87>(s);
    if r.len == 0 {
        return (NO_NUMBER, 0);
    }
    let v = r.value;
    let neg = v.is_sign_negative();
    if v.is_nan() {
        return (if neg { NEG_NAN } else { POS_NAN }, r.len);
    }
    // The biased exponent above the 64-bit significand orders magnitudes.
    let (be, m) = v.encoding();
    let key = (i128::from(be) << 64) | m as i128;
    (if neg { -key } else { key }, r.len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> i128 {
        strtold_key(s.as_bytes()).0
    }

    #[test]
    fn digits_beyond_double_precision_count() {
        assert!(key("1.0000000000000000001") > key("1"));
        assert!(key("1.00000000000000000000001") == key("1"));
        assert!(
            key("123456789012345678901234567890") < key("123456789012345678901234567891e0") + 1
        );
        assert_eq!(key("0"), 0);
        assert_eq!(key("-0"), 0);
    }

    #[test]
    fn order_and_prefixes() {
        assert_eq!(strtold_key(b"abc"), (NO_NUMBER, 0));
        assert_eq!(strtold_key(b" 1.2ignore").1, 4);
        assert_eq!(strtold_key(b"-infinity").1, 9);
        assert_eq!(strtold_key(b"nan(123)x").1, 8);
        assert!(NO_NUMBER < key("-nan") && key("-nan") < key("nan"));
        assert!(key("nan") < key("-inf") && key("-inf") < key("-1e4000"));
        assert!(key("-1") < key("-1e-4940") && key("-1e-4940") < key("0"));
        assert!(key("1e4000") < key("1e5000") || key("1e4000") < key("inf"));
        assert!(key("1e4000") < key("inf"));
        assert_eq!(key("1e5000"), key("inf"));
    }
}
