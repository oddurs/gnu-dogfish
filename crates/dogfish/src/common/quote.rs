//! Quoting of file names in messages and listings (gnulib `quotearg`).

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

/// `quoteaf`: how GNU names files in most messages (`'a b'`, `"it's"`, `'nl'$'\n''x'`).
pub(crate) fn qa(s: impl AsRef<OsStr>) -> String {
    quote_bytes(s.as_ref().as_bytes(), Quoting::ShellEscapeAlways)
}

/// `quotef`: quotes only names that need it, and `:` (used before `: error` text).
pub(crate) fn qf(s: impl AsRef<OsStr>) -> String {
    String::from_utf8_lossy(&quote_bytes_raw(
        s.as_ref().as_bytes(),
        Quoting::ShellEscape,
        b":",
    ))
    .into_owned()
}

/// `quote`: GNU's locale quoting, `'tab\tname'` (curly quotes in UTF-8 locales).
pub(crate) fn quote_locale(s: impl AsRef<OsStr>) -> String {
    quote_bytes(s.as_ref().as_bytes(), Quoting::Locale)
}

// ----- file-name quoting (gnulib quotearg) ---------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Quoting {
    Literal,
    Shell,
    ShellAlways,
    ShellEscape,
    ShellEscapeAlways,
    C,
    CMaybe,
    Escape,
    Locale,
    CLocale,
}

impl Quoting {
    pub(crate) const NAMES: [&'static str; 10] = [
        "literal",
        "shell",
        "shell-always",
        "shell-escape",
        "shell-escape-always",
        "c",
        "c-maybe",
        "escape",
        "locale",
        "clocale",
    ];

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "literal" => Self::Literal,
            "shell" => Self::Shell,
            "shell-always" => Self::ShellAlways,
            "shell-escape" => Self::ShellEscape,
            "shell-escape-always" => Self::ShellEscapeAlways,
            "c" => Self::C,
            "c-maybe" => Self::CMaybe,
            "escape" => Self::Escape,
            "locale" => Self::Locale,
            "clocale" => Self::CLocale,
            _ => return None,
        })
    }
}

/// True when the character-type locale is UTF-8, so multibyte characters
/// count as printable (otherwise every non-ASCII byte is, as in the C locale).
fn locale_is_utf8() -> bool {
    static UTF8: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *UTF8.get_or_init(|| {
        let value = ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .filter_map(|v| std::env::var(v).ok())
            .find(|v| !v.is_empty())
            .unwrap_or_default()
            .to_ascii_lowercase();
        value.contains("utf-8") || value.contains("utf8")
    })
}

/// Length and printability of the character starting `bytes`, per the locale.
pub(crate) fn char_at(bytes: &[u8]) -> (usize, bool) {
    let c = bytes[0];
    if c < 0x80 || !locale_is_utf8() {
        return (1, (0x20..0x7f).contains(&c));
    }
    let len = match c {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return (1, false),
    };
    match bytes.get(..len).map(std::str::from_utf8) {
        Some(Ok(s)) => (len, s.chars().all(|ch| !ch.is_control())),
        _ => (1, false),
    }
}

