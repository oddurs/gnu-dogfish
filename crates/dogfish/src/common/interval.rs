//! GNU sleep intervals (sleep, timeout, watch).

use std::time::Duration;

use dogfish_core::parse_duration_str;

use crate::common::strtod::strtod_prefix;

/// Parses a GNU `sleep`/`timeout` interval: a floating-point number with an
/// optional `s`, `m`, `h` or `d` suffix. As an extension, compound human
/// durations (`500ms`, `1m30s`) are accepted too.
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
    let human = !s.is_empty()
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'.' || !c.is_ascii());
    human
        .then(|| parse_duration_str(s).ok())
        .flatten()
        .map(|d| d.as_secs_f64())
}

pub(crate) fn duration_from_secs(secs: f64) -> Duration {
    Duration::try_from_secs_f64(secs).unwrap_or(Duration::MAX)
}
