//! GNU `human_readable` number formatting and `--block-size` parsing (df, du, ls).

use anyhow::Result;

use crate::common::quote::{Quoting, quote_bytes};

pub(crate) const HUMAN_AUTOSCALE: u32 = 1;
pub(crate) const HUMAN_SI: u32 = 2;
pub(crate) const HUMAN_BASE_1024: u32 = 4;
pub(crate) const HUMAN_B: u32 = 8;
pub(crate) const HUMAN_SUPPRESS_POINT_ZERO: u32 = 16;

/// Size/prefix letters indexed by power, as in gnulib (`k` is used for 1000¹).
const POWER_LETTERS: [char; 11] = ['\0', 'K', 'M', 'G', 'T', 'P', 'E', 'Z', 'Y', 'R', 'Q'];

/// gnulib `human_readable` with its default (ceiling) rounding: converts `n`
/// units of `from` bytes into units of `to` bytes, autoscaling with `-h`/`--si`.
pub(crate) fn human_readable(n: u64, opts: u32, from: u64, to: u64) -> String {
    let autoscale = opts & HUMAN_AUTOSCALE != 0;
    let base: u64 = if opts & HUMAN_BASE_1024 != 0 {
        1024
    } else {
        1000
    };
    let max_exp = POWER_LETTERS.len() as i32 - 1;
    let mut exponent: i32 = -1;

    let mut start = None;
    if to <= from {
        if from.is_multiple_of(to) {
            start = n.checked_mul(from / to).map(|amount| (amount, 0, 0));
        }
    } else if from != 0 && to.is_multiple_of(from) {
        let divisor = u128::from(to / from);
        let n = u128::from(n);
        let r10 = (n % divisor) * 10;
        let r2 = (r10 % divisor) * 2;
        let rounding = if r2 < divisor {
            u64::from(0 < r2)
        } else {
            2 + u64::from(divisor < r2)
        };
        start = Some(((n / divisor) as u64, (r10 / divisor) as u64, rounding));
    }
    let Some((mut amount, mut tenths, mut rounding)) = start else {
        return human_readable_float(n, opts, from, to);
    };

    let mut fraction = String::new();
    if autoscale {
        exponent = 0;
        if base <= amount {
            loop {
                let r10 = (amount % base) * 10 + tenths;
                let r2 = (r10 % base) * 2 + (rounding >> 1);
                amount /= base;
                tenths = r10 / base;
                rounding = if r2 < base {
                    u64::from(r2 + rounding != 0)
                } else {
                    2 + u64::from(base < r2 + rounding)
                };
                exponent += 1;
                if !(base <= amount && exponent < max_exp) {
                    break;
                }
            }
            if amount < 10 {
                if 0 < rounding {
                    tenths += 1;
                    rounding = 0;
                    if tenths == 10 {
                        amount += 1;
                        tenths = 0;
                    }
                }
                if amount < 10 && (tenths != 0 || opts & HUMAN_SUPPRESS_POINT_ZERO == 0) {
                    fraction = format!(".{tenths}");
                    tenths = 0;
                    rounding = 0;
                }
            }
        }
    }
    if 0 < tenths + rounding {
        amount += 1;
        if autoscale && amount == base && exponent < max_exp {
            exponent += 1;
            if opts & HUMAN_SUPPRESS_POINT_ZERO == 0 {
                fraction = ".0".to_string();
            }
            amount = 1;
        }
    }
    let mut out = amount.to_string();
    out.push_str(&fraction);
    push_human_suffix(&mut out, opts, exponent, base, to);
    out
}

/// The floating-point fallback gnulib uses when the block sizes do not divide.
fn human_readable_float(n: u64, opts: u32, from: u64, to: u64) -> String {
    let base: u64 = if opts & HUMAN_BASE_1024 != 0 {
        1024
    } else {
        1000
    };
    let mut amount = n as f64 * (from as f64 / to as f64);
    let mut exponent = -1;
    let mut out = if opts & HUMAN_AUTOSCALE == 0 {
        format!("{:.0}", amount.ceil())
    } else {
        exponent = 0;
        let mut scale = 1.0;
        loop {
            scale *= base as f64;
            exponent += 1;
            if !(scale * base as f64 <= amount && exponent < POWER_LETTERS.len() as i32 - 1) {
                break;
            }
        }
        amount /= scale;
        let tenths = (amount * 10.0).ceil() / 10.0;
        if tenths < 10.0 {
            format!("{tenths:.1}")
        } else {
            format!("{:.0}", amount.ceil())
        }
    };
    push_human_suffix(&mut out, opts, exponent, base, to);
    out
}

fn push_human_suffix(out: &mut String, opts: u32, mut exponent: i32, base: u64, to: u64) {
    if opts & HUMAN_SI == 0 {
        return;
    }
    if exponent < 0 {
        exponent = 0;
        let mut power: u64 = 1;
        while power < to {
            power = power.saturating_mul(base);
            exponent += 1;
            if exponent == POWER_LETTERS.len() as i32 - 1 {
                break;
            }
        }
    }
    if exponent > 0 {
        out.push(if base == 1000 && exponent == 1 {
            'k'
        } else {
            POWER_LETTERS[exponent as usize]
        });
    }
    if opts & HUMAN_B != 0 {
        if base == 1024 && exponent > 0 {
            out.push('i');
        }
        out.push('B');
    }
}

