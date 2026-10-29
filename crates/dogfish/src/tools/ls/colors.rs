//! GNU `ls` colours (LS_COLORS).

use dogfish_core::Style;

use crate::common::fnmatch::fnmatch;
use crate::common::quote::{Quoting, quote_bytes};

/// Indices into [`LsColors::ind`], in GNU's `enum indicator_no` order.
pub(crate) const C_LEFT: usize = 0;
pub(crate) const C_RIGHT: usize = 1;
pub(crate) const C_END: usize = 2;
pub(crate) const C_RESET: usize = 3;
pub(crate) const C_NORM: usize = 4;
pub(crate) const C_FILE: usize = 5;
pub(crate) const C_DIR: usize = 6;
pub(crate) const C_LINK: usize = 7;
pub(crate) const C_FIFO: usize = 8;
pub(crate) const C_SOCK: usize = 9;
pub(crate) const C_BLK: usize = 10;
pub(crate) const C_CHR: usize = 11;
pub(crate) const C_MISSING: usize = 12;
pub(crate) const C_ORPHAN: usize = 13;
pub(crate) const C_EXEC: usize = 14;
const C_DOOR: usize = 15;
pub(crate) const C_SETUID: usize = 16;
pub(crate) const C_SETGID: usize = 17;
pub(crate) const C_STICKY: usize = 18;
pub(crate) const C_OTHER_WRITABLE: usize = 19;
pub(crate) const C_STICKY_OTHER_WRITABLE: usize = 20;
pub(crate) const C_CAP: usize = 21;
pub(crate) const C_MULTIHARDLINK: usize = 22;
pub(crate) const C_CLR_TO_EOL: usize = 23;

/// The LS_COLORS key of each indicator (GNU `indicator_name`).
const LS_COLOR_KEYS: [&str; 24] = [
    "lc", "rc", "ec", "rs", "no", "fi", "di", "ln", "pi", "so", "bd", "cd", "mi", "or", "ex", "do",
    "su", "sg", "st", "ow", "tw", "ca", "mh", "cl",
];

/// The `TERM` patterns of GNU's built-in dircolors database: without
/// LS_COLORS, `ls` colours only for these terminals (or a non-empty COLORTERM).
const LS_COLOR_TERMS: [&str; 25] = [
    "Eterm",
    "ansi",
    "*color*",
    "con[0-9]*x[0-9]*",
    "cons25",
    "console",
    "cygwin",
    "*direct*",
    "dtterm",
    "gnome",
    "hurd",
    "jfbterm",
    "konsole",
    "kterm",
    "linux",
    "linux-c",
    "mlterm",
    "putty",
    "rxvt*",
    "screen*",
    "st",
    "terminator",
    "tmux*",
    "vt100",
    "xterm*",
];

/// A `*suffix=seq` entry of LS_COLORS (GNU `struct color_ext_type`).
pub(crate) struct ColorExt {
    suffix: Vec<u8>,
    pub(crate) seq: Vec<u8>,
    /// Compare case-sensitively (set when differently-cased suffixes differ).
    exact: bool,
    /// Shadowed by an entry of higher precedence.
    ignored: bool,
}

/// GNU `ls`'s colour table: the indicators and the suffix list.
pub(crate) struct LsColors {
    pub(crate) ind: [Option<Vec<u8>>; 24],
    /// Highest precedence (last defined) first.
    exts: Vec<ColorExt>,
    /// `ln=target`: colour symlinks like the file they point to.
    pub(crate) referent: bool,
}

impl LsColors {
    /// GNU's built-in `color_indicator` table.
    fn defaults() -> Self {
        let mut ind: [Option<Vec<u8>>; 24] = Default::default();
        for (i, seq) in [
            (C_LEFT, &b"\x1b["[..]),
            (C_RIGHT, b"m"),
            (C_RESET, b"0"),
            (C_DIR, b"01;34"),
            (C_LINK, b"01;36"),
            (C_FIFO, b"33"),
            (C_SOCK, b"01;35"),
            (C_BLK, b"01;33"),
            (C_CHR, b"01;33"),
            (C_EXEC, b"01;32"),
            (C_DOOR, b"01;35"),
            (C_SETUID, b"37;41"),
            (C_SETGID, b"30;43"),
            (C_STICKY, b"37;44"),
            (C_OTHER_WRITABLE, b"34;42"),
            (C_STICKY_OTHER_WRITABLE, b"30;42"),
            (C_CLR_TO_EOL, b"\x1b[K"),
        ] {
            ind[i] = Some(seq.to_vec());
        }
        LsColors {
            ind,
            exts: Vec::new(),
            referent: false,
        }
    }

