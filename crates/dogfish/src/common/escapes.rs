//! Backslash escapes shared by echo and `printf %b`.

/// Expands echo-style escapes (also used by `printf %b`). Returns the bytes and
/// whether `\c` requested that all further output stop.
///
/// `\0NNN` is octal; with `bare_octal` (printf `%b`), `\NNN` is octal too.
pub fn unescape_echo_bytes(s: &[u8], bare_octal: bool) -> (Vec<u8>, bool) {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let ch = s[i];
        i += 1;
        if ch != b'\\' || i >= s.len() {
            out.push(ch);
            continue;
        }
        let e = s[i];
        i += 1;
        match e {
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'c' => return (out, true),
            b'e' | b'E' => out.push(0x1b),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0b),
            b'\\' => out.push(b'\\'),
            b'x' if i < s.len() && s[i].is_ascii_hexdigit() => {
                let mut v = 0u32;
                let mut n = 0;
                while n < 2 && i < s.len() && s[i].is_ascii_hexdigit() {
                    v = v * 16 + (s[i] as char).to_digit(16).unwrap();
                    i += 1;
                    n += 1;
                }
                out.push(v as u8);
            }
            b'0'..=b'7' if e == b'0' || bare_octal => {
                let mut v = if e == b'0' { 0u32 } else { (e - b'0') as u32 };
                let max = if e == b'0' { 3 } else { 2 };
                let mut n = 0;
                while n < max && i < s.len() && (b'0'..=b'7').contains(&s[i]) {
                    v = v * 8 + (s[i] - b'0') as u32;
                    i += 1;
                    n += 1;
                }
                out.push(v as u8);
            }
            other => {
                out.push(b'\\');
                out.push(other);
            }
        }
    }
    (out, false)
}
