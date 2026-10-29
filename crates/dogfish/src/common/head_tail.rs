//! Helpers shared by head and tail: `==> NAME <==` headers, GNU counts and
//! the last-N-lines scan.

use dogfish_core::InputSource;
use memchr::memrchr_iter;

use crate::common::xstrtol::xdectoumax;

/// Name GNU uses for an input in `==> NAME <==` headers and messages.
pub(crate) fn header_name(src: &InputSource) -> &str {
    match src {
        InputSource::Stdin => "standard input",
        InputSource::File(p) => p.as_str(),
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

/// The first digit getopt would return as an option (`-3` after the
/// obsolete first argument, or `-2cX`): head and tail list `0`-`9` among
/// their options only to reject them with their own message. `with_arg` and
/// `long_with_arg` are the options that take a separate argument. Scans like
/// a permuting getopt, up to `--`.
pub(crate) fn digit_option(
    args: &[String],
    with_arg: &[u8],
    long_with_arg: &[&str],
) -> Option<char> {
    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            if !long.contains('=') && long_with_arg.iter().any(|l| l.starts_with(long)) {
                iter.next();
            }
            continue;
        }
        let bytes = arg.as_bytes();
        if bytes.len() < 2 || bytes[0] != b'-' {
            continue;
        }
        for (i, &c) in bytes.iter().enumerate().skip(1) {
            if c.is_ascii_digit() {
                return Some(c as char);
            }
            if with_arg.contains(&c) {
                if i + 1 == bytes.len() {
                    iter.next();
                }
                break;
            }
        }
    }
    None
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