    /// GNU `parse_ls_color`: the table to colour with, or `None` (after a
    /// warning, for an unparsable LS_COLORS) when output stays uncoloured.
    pub(crate) fn from_env() -> Option<Self> {
        use std::os::unix::ffi::OsStrExt;
        let Some(spec) = std::env::var_os("LS_COLORS").filter(|v| !v.is_empty()) else {
            let colorterm = std::env::var_os("COLORTERM").is_some_and(|v| !v.is_empty());
            let known_term = std::env::var_os("TERM").is_some_and(|t| {
                LS_COLOR_TERMS
                    .iter()
                    .any(|p| fnmatch(p.as_bytes(), t.as_bytes(), false))
            });
            return (colorterm || known_term).then(Self::defaults);
        };
        let parsed = Self::parse(spec.as_bytes());
        if parsed.is_none() {
            eprintln!("ls: unparsable value for LS_COLORS environment variable");
        }
        parsed
    }

    fn parse(spec: &[u8]) -> Option<Self> {
        let at = |i: usize| spec.get(i).copied().unwrap_or(0);
        let mut c = Self::defaults();
        let mut exts = Vec::new();
        let mut i = 0;
        loop {
            match at(i) {
                0 => break,
                b':' => i += 1,
                b'*' => {
                    i += 1;
                    let suffix = ls_color_string(spec, &mut i, true)?;
                    if at(i) != b'=' {
                        return None;
                    }
                    i += 1;
                    let seq = ls_color_string(spec, &mut i, false)?;
                    exts.push(ColorExt {
                        suffix,
                        seq,
                        exact: false,
                        ignored: false,
                    });
                }
                first => {
                    let second = at(i + 1);
                    if second == 0 || at(i + 2) != b'=' {
                        return None;
                    }
                    let label = [first, second];
                    i += 3;
                    let slot = LS_COLOR_KEYS.iter().position(|k| k.as_bytes() == label);
                    let value = slot.and_then(|_| ls_color_string(spec, &mut i, false));
                    let (Some(slot), Some(value)) = (slot, value) else {
                        // GNU says this for a malformed value of a known key too.
                        eprintln!(
                            "ls: unrecognized prefix: {}",
                            quote_bytes(&label, Quoting::Locale)
                        );
                        return None;
                    };
                    c.ind[slot] = Some(value);
                }
            }
        }
        // Later definitions take precedence. Like GNU, drop entries that can
        // never match, and compare case-sensitively only where suffixes that
        // differ just in case have different sequences.
        exts.reverse();
        for a in 0..exts.len() {
            let mut case_ignored = false;
            for b in a + 1..exts.len() {
                let (head, tail) = exts.split_at_mut(b);
                let (e1, e2) = (&mut head[a], &mut tail[0]);
                if e1.ignored || e2.ignored || e1.suffix.len() != e2.suffix.len() {
                    continue;
                }
                if e1.suffix == e2.suffix {
                    e2.ignored = true;
                } else if e1.suffix.eq_ignore_ascii_case(&e2.suffix) {
                    if case_ignored {
                        e2.ignored = true;
                    } else if e1.seq == e2.seq {
                        e2.ignored = true;
                        case_ignored = true;
                    } else {
                        e1.exact = true;
                        e2.exact = true;
                    }
                }
            }
        }
        c.exts = exts;
        c.referent = c.ind[C_LINK].as_deref() == Some(b"target");
        Some(c)
    }

    /// GNU `is_colored`: set to something other than empty, `0` or `00`.
    pub(crate) fn is_colored(&self, i: usize) -> bool {
        !matches!(self.ind[i].as_deref(), None | Some(b"" | b"0" | b"00"))
    }

    /// The sequence of indicator `i` (empty when unset).
    pub(crate) fn seq(&self, i: usize) -> &[u8] {
        self.ind[i].as_deref().unwrap_or_default()
    }

    /// The sequence of the first suffix entry matching `name`.
    pub(crate) fn suffix_color(&self, name: &[u8]) -> Option<&[u8]> {
        self.exts
            .iter()
            .filter(|e| !e.ignored && e.suffix.len() <= name.len())
            .find(|e| {
                let tail = &name[name.len() - e.suffix.len()..];
                if e.exact {
                    tail == e.suffix.as_slice()
                } else {
                    tail.eq_ignore_ascii_case(&e.suffix)
                }
            })
            .map(|e| e.seq.as_slice())
    }
}

