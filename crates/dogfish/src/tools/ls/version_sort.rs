//! Version sort (gnulib `filevercmp`), used by `ls -v` / `--sort=version`.

/// gnulib `filevercmp`, used by `ls -v` / `--sort=version`.
pub(crate) fn filevercmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }
    // "." sorts first, then "..", then other dot files, then everything else.
    match (a[0] == b'.', b[0] == b'.') {
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        (true, true) => {
            for special in [&b"."[..], &b".."[..]] {
                match (a == special, b == special) {
                    (true, true) => return Ordering::Equal,
                    (true, false) => return Ordering::Less,
                    (false, true) => return Ordering::Greater,
                    _ => {}
                }
            }
        }
        _ => {}
    }
    let (ap, bp) = (version_prefix_len(a), version_prefix_len(b));
    let result = version_revcmp(&a[..ap], &b[..bp]);
    if result != Ordering::Equal || (ap == a.len() && bp == b.len()) {
        result
    } else {
        version_revcmp(a, b)
    }
}

/// Length of `s` without its file suffix, `(\.[A-Za-z~][A-Za-z0-9~]*)*$`.
fn version_prefix_len(s: &[u8]) -> usize {
    let n = s.len();
    let mut prefix = 0;
    let mut i = 0;
    while i < n {
        i += 1;
        prefix = i;
        while i + 1 < n && s[i] == b'.' && (s[i + 1].is_ascii_alphabetic() || s[i + 1] == b'~') {
            i += 2;
            while i < n && (s[i].is_ascii_alphanumeric() || s[i] == b'~') {
                i += 1;
            }
        }
    }
    prefix
}

fn version_order(s: &[u8], pos: usize) -> i32 {
    match s.get(pos) {
        None => -1,
        Some(c) if c.is_ascii_digit() => 0,
        Some(c) if c.is_ascii_alphabetic() => i32::from(*c),
        Some(b'~') => -2,
        Some(c) => i32::from(*c) + 256,
    }
}

fn version_revcmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        while (i < a.len() && !a[i].is_ascii_digit()) || (j < b.len() && !b[j].is_ascii_digit()) {
            let (x, y) = (version_order(a, i), version_order(b, j));
            if x != y {
                return x.cmp(&y);
            }
            i += 1;
            j += 1;
        }
        while i < a.len() && a[i] == b'0' {
            i += 1;
        }
        while j < b.len() && b[j] == b'0' {
            j += 1;
        }
        let mut first_diff = std::cmp::Ordering::Equal;
        while i < a.len() && j < b.len() && a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            if first_diff == std::cmp::Ordering::Equal {
                first_diff = a[i].cmp(&b[j]);
            }
            i += 1;
            j += 1;
        }
        if i < a.len() && a[i].is_ascii_digit() {
            return std::cmp::Ordering::Greater;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            return std::cmp::Ordering::Less;
        }
        if first_diff != std::cmp::Ordering::Equal {
            return first_diff;
        }
    }
    std::cmp::Ordering::Equal
}
