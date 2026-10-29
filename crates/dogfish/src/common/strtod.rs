//! C `strtod` number scanning (printf, seq, sleep, timeout, watch).

/// Parses the longest prefix of `s` that C `strtod` accepts: leading white
/// space, a sign, decimal or hexadecimal digits with an optional fraction and
/// exponent, or `inf`/`infinity`/`nan`. Returns the value and the number of
/// bytes consumed, or `None` when no number starts `s`.
pub(crate) fn strtod_prefix(s: &str) -> Option<(f64, usize)> {
    let b = s.as_bytes();
    let mut i = b
        .iter()
        .take_while(|c| matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
        .count();
    let negative = match b.get(i) {
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
    let signed = |v: f64| if negative { -v } else { v };
    let rest = &s[i..];
    let starts_with_ci = |p: &str| {
        rest.len() >= p.len() && rest.as_bytes()[..p.len()].eq_ignore_ascii_case(p.as_bytes())
    };
    if starts_with_ci("infinity") {
        return Some((signed(f64::INFINITY), i + 8));
    }
    if starts_with_ci("inf") {
        return Some((signed(f64::INFINITY), i + 3));
    }
    if starts_with_ci("nan") {
        let mut end = i + 3;
        if b.get(end) == Some(&b'(')
            && let Some(close) = b[end + 1..]
                .iter()
                .position(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))
            && b[end + 1 + close] == b')'
        {
            end += close + 2;
        }
        return Some((f64::NAN, end));
    }
    let hex_digit_at = |j: usize| b.get(j).is_some_and(u8::is_ascii_hexdigit);
    if b.get(i) == Some(&b'0')
        && matches!(b.get(i + 1), Some(b'x' | b'X'))
        && (hex_digit_at(i + 2) || (b.get(i + 2) == Some(&b'.') && hex_digit_at(i + 3)))
    {
        let mut j = i + 2;
        let mut mantissa = 0f64;
        let mut exp = 0i64;
        while hex_digit_at(j) {
            mantissa = mantissa * 16.0 + f64::from((b[j] as char).to_digit(16).unwrap_or(0));
            j += 1;
        }
        if b.get(j) == Some(&b'.') {
            j += 1;
            while hex_digit_at(j) {
                mantissa = mantissa * 16.0 + f64::from((b[j] as char).to_digit(16).unwrap_or(0));
                exp -= 4;
                j += 1;
            }
        }
        if matches!(b.get(j), Some(b'p' | b'P')) {
            let mut k = j + 1;
            let exp_negative = match b.get(k) {
                Some(b'-') => {
                    k += 1;
                    true
                }
                Some(b'+') => {
                    k += 1;
                    false
                }
                _ => false,
            };
            let digits = b[k..].iter().take_while(|c| c.is_ascii_digit()).count();
            if digits > 0 {
                let e: i64 = s[k..k + digits].parse().unwrap_or(i64::MAX / 2);
                exp += if exp_negative { -e } else { e };
                j = k + digits;
            }
        }
        let value = mantissa * 2f64.powi(exp.clamp(-2200, 2200) as i32);
        return Some((signed(value), j));
    }
    let int_digits = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
    let mut j = i + int_digits;
    let mut frac_digits = 0;
    if b.get(j) == Some(&b'.') {
        frac_digits = b[j + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
        if int_digits + frac_digits > 0 {
            j += 1 + frac_digits;
        }
    }
    if int_digits + frac_digits == 0 {
        return None;
    }
    if matches!(b.get(j), Some(b'e' | b'E')) {
        let mut k = j + 1;
        if matches!(b.get(k), Some(b'-' | b'+')) {
            k += 1;
        }
        let digits = b[k..].iter().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 {
            j = k + digits;
        }
    }
    let value: f64 = s[i..j].parse().ok()?;
    Some((signed(value), j))
}