/// Why a `--block-size`-style argument was rejected (gnulib `strtol_error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SizeError {
    Invalid,
    InvalidSuffix,
    TooLarge,
}

impl SizeError {
    /// GNU's `xstrtol_fatal` wording, e.g. `invalid suffix in -B argument '1x'`.
    pub(crate) fn message(self, option: &str, value: &str) -> String {
        let value = quote_bytes(value.as_bytes(), Quoting::Locale);
        match self {
            Self::Invalid => format!("invalid {option} argument {value}"),
            Self::InvalidSuffix => format!("invalid suffix in {option} argument {value}"),
            Self::TooLarge => format!("{option} argument {value} too large"),
        }
    }
}

/// gnulib `xstrtoumax` with the `"eEgGkKmMpPtTyYzZ0"` suffixes used for sizes.
pub(crate) fn parse_size_suffixed(s: &str) -> Result<u64, SizeError> {
    let bytes = s.as_bytes();
    if s.trim_start().starts_with('-') {
        return Err(SizeError::Invalid);
    }
    let (radix, digits_start) = if bytes.len() > 2
        && bytes[0] == b'0'
        && (bytes[1] | 0x20) == b'x'
        && bytes[2].is_ascii_hexdigit()
    {
        (16, 2)
    } else if bytes.first() == Some(&b'0') {
        (8, 0)
    } else {
        (10, 0)
    };
    let digits_len = bytes[digits_start..]
        .iter()
        .take_while(|b| (**b as char).is_digit(radix))
        .count();
    let mut overflow = false;
    let mut value: u64 = 1;
    let mut pos = digits_start + digits_len;
    if digits_len == 0 {
        if bytes.is_empty() || !b"eEgGkKmMpPtTyYzZ".contains(&bytes[0]) {
            return Err(SizeError::Invalid);
        }
        pos = 0;
    } else {
        match u64::from_str_radix(&s[digits_start..pos], radix) {
            Ok(v) => value = v,
            Err(_) => {
                overflow = true;
                value = u64::MAX;
            }
        }
    }
    if pos == bytes.len() {
        return if overflow {
            Err(SizeError::TooLarge)
        } else {
            Ok(value)
        };
    }
    let suffix = bytes[pos];
    let mut base: u64 = 1024;
    let mut consumed = 1;
    match bytes.get(pos + 1) {
        Some(b'i') if bytes.get(pos + 2) == Some(&b'B') => consumed = 3,
        Some(b'B') | Some(b'D') => {
            base = 1000;
            consumed = 2;
        }
        _ => {}
    }
    let power = match suffix {
        b'k' | b'K' => 1,
        b'M' | b'm' => 2,
        b'G' | b'g' => 3,
        b'T' | b't' => 4,
        b'P' => 5,
        b'E' => 6,
        b'Z' => 7,
        b'Y' => 8,
        _ => return Err(SizeError::InvalidSuffix),
    };
    for _ in 0..power {
        match value.checked_mul(base) {
            Some(v) => value = v,
            None => overflow = true,
        }
    }
    if pos + consumed != bytes.len() {
        return Err(SizeError::InvalidSuffix);
    }
    if overflow {
        Err(SizeError::TooLarge)
    } else {
        Ok(value)
    }
}

/// gnulib `human_options`: parses a block size such as `1K`, `M`, `KB`, `si` or
/// `human-readable` into `(human_readable opts, output block size)`.
pub(crate) fn parse_block_size(spec: &str) -> Result<(u32, u64), SizeError> {
    let spec = spec.strip_prefix('\'').unwrap_or(spec);
    for (word, opts) in [
        (
            "human-readable",
            HUMAN_AUTOSCALE | HUMAN_SI | HUMAN_BASE_1024,
        ),
        ("si", HUMAN_AUTOSCALE | HUMAN_SI),
    ] {
        if !spec.is_empty() && word.starts_with(spec) {
            return Ok((opts, 1));
        }
    }
    let size = parse_size_suffixed(spec)?;
    if size == 0 {
        return Err(SizeError::Invalid);
    }
    let mut opts = 0;
    // A spec without leading digits ("K", "MiB", "kB") also prints the unit.
    if !spec.starts_with(|c: char| c.is_ascii_digit()) {
        opts |= HUMAN_SI;
        if spec.ends_with('B') {
            opts |= HUMAN_B;
        }
        if !spec.ends_with('B') || spec.ends_with("iB") {
            opts |= HUMAN_BASE_1024;
        }
    }
    Ok((opts, size))
}

/// The default output block size for a tool honoring `tool_var` (e.g.
/// `DU_BLOCK_SIZE`), then `BLOCK_SIZE`, `BLOCKSIZE` and `POSIXLY_CORRECT`.
pub(crate) fn env_block_size(tool_var: &str) -> (u32, u64) {
    let spec = std::env::var(tool_var)
        .or_else(|_| std::env::var("BLOCK_SIZE"))
        .or_else(|_| std::env::var("BLOCKSIZE"));
    if let Ok(spec) = spec
        && let Ok(parsed) = parse_block_size(&spec)
    {
        return parsed;
    }
    let default = if std::env::var_os("POSIXLY_CORRECT").is_some() {
        512
    } else {
        1024
    };
    (0, default)
}
