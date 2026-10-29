//! POSIX `fnmatch` patterns (du, ls).

/// POSIX `fnmatch` (no flags beyond `period`: a leading `.` must match literally).
pub(crate) fn fnmatch(pattern: &[u8], name: &[u8], period: bool) -> bool {
    if period && name.first() == Some(&b'.') && pattern.first() != Some(&b'.') {
        return false;
    }
    fnmatch_inner(pattern, name)
}

fn fnmatch_inner(p: &[u8], n: &[u8]) -> bool {
    let (mut pi, mut ni) = (0, 0);
    let mut backtrack: Option<(usize, usize)> = None;
    while ni < n.len() || pi < p.len() {
        if pi < p.len() {
            match p[pi] {
                b'*' => {
                    backtrack = Some((pi, ni + 1));
                    pi += 1;
                    continue;
                }
                b'?' if ni < n.len() => {
                    pi += 1;
                    ni += 1;
                    continue;
                }
                b'[' if ni < n.len() => {
                    if let Some((matched, next)) = match_bracket(&p[pi..], n[ni]) {
                        if matched {
                            pi += next;
                            ni += 1;
                            continue;
                        }
                    } else if n[ni] == b'[' {
                        pi += 1;
                        ni += 1;
                        continue;
                    }
                }
                b'\\' if pi + 1 < p.len() && ni < n.len() && p[pi + 1] == n[ni] => {
                    pi += 2;
                    ni += 1;
                    continue;
                }
                c if c != b'\\' && ni < n.len() && c == n[ni] => {
                    pi += 1;
                    ni += 1;
                    continue;
                }
                _ => {}
            }
        }
        match backtrack {
            Some((bp, bn)) if bn <= n.len() => {
                pi = bp + 1;
                ni = bn;
                backtrack = Some((bp, bn + 1));
            }
            _ => return false,
        }
    }
    true
}

/// Matches `c` against the bracket expression at the start of `p`; returns
/// `(matched, length of the expression)` or `None` if it is unterminated.
fn match_bracket(p: &[u8], c: u8) -> Option<(bool, usize)> {
    let mut i = 1;
    let negate = matches!(p.get(i), Some(b'!') | Some(b'^'));
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < p.len() {
        if p[i] == b']' && !first {
            return Some((matched != negate, i + 1));
        }
        first = false;
        let mut lo = p[i];
        if lo == b'\\' && i + 1 < p.len() {
            i += 1;
            lo = p[i];
        }
        if p.get(i + 1) == Some(&b'-') && p.get(i + 2).is_some_and(|h| *h != b']') {
            let hi = p[i + 2];
            matched |= (lo..=hi).contains(&c);
            i += 3;
        } else {
            matched |= lo == c;
            i += 1;
        }
    }
    None
}
