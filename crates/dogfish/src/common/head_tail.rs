//! Helpers shared by head and tail: `==> NAME <==` headers, GNU counts and
//! the last-N-lines scan.

use std::ffi::OsStr;

use dogfish_core::InputSource;
use memchr::memrchr_iter;

use crate::common::xstrtol::xdectoumax;

/// Name GNU uses for an input in `==> NAME <==` headers and messages.
pub(crate) fn header_name(src: &InputSource) -> &OsStr {
    match src {
        InputSource::Stdin => OsStr::new("standard input"),
        InputSource::File(p) => p,
    }
}

/// The suffixes head and tail accept on counts (`2b`, `1k`, `1MiB`, `1kB`).
pub(crate) const COUNT_SUFFIXES: &str = "bkKmMGTPEZYRQ0";

/// Parses the count of `-n`/`-c` (sign already removed) like GNU's
/// `xdectoumax`, or returns the `invalid number of lines: 'X'` message.
pub(crate) fn parse_count(s: &str, count_lines: bool) -> Result<u64, String> {
    let what = if count_lines {
        "invalid number of lines"
    } else {
        "invalid number of bytes"
    };
    xdectoumax(s, 0, u64::MAX, COUNT_SUFFIXES, what)
}

/// Offset where the last `n` records of `bytes` begin (an unterminated final
/// record counts), for records ending in `eol`.
pub(crate) fn last_lines_start(bytes: &[u8], n: u64, eol: u8) -> usize {
    if n == 0 || bytes.is_empty() {
        return bytes.len();
    }
    let search = bytes.strip_suffix(&[eol]).unwrap_or(bytes);
    let nth = usize::try_from(n - 1).unwrap_or(usize::MAX);
    memrchr_iter(eol, search)
        .nth(nth)
        .map(|pos| pos + 1)
        .unwrap_or(0)
}

pub(crate) fn to_usize(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}
