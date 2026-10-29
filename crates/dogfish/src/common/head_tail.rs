//! Helpers shared by head and tail: `==> NAME <==` headers, GNU counts and
//! the legacy `-N` syntax.

use dogfish_core::InputSource;
use memchr::memrchr_iter;

/// Name GNU uses for an input in `==> NAME <==` headers.
pub(crate) fn header_name(src: &InputSource) -> &str {
    match src {
        InputSource::Stdin => "standard input",
        InputSource::File(p) => p.as_str(),
    }
}

/// Parses a GNU count with optional multiplier suffix (`b`, `K`, `kB`, `MiB`, ...).
pub(crate) fn parse_gnu_count(s: &str) -> Option<u64> {
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, suffix) = s.split_at(split);
    if num.is_empty() {
        return None;
    }
    let n: u64 = num.parse().unwrap_or(u64::MAX);
    let pow = |base: u64, exp: u32| base.checked_pow(exp).unwrap_or(u64::MAX);
    let mult = match suffix {
        "" => 1,
        "b" => 512,
        "kB" | "KB" => 1000,
        "k" | "K" | "KiB" | "kiB" => 1024,
        _ => {
            let letters = "KMGTPEZYRQ";
            let mut chars = suffix.chars();
            let first = chars.next()?.to_ascii_uppercase();
            let exp = letters.find(first)? as u32 + 1;
            match chars.as_str() {
                "" | "iB" => pow(1024, exp),
                "B" => pow(1000, exp),
                _ => return None,
            }
        }
    };
    Some(n.saturating_mul(mult))
}

/// Converts the obsolete `-5` shorthand (first argument only, as in GNU) to `-n 5`.
pub(crate) fn normalize_legacy_num_arg(mut args: Vec<String>) -> Vec<String> {
    if let Some(a) = args.get(1)
        && a.len() > 1
        && a.starts_with('-')
        && a[1..].chars().all(|c| c.is_ascii_digit())
    {
        let n = a[1..].to_string();
        args.splice(1..2, ["-n".to_string(), n]);
    }
    args
}

/// Offset where the last `n` lines of `bytes` begin (an unterminated final line counts).
pub(crate) fn last_lines_start(bytes: &[u8], n: u64) -> usize {
    if n == 0 || bytes.is_empty() {
        return bytes.len();
    }
    let search = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let nth = usize::try_from(n - 1).unwrap_or(usize::MAX);
    memrchr_iter(b'\n', search)
        .nth(nth)
        .map(|pos| pos + 1)
        .unwrap_or(0)
}

pub(crate) fn to_usize(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}
