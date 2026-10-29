//! GNU sleep intervals (sleep, timeout, watch).

use std::time::Duration;

use crate::common::strtod::strtod_prefix;

/// Parses a GNU `sleep`/`timeout` interval: a floating-point number with an
/// optional `s`, `m`, `h` or `d` suffix. As an extension, compound human
/// durations (`500ms`, `1m30s`) are accepted too (see [`parse_compound`]).
pub(crate) fn parse_interval(s: &str) -> Option<f64> {
    if let Some((value, used)) = strtod_prefix(s) {
        let scale = match &s[used..] {
            "" | "s" => Some(1.0),
            "m" => Some(60.0),
            "h" => Some(3600.0),
            "d" => Some(86400.0),
            _ => None,
        };
        if let Some(scale) = scale {
            // `!(0 <= v)` also rejects NaN, like GNU.
            return (value >= 0.0).then_some(value * scale);
        }
    }
    parse_compound(s)
}

/// The extension: one or more `NUMBER UNIT` parts, where UNIT is one of
/// GNU's suffixes or `ms`, `us`/`µs`, `ns` (`500ms`, `1m30s`, `1h30m`).
/// Spelled-out words such as `day` stay invalid, as in GNU.
fn parse_compound(s: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut rest = s;
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let num_len = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let value: f64 = rest[..num_len].parse().ok()?;
        rest = &rest[num_len..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let scale = match &rest[..unit_len] {
            "ns" => 1e-9,
            "us" | "µs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            "d" => 86400.0,
            _ => return None,
        };
        rest = &rest[unit_len..];
        total += value * scale;
    }
    Some(total)
}

pub(crate) fn duration_from_secs(secs: f64) -> Duration {
    Duration::try_from_secs_f64(secs).unwrap_or(Duration::MAX)
}

#[cfg(test)]
mod tests {
    use super::parse_interval;

    #[test]
    fn gnu_suffixes_and_compound_extension() {
        assert_eq!(parse_interval("1.5"), Some(1.5));
        assert_eq!(parse_interval("2m"), Some(120.0));
        assert_eq!(parse_interval("500ms"), Some(0.5));
        assert_eq!(parse_interval("1m30s"), Some(90.0));
        for bad in ["42day", "1days", "42D", "1m30", "", "ms", "1.2.3s", "-1"] {
            assert_eq!(parse_interval(bad), None, "{bad}");
        }
    }
}