/// GNU `get_funky_string`: decodes one LS_COLORS value (`\e`, `\033`, `\x1b`,
/// `^[`, ...) from `spec[*i..]`, stopping at `:` or the end (or at `=` when
/// `equals_end`). `None` for a malformed escape.
fn ls_color_string(spec: &[u8], i: &mut usize, equals_end: bool) -> Option<Vec<u8>> {
    enum State {
        Ground,
        Backslash,
        Octal,
        Hex,
        Caret,
    }
    let mut out = Vec::new();
    let mut state = State::Ground;
    let mut num: u8 = 0;
    loop {
        let ch = spec.get(*i).copied().unwrap_or(0);
        match state {
            State::Ground => match ch {
                0 | b':' => return Some(out),
                b'=' if equals_end => return Some(out),
                b'\\' => {
                    state = State::Backslash;
                    *i += 1;
                }
                b'^' => {
                    state = State::Caret;
                    *i += 1;
                }
                _ => {
                    out.push(ch);
                    *i += 1;
                }
            },
            State::Backslash => {
                match ch {
                    0 => return None,
                    b'0'..=b'7' => {
                        state = State::Octal;
                        num = ch - b'0';
                    }
                    b'x' | b'X' => {
                        state = State::Hex;
                        num = 0;
                    }
                    _ => {
                        out.push(match ch {
                            b'a' => 7,
                            b'b' => 8,
                            b'e' => 27,
                            b'f' => 12,
                            b'n' => b'\n',
                            b'r' => b'\r',
                            b't' => b'\t',
                            b'v' => 11,
                            b'?' => 127,
                            b'_' => b' ',
                            other => other,
                        });
                        state = State::Ground;
                    }
                }
                *i += 1;
            }
            State::Octal => {
                if (b'0'..=b'7').contains(&ch) {
                    num = (num << 3).wrapping_add(ch - b'0');
                    *i += 1;
                } else {
                    out.push(num);
                    state = State::Ground;
                }
            }
            State::Hex => match (ch as char).to_digit(16) {
                Some(d) => {
                    num = (num << 4).wrapping_add(d as u8);
                    *i += 1;
                }
                None => {
                    out.push(num);
                    state = State::Ground;
                }
            },
            State::Caret => {
                state = State::Ground;
                match ch {
                    b'@'..=b'~' => {
                        out.push(ch & 0o37);
                        *i += 1;
                    }
                    // GNU does not consume the `?` of `^?`: it is output too.
                    b'?' => out.push(127),
                    _ => return None,
                }
            }
        }
    }
}

pub(crate) fn style_entry_name(
    name: &str,
    kind: &str,
    mode: &str,
    classify: bool,
    st: Style,
) -> String {
    let suffix = if classify {
        match kind {
            "dir" => "/",
            "symlink" => "@",
            "exe" => "*",
            _ => "",
        }
    } else {
        ""
    };
    let label = format!("{}{}", name, suffix);
    match kind {
        "dir" => st.bold_blue(&label),
        "symlink" => st.magenta(&label),
        "exe" => st.bold_green(&label),
        _ if mode.starts_with('p') || mode.starts_with('s') => st.yellow(&label),
        _ => label,
    }
}

pub(crate) fn colorize_mode(mode: &str, st: Style) -> String {
    let mut out = String::new();
    for ch in mode.chars() {
        match ch {
            'd' => out.push_str(&st.bold_blue("d")),
            'l' => out.push_str(&st.magenta("l")),
            'r' => out.push_str(&st.yellow("r")),
            'w' => out.push_str(&st.red("w")),
            'x' => out.push_str(&st.green("x")),
            '-' => out.push_str(&st.dim("-")),
            other => out.push(other),
        }
    }
    out
}

/// Entry points for the fuzz targets in `fuzz/` (only with the `fuzzing` feature).
#[cfg(feature = "fuzzing")]
#[allow(dead_code)]
pub mod fuzz {
    use super::*;

    /// An `LS_COLORS` value.
    pub fn parse(spec: &[u8]) {
        if let Some(c) = LsColors::parse(spec) {
            let _ = c.suffix_color(b"name.txt");
        }
    }
}
