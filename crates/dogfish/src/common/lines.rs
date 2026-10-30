//! Line splitting and NDJSON detection shared by the text tools.
//!
//! Records end in `\n`, or in NUL with `-z/--zero-terminated` (GNU's
//! `line_end` / `eolchar`): every line-oriented tool takes the terminator as
//! a byte from [`line_end`] and uses it for both input and output.

/// The record terminator: NUL under `-z/--zero-terminated`, else newline.
pub(crate) fn line_end(zero_terminated: bool) -> u8 {
    if zero_terminated { 0 } else { b'\n' }
}

/// Splits a buffer into records ending in `eol` (without it); a final
/// unterminated record is yielded as well, but a trailing `eol` does not
/// produce an empty one.
pub(crate) fn split_records(buf: &[u8], eol: u8) -> impl Iterator<Item = &[u8]> {
    let body = buf.strip_suffix(&[eol]).unwrap_or(buf);
    let empty = buf.is_empty();
    body.split(move |&b| b == eol).filter(move |_| !empty)
}

pub(crate) fn is_blank(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

/// Returns true if the (trimmed) input looks like the start of an NDJSON / JSON stream.
pub(crate) fn looks_like_json(bytes: &[u8]) -> bool {
    matches!(bytes.trim_ascii_start().first(), Some(b'{') | Some(b'['))
}
