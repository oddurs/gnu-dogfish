//! gnulib `posix2_version`: the POSIX version a tool conforms to, which
//! decides whether obsolete syntax such as `tail +2` or `uniq +1` is accepted.

/// `_POSIX2_VERSION` from the environment when it is a whole number, else
/// 200809 (POSIX.1-2008), clamped to the `int` range as in gnulib.
pub(crate) fn posix2_version() -> i64 {
    let v = std::env::var("_POSIX2_VERSION")
        .ok()
        .filter(|s| !s.is_empty())
        .and_then(|s| parse_strtol(&s))
        .unwrap_or(200809);
    v.clamp(i64::from(i32::MIN), i64::from(i32::MAX))
}

/// `strtol (s, &e, 10)` that must consume the whole string; saturates like
/// strtol on overflow.
fn parse_strtol(s: &str) -> Option<i64> {
    let t = s.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mag = digits.parse::<i64>().unwrap_or(i64::MAX);
    Some(if neg { -mag } else { mag })
}

#[cfg(test)]
mod tests {
    use super::parse_strtol;

    #[test]
    fn strtol_whole_string() {
        assert_eq!(parse_strtol("199209"), Some(199209));
        assert_eq!(parse_strtol(" -5"), Some(-5));
        assert_eq!(parse_strtol("12x"), None);
        assert_eq!(parse_strtol("+"), None);
    }
}
