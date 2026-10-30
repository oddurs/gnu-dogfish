//! Number formatting shared by printf and seq.

#[derive(Default, Clone, Copy, Debug)]
pub(crate) struct PrintfFlags {
    pub(crate) left: bool,
    pub(crate) zero: bool,
    pub(crate) plus: bool,
    pub(crate) space: bool,
    pub(crate) alt: bool,
}

/// Appends `body` to `out`, padded to `width` as C's printf pads a
/// conversion: with spaces, or with zeros after any sign and `0x` when
/// `flags.zero` and `zero_ok` (numbers, not strings or inf/nan).
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
