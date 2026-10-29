//! gnulib `xstrtoumax` and `xdectoumax`: decimal counts with multiplier
//! suffixes, and GNU's diagnostics for counts that are invalid or too large.

use crate::common::quote::quote_locale;

/// What `xstrtoumax` found wrong (gnulib `strtol_error`, minus `LONGINT_OK`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StrtolError {
    /// No number (or a negative one).
    Invalid,
    /// The number overflowed `u64` (the value saturates).
    Overflow,
    /// Trailing characters that are not a valid suffix.
    InvalidSuffix,
    /// Both of the above.
    InvalidSuffixWithOverflow,
}

/// gnulib `xstrtoumax (s, &end, 10, &val, valid_suffixes)`.
///
/// Leading white space and a `+` are accepted, a `-` is not. With no digits
/// but a valid suffix the number is 1 (`k` is 1024). A suffix letter scales
/// by powers of 1024, or of 1000 when followed by `B` (`kB`) and
/// `valid_suffixes` contains `0`, which also allows `KiB`. Returns the value
/// (saturated on overflow) together with any error, as `xstrtoumax` does.
pub(crate) fn xstrtoumax(s: &str, valid_suffixes: &str) -> (u64, Option<StrtolError>) {
    let bytes = s.as_bytes();
    let start = bytes
        .iter()
        .position(|&b| !matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
        .unwrap_or(bytes.len());
    let mut pos = start;
    match bytes.get(pos) {
        Some(b'-') => return (0, Some(StrtolError::Invalid)),
        Some(b'+') => pos += 1,
        _ => {}
    }
    let digits = bytes[pos..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    let mut overflow = false;
    let mut value: u64 = 0;
    if digits == 0 {
        // No number (strtoumax's end is then `s` itself): a valid suffix
        // alone means 1, anything else is invalid.
        match bytes.first() {
            Some(&c) if valid_suffixes.as_bytes().contains(&c) => {
                value = 1;
                pos = 0;
            }
            _ => return (0, Some(StrtolError::Invalid)),
        }
    } else {
        for &d in &bytes[pos..pos + digits] {
            match value
                .checked_mul(10)
                .and_then(|v| v.checked_add(u64::from(d - b'0')))
            {
                Some(v) => value = v,
                None => {
                    overflow = true;
                    value = u64::MAX;
                }
            }
        }
        pos += digits;
    }
    let with = |overflow: bool, bad_suffix: bool| match (overflow, bad_suffix) {
        (false, false) => None,
        (true, false) => Some(StrtolError::Overflow),
        (false, true) => Some(StrtolError::InvalidSuffix),
        (true, true) => Some(StrtolError::InvalidSuffixWithOverflow),
    };
    let Some(&suffix) = bytes.get(pos) else {
        return (value, with(overflow, false));
    };
    if !valid_suffixes.as_bytes().contains(&suffix) {
        return (value, with(overflow, true));
    }
    let mut base: u64 = 1024;
    let mut consumed = 1;
    if b"EGgkKMmPQRTtYZ".contains(&suffix) && valid_suffixes.contains('0') {
        match bytes.get(pos + 1) {
            Some(b'i') if bytes.get(pos + 2) == Some(&b'B') => consumed = 3,
            Some(b'B') | Some(b'D') => {
                base = 1000;
                consumed = 2;
            }
            _ => {}
        }
    }
    let (factor, power) = match suffix {
        b'b' => (512, 1),
        b'B' => (1024, 1),
        b'c' => (1, 1),
        b'w' => (2, 1),
        b'k' | b'K' => (base, 1),
        b'M' | b'm' => (base, 2),
        b'G' | b'g' => (base, 3),
        b'T' | b't' => (base, 4),
        b'P' => (base, 5),
        b'E' => (base, 6),
        b'Z' => (base, 7),
        b'Y' => (base, 8),
        b'R' => (base, 9),
        b'Q' => (base, 10),
        _ => return (value, with(overflow, true)),
    };
    for _ in 0..power {
        match value.checked_mul(factor) {
            Some(v) => value = v,
            None => {
                overflow = true;
                value = u64::MAX;
            }
        }
    }
    (value, with(overflow, pos + consumed != bytes.len()))
}

/// gnulib `xdectoumax`: parses `s` and checks it against `min..=max`; on
/// failure returns GNU's message, `ERR: 'S'` plus `: Value too large for
/// defined data type` (or `: Numerical result out of range`) when the value
/// is out of range, for the caller to print and exit 1.
pub(crate) fn xdectoumax(
    s: &str,
    min: u64,
    max: u64,
    valid_suffixes: &str,
    err: &str,
) -> Result<u64, String> {
    let (value, error) = xstrtoumax(s, valid_suffixes);
    let errno_text = match error {
        None if (min..=max).contains(&value) => return Ok(value),
        // xnumtoint's heuristic: a value beyond INT_MAX / 2 is a type overflow.
        None if value > (i32::MAX / 2) as u64 => Some("Value too large for defined data type"),
        None => Some("Numerical result out of range"),
        Some(StrtolError::Overflow) => Some("Value too large for defined data type"),
        Some(_) => None,
    };
    let mut msg = format!("{err}: {}", quote_locale(s));
    if let Some(text) = errno_text {
        msg.push_str(": ");
        msg.push_str(text);
    }
    Err(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "bkKmMGTPEZYRQ0";

    #[test]
    fn plain_and_suffixed_counts() {
        assert_eq!(xstrtoumax("12", HEAD), (12, None));
        assert_eq!(xstrtoumax(" +3", HEAD), (3, None));
        assert_eq!(xstrtoumax("2b", HEAD), (1024, None));
        assert_eq!(xstrtoumax("1k", HEAD), (1024, None));
        assert_eq!(xstrtoumax("1kB", HEAD), (1000, None));
        assert_eq!(xstrtoumax("1KiB", HEAD), (1024, None));
        assert_eq!(xstrtoumax("k", HEAD), (1024, None));
        assert_eq!(xstrtoumax("1M", HEAD), (1 << 20, None));
    }

    #[test]
    fn errors() {
        assert_eq!(xstrtoumax("-1", HEAD).1, Some(StrtolError::Invalid));
        assert_eq!(xstrtoumax("", HEAD).1, Some(StrtolError::Invalid));
        assert_eq!(xstrtoumax("1x", HEAD).1, Some(StrtolError::InvalidSuffix));
        assert_eq!(xstrtoumax("1g", HEAD).1, Some(StrtolError::InvalidSuffix));
        assert_eq!(xstrtoumax("1kx", HEAD).1, Some(StrtolError::InvalidSuffix));
        assert_eq!(
            xstrtoumax("99999999999999999999", HEAD),
            (u64::MAX, Some(StrtolError::Overflow))
        );
        assert_eq!(
            xstrtoumax("99999999999999999999x", HEAD).1,
            Some(StrtolError::InvalidSuffixWithOverflow)
        );
        assert_eq!(xstrtoumax("1Q", HEAD).1, Some(StrtolError::Overflow));
    }

    #[test]
    fn messages() {
        let e = "invalid number of lines";
        assert_eq!(
            xdectoumax("1x", 0, u64::MAX, HEAD, e),
            Err("invalid number of lines: '1x'".to_string())
        );
        assert_eq!(
            xdectoumax("99999999999999999999", 0, u64::MAX, HEAD, e),
            Err("invalid number of lines: '99999999999999999999': \
                 Value too large for defined data type"
                .to_string())
        );
        assert_eq!(
            xdectoumax("5", 1, 3, "", e),
            Err("invalid number of lines: '5': Numerical result out of range".to_string())
        );
    }
}
