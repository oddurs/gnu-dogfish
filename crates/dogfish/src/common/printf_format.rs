//! Number formatting shared by printf and seq.

#[derive(Default, Clone, Copy)]
pub(crate) struct PrintfFlags {
    pub(crate) left: bool,
    pub(crate) zero: bool,
    pub(crate) plus: bool,
    pub(crate) space: bool,
    pub(crate) alt: bool,
}

pub(crate) fn pad_field(body: Vec<u8>, width: usize, flags: PrintfFlags, zero_ok: bool) -> Vec<u8> {
    if body.len() >= width {
        return body;
    }
    let mut v = Vec::with_capacity(width);
    pad_field_into(&mut v, &body, width, flags, zero_ok);
    v
}

/// Appends `body` to `out`, padded as [`pad_field`] pads it.
pub(crate) fn pad_field_into(
    out: &mut Vec<u8>,
    body: &[u8],
    width: usize,
    flags: PrintfFlags,
    zero_ok: bool,
) {
    let pad = width.saturating_sub(body.len());
    if pad == 0 || flags.left {
        out.extend_from_slice(body);
        out.resize(out.len() + pad, b' ');
    } else if flags.zero && zero_ok {
        // Zeros go after any sign / 0x prefix.
        let sign_len = match body {
            [b'-' | b'+' | b' ', b'0', b'x' | b'X', ..] => 3,
            [b'0', b'x' | b'X', ..] => 2,
            [b'-' | b'+' | b' ', ..] => 1,
            _ => 0,
        };
        out.extend_from_slice(&body[..sign_len]);
        out.resize(out.len() + pad, b'0');
        out.extend_from_slice(&body[sign_len..]);
    } else {
        out.resize(out.len() + pad, b' ');
        out.extend_from_slice(body);
    }
}

/// C-style `%e` rendering of a non-negative finite value.
fn c_exp(v: f64, prec: usize, upper: bool) -> String {
    let s = format!("{:.prec$e}", v);
    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let e = if upper { 'E' } else { 'e' };
    format!(
        "{}{}{}{:02}",
        mant,
        e,
        if exp < 0 { '-' } else { '+' },
        exp.abs()
    )
}

fn strip_frac_zeros(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

pub(crate) fn format_float(
    v: f64,
    conv: u8,
    prec: Option<usize>,
    flags: PrintfFlags,
) -> (String, bool) {
    let upper = conv.is_ascii_uppercase();
    let sign = if v.is_sign_negative() && !v.is_nan() {
        "-"
    } else if flags.plus {
        "+"
    } else if flags.space {
        " "
    } else {
        ""
    };
    let a = v.abs();
    if !a.is_finite() {
        let s = if a.is_nan() { "nan" } else { "inf" };
        let s = if upper {
            s.to_uppercase()
        } else {
            s.to_string()
        };
        return (format!("{}{}", sign, s), false);
    }
    let prec = prec.unwrap_or(6);
    let body = match conv.to_ascii_lowercase() {
        b'f' => {
            let mut s = format!("{:.prec$}", a);
            if flags.alt && prec == 0 {
                s.push('.');
            }
            s
        }
        b'e' => {
            let mut s = c_exp(a, prec, upper);
            if flags.alt && prec == 0 {
                let idx = s.find(['e', 'E']).unwrap_or(s.len());
                s.insert(idx, '.');
            }
            s
        }
        _ => {
            // %g
            let p = if prec == 0 { 1 } else { prec };
            let x: i32 = if a == 0.0 {
                0
            } else {
                let e = c_exp(a, p - 1, false);
                e[e.find('e').unwrap() + 1..].parse().unwrap_or(0)
            };
            if (x as i64) < p as i64 && x >= -4 {
                let s = format!("{:.*}", (p as i64 - 1 - x as i64).max(0) as usize, a);
                if flags.alt { s } else { strip_frac_zeros(&s) }
            } else {
                let s = c_exp(a, p - 1, upper);
                if flags.alt {
                    s
                } else {
                    let idx = s.find(['e', 'E']).unwrap();
                    format!("{}{}", strip_frac_zeros(&s[..idx]), &s[idx..])
                }
            }
        }
    };
    (format!("{}{}", sign, body), true)
}