/// Display width of an already-quoted name (`mbsnwidth`): control characters
/// take no columns.
pub(crate) fn display_width(bytes: &[u8]) -> usize {
    // In single-byte locales GNU ls counts only printable bytes.
    if !locale_is_utf8() {
        return bytes.iter().filter(|b| (0x20..0x7f).contains(*b)).count();
    }
    String::from_utf8_lossy(bytes)
        .chars()
        .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

/// Quotes `arg` like gnulib `quotearg_buffer` in `style`.
pub(crate) fn quote_bytes(arg: &[u8], style: Quoting) -> String {
    String::from_utf8_lossy(&quote_bytes_raw(arg, style, b"")).into_owned()
}

/// `quotearg_buffer_restyled`; `extra` are additional bytes to backslash-escape.
pub(crate) fn quote_bytes_raw(arg: &[u8], style: Quoting, extra: &[u8]) -> Vec<u8> {
    let utf8 = locale_is_utf8();
    let (open, close): (&[u8], &[u8]) = match style {
        Quoting::C | Quoting::CMaybe => (b"\"", b"\""),
        Quoting::Locale if utf8 => ("\u{2018}".as_bytes(), "\u{2019}".as_bytes()),
        Quoting::CLocale if utf8 => ("\u{2018}".as_bytes(), "\u{2019}".as_bytes()),
        Quoting::Locale => (b"'", b"'"),
        Quoting::CLocale => (b"\"", b"\""),
        Quoting::Shell
        | Quoting::ShellAlways
        | Quoting::ShellEscape
        | Quoting::ShellEscapeAlways => (b"'", b"'"),
        Quoting::Literal | Quoting::Escape => (b"", b""),
    };
    if style == Quoting::Literal {
        return arg.to_vec();
    }
    let shell = matches!(
        style,
        Quoting::Shell | Quoting::ShellAlways | Quoting::ShellEscape | Quoting::ShellEscapeAlways
    );
    // "Elided" styles add the outer quotes only when something needs them.
    let elide = matches!(
        style,
        Quoting::Shell | Quoting::ShellEscape | Quoting::CMaybe
    );
    let backslash = !matches!(style, Quoting::Shell | Quoting::ShellAlways);
    let force = || match style {
        Quoting::Shell => quote_bytes_raw(arg, Quoting::ShellAlways, extra),
        Quoting::CMaybe => quote_bytes_raw(arg, Quoting::C, extra),
        _ => quote_bytes_raw(arg, Quoting::ShellEscapeAlways, extra),
    };

    let mut out: Vec<u8> = Vec::with_capacity(arg.len() + 2);
    if !elide {
        out.extend_from_slice(open);
    }
    let mut pending_shell_escape_end = false;
    let mut encountered_single_quote = false;
    let mut all_compat = true;
    let mut i = 0;
    while i < arg.len() {
        let mut c = arg[i];
        let mut escaping = false;
        let mut compat = false;
        let mut is_right_quote = false;
        if backslash && !shell && !close.is_empty() && arg[i..].starts_with(close) {
            if elide {
                return force();
            }
            is_right_quote = true;
        }
        // `None` means "store as is" (after the quote_these_too check),
        // `Some(e)` means "store backslash + e".
        let mut escape: Option<u8> = None;
        let mut direct = false;
        match c {
            0x07 | 0x08 | 0x0c | 0x0b | b'\n' | b'\r' | b'\t' => {
                let e = match c {
                    0x07 => b'a',
                    0x08 => b'b',
                    0x0c => b'f',
                    0x0b => b'v',
                    b'\n' => b'n',
                    b'\r' => b'r',
                    _ => b't',
                };
                if shell && elide {
                    return force();
                }
                if backslash {
                    escape = Some(e);
                }
            }
            b'\\' => {
                if shell {
                    if elide {
                        return force();
                    }
                    direct = true;
                } else if backslash && elide {
                    // c-maybe: a lone backslash needs no quoting.
                    direct = true;
                } else if backslash {
                    escape = Some(b'\\');
                }
            }
            b'?' => {
                if shell && elide {
                    return force();
                }
            }
            b'{' | b'}' | b'#' | b'~' | b' ' | b'!' | b'"' | b'$' | b'&' | b'(' | b')' | b'*'
            | b';' | b'<' | b'=' | b'>' | b'[' | b'^' | b'`' | b'|' => {
                let special = match c {
                    b'{' | b'}' => arg.len() == 1,
                    b'#' | b'~' => i == 0,
                    _ => true,
                };
                if special {
                    if matches!(c, b'{' | b'}' | b'#' | b'~' | b' ') {
                        compat = true;
                    }
                    if shell && elide {
                        return force();
                    }
                }
            }
            b'\'' => {
                encountered_single_quote = true;
                compat = true;
                if shell {
                    if elide {
                        return force();
                    }
                    out.extend_from_slice(b"'\\'");
                    pending_shell_escape_end = false;
                }
            }
            b'%'
            | b'+'
            | b','
            | b'-'
            | b'.'
            | b'/'
            | b'0'..=b'9'
            | b':'
            | b'A'..=b'Z'
            | b']'
            | b'_'
            | b'a'..=b'z' => compat = true,
            _ => {
                let (len, printable) = char_at(&arg[i..]);
                compat = printable;
                if len > 1 || (backslash && !printable) {
                    for j in 0..len {
                        c = arg[i + j];
                        if backslash && !printable {
                            if elide {
                                return force();
                            }
                            escaping = true;
                            if shell && !pending_shell_escape_end {
                                out.extend_from_slice(b"'$'");
                                pending_shell_escape_end = true;
                            }
                            out.push(b'\\');
                            out.push(b'0' + (c >> 6));
                            out.push(b'0' + ((c >> 3) & 7));
                            c = b'0' + (c & 7);
                        } else if is_right_quote {
                            out.push(b'\\');
                            is_right_quote = false;
                        }
                        if j + 1 == len {
                            break;
                        }
                        if pending_shell_escape_end && !escaping {
                            out.extend_from_slice(b"''");
                            pending_shell_escape_end = false;
                        }
                        out.push(c);
                    }
                    i += len - 1;
                    direct = true;
                }
            }
        }
        if escape.is_none()
            && !direct
            && !(((backslash && !shell) || elide) && extra.contains(&c))
            && !is_right_quote
        {
            direct = true;
        }
        if !direct {
            // store_escape
            if elide {
                return force();
            }
            escaping = true;
            if shell && !pending_shell_escape_end {
                out.extend_from_slice(b"'$'");
                pending_shell_escape_end = true;
            }
            out.push(b'\\');
            if let Some(e) = escape {
                c = e;
            }
        }
        // store_c
        if pending_shell_escape_end && !escaping {
            out.extend_from_slice(b"''");
            pending_shell_escape_end = false;
        }
        out.push(c);
        all_compat &= compat;
        i += 1;
    }
    if arg.is_empty() && shell && elide {
        return force();
    }
    if shell && !elide && encountered_single_quote && all_compat {
        return quote_bytes_raw(arg, Quoting::C, extra);
    }
    if !elide {
        out.extend_from_slice(close);
    }
    out
}

/// coreutils `quoteaf`: shell-escape-always quoting used in diagnostics.
pub(crate) fn quote_af(name: &str) -> String {
    quote_bytes(name.as_bytes(), Quoting::ShellEscapeAlways)
}

/// coreutils `quotef`: shell-escape quoting, only when needed.
pub(crate) fn quote_f(name: &str) -> String {
    quote_bytes(name.as_bytes(), Quoting::ShellEscape)
}
