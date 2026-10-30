//! GNU sort's keys and comparisons: `begfield`/`limfield`, `keycompare`,
//! `compare`, and the orderings behind `-n`, `-g`, `-h`, `-M`, `-R`, `-V`.

use std::borrow::Cow;
use std::cmp::Ordering;

use digest::Digest;
use md5::Md5;

use super::ldbl;

/// GNU's `blanks` table, `field_sep`: the blanks that separate fields,
/// which include newline (part of a record only with `-z`).
pub(super) fn blank(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n')
}

/// The characters `-d` or `-i` ignore (GNU's `ignore` table).
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Ignore {
    #[default]
    None,
    /// `-d`: all but blanks and alphanumerics.
    Nondictionary,
    /// `-i`: the unprintable characters.
    Nonprinting,
}

impl Ignore {
    fn ignores(self, c: u8) -> bool {
        match self {
            Ignore::None => false,
            Ignore::Nondictionary => !c.is_ascii_alphanumeric() && !blank(c),
            Ignore::Nonprinting => !(0x20..0x7f).contains(&c),
        }
    }
}

/// A key's ordering options (GNU `struct keyfield` minus the positions).
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct SortOpts {
    pub(super) skipsblanks: bool,
    pub(super) skipeblanks: bool,
    pub(super) ignore: Ignore,
    /// `-f`: fold lower case to upper case.
    pub(super) translate: bool,
    pub(super) general: bool,
    pub(super) human: bool,
    pub(super) month: bool,
    pub(super) numeric: bool,
    pub(super) random: bool,
    pub(super) reverse: bool,
    pub(super) version: bool,
}

/// Which blanks a key's `b` skips (GNU `enum blanktype`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Blanks {
    Start,
    End,
    Both,
}

impl SortOpts {
    /// GNU `set_ordering`: applies the option letters at the start of `s`;
    /// returns how many it consumed.
    pub(super) fn set_ordering(&mut self, s: &[u8], blanks: Blanks) -> usize {
        for (n, &c) in s.iter().enumerate() {
            match c {
                b'b' => {
                    if blanks != Blanks::End {
                        self.skipsblanks = true;
                    }
                    if blanks != Blanks::Start {
                        self.skipeblanks = true;
                    }
                }
                b'd' => self.ignore = Ignore::Nondictionary,
                b'f' => self.translate = true,
                b'g' => self.general = true,
                b'h' => self.human = true,
                // -d implies -i, so -i does not override it.
                b'i' if self.ignore == Ignore::None => self.ignore = Ignore::Nonprinting,
                b'i' => {}
                b'M' => self.month = true,
                b'n' => self.numeric = true,
                b'R' => self.random = true,
                b'r' => self.reverse = true,
                b'V' => self.version = true,
                _ => return n,
            }
        }
        s.len()
    }

    /// GNU `key_numeric`.
    pub(super) fn key_numeric(&self) -> bool {
        self.numeric || self.general || self.human
    }

    /// GNU `default_key_compare`: no ordering option at all (`-r` aside).
    pub(super) fn is_default(&self) -> bool {
        !(self.ignore != Ignore::None
            || self.translate
            || self.skipsblanks
            || self.skipeblanks
            || self.key_numeric()
            || self.month
            || self.version
            || self.random)
    }

    /// True when `-d`, `-i` or `-f` rewrite the key before it is compared.
    pub(super) fn filters(&self) -> bool {
        self.ignore != Ignore::None || self.translate
    }

    /// GNU `key_to_opts`: the key's option letters, in GNU's order.
    pub(super) fn letters(&self) -> String {
        [
            (self.skipsblanks || self.skipeblanks, 'b'),
            (self.ignore == Ignore::Nondictionary, 'd'),
            (self.translate, 'f'),
            (self.general, 'g'),
            (self.human, 'h'),
            (self.ignore == Ignore::Nonprinting, 'i'),
            (self.month, 'M'),
            (self.numeric, 'n'),
            (self.random, 'R'),
            (self.reverse, 'r'),
            (self.version, 'V'),
        ]
        .iter()
        .filter_map(|&(on, c)| on.then_some(c))
        .collect()
    }

    /// GNU `check_ordering_compatibility`: at most one of `-n`, `-g`, `-h`,
    /// `-M` and the string orderings (`-V`, `-R`, `-d`, `-i`).
    pub(super) fn is_compatible(&self) -> bool {
        let string_like = self.version || self.random || self.ignore != Ignore::None;
        [
            self.numeric,
            self.general,
            self.human,
            self.month,
            string_like,
        ]
        .iter()
        .filter(|&&on| on)
        .count()
            <= 1
    }
}

/// "No field": a key that starts at the start of the line, or ends at its
/// end (GNU's `SIZE_MAX`).
pub(super) const NONE: usize = usize::MAX;

/// A `-k` key (GNU `struct keyfield`).
#[derive(Clone, Copy)]
pub(super) struct SortKey {
    /// Zero-based start field, or [`NONE`] for the start of the line.
    pub(super) sword: usize,
    /// Characters to skip in the start field.
    pub(super) schar: usize,
    /// Zero-based end field, or [`NONE`] for the end of the line.
    pub(super) eword: usize,
    /// Characters of the end field, 0 for all of it.
    pub(super) echar: usize,
    pub(super) opts: SortOpts,
    /// Given in the obsolete `+POS1 -POS2` form.
    pub(super) traditional: bool,
}

impl SortKey {
    pub(super) fn whole_line(opts: SortOpts) -> Self {
        SortKey {
            sword: NONE,
            schar: 0,
            eword: NONE,
            echar: 0,
            opts,
            traditional: false,
        }
    }
}

pub(super) struct Sorter {
    pub(super) keys: Vec<SortKey>,
    pub(super) tab: Option<u8>,
    /// `-s` or `-u`: no last-resort whole-line comparison.
    pub(super) no_last_resort: bool,
    /// The global `-r`, for the last-resort comparison.
    pub(super) reverse: bool,
    /// MD5 state primed with the `-R` seed; each key is hashed with a clone.
    pub(super) random_seed: Md5,
}

impl Sorter {
    /// GNU `begfield`: where `key` starts in `line` (without its newline).
    pub(super) fn begfield(&self, line: &[u8], key: &SortKey) -> usize {
        let lim = line.len();
        let mut p = 0;
        let mut sword = key.sword;
        if let Some(t) = self.tab {
            while p < lim && sword > 0 {
                sword -= 1;
                while p < lim && line[p] != t {
                    p += 1;
                }
                if p < lim {
                    p += 1;
                }
            }
        } else {
            while p < lim && sword > 0 {
                sword -= 1;
                while p < lim && blank(line[p]) {
                    p += 1;
                }
                while p < lim && !blank(line[p]) {
                    p += 1;
                }
            }
        }
        if key.opts.skipsblanks {
            while p < lim && blank(line[p]) {
                p += 1;
            }
        }
        p.saturating_add(key.schar).min(lim)
    }

    /// GNU `limfield`: where `key` ends in `line`.
    pub(super) fn limfield(&self, line: &[u8], key: &SortKey) -> usize {
        let lim = line.len();
        let echar = key.echar;
        let mut eword = key.eword;
        if echar == 0 {
            eword = eword.wrapping_add(1); // all of the end field
        }
        let mut p = 0;
        if let Some(t) = self.tab {
            while p < lim && eword > 0 {
                eword -= 1;
                while p < lim && line[p] != t {
                    p += 1;
                }
                if p < lim && (eword > 0 || echar > 0) {
                    p += 1;
                }
            }
        } else {
            while p < lim && eword > 0 {
                eword -= 1;
                while p < lim && blank(line[p]) {
                    p += 1;
                }
                while p < lim && !blank(line[p]) {
                    p += 1;
                }
            }
        }
        if echar != 0 {
            if key.opts.skipeblanks {
                while p < lim && blank(line[p]) {
                    p += 1;
                }
            }
            p = p.saturating_add(echar).min(lim);
        }
        p
    }

    /// Where the bytes `keycompare` compares for `key` start and end in
    /// `line`: empty when the limit comes before the start.
    pub(super) fn key_bounds(&self, line: &[u8], key: &SortKey) -> (usize, usize) {
        let lim = if key.eword == NONE {
            line.len()
        } else {
            self.limfield(line, key)
        };
        let beg = if key.sword != NONE {
            self.begfield(line, key)
        } else {
            let mut b = 0;
            if key.opts.skipsblanks {
                while b < lim && blank(line[b]) {
                    b += 1;
                }
            }
            b
        };
        (beg, lim.max(beg))
    }

    pub(super) fn key_slice<'a>(&self, line: &'a [u8], key: &SortKey) -> &'a [u8] {
        let (beg, lim) = self.key_bounds(line, key);
        &line[beg..lim]
    }

    /// GNU `keycompare` from key `first` on.
    pub(super) fn keycompare_from(&self, first: usize, a: &[u8], b: &[u8]) -> Ordering {
        for key in &self.keys[first..] {
            let ord = self.key_order(key, self.key_slice(a, key), self.key_slice(b, key));
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    }

    /// The order of two extracted keys under `key`'s options, reversal
    /// included.
    pub(super) fn key_order(&self, key: &SortKey, ka: &[u8], kb: &[u8]) -> Ordering {
        let o = &key.opts;
        let ord = if o.key_numeric() || o.month || o.random || o.version {
            // These orderings see the key after -d/-i/-f rewriting.
            let (ka, kb): (Cow<[u8]>, Cow<[u8]>) = if o.filters() {
                (filtered(ka, o).into(), filtered(kb, o).into())
            } else {
                (ka.into(), kb.into())
            };
            if o.numeric {
                numeric_compare(&ka, &kb)
            } else if o.general {
                general_key(&ka).cmp(&general_key(&kb))
            } else if o.human {
                human_compare(&ka, &kb)
            } else if o.month {
                month_of(&ka).0.cmp(&month_of(&kb).0)
            } else if o.random {
                self.random_compare(&ka, &kb)
            } else {
                filevercmp(&ka, &kb)
            }
        } else if o.filters() {
            let keep = |c: &u8| !o.ignore.ignores(*c);
            let fold = |c: &u8| {
                if o.translate {
                    c.to_ascii_uppercase()
                } else {
                    *c
                }
            };
            ka.iter()
                .filter(|c| keep(c))
                .map(fold)
                .cmp(kb.iter().filter(|c| keep(c)).map(fold))
        } else {
            ka.cmp(kb)
        };
        if o.reverse { ord.reverse() } else { ord }
    }

    /// GNU `compare_random`: order by the seeded MD5 of each key, falling back
    /// to the bytes themselves so that only identical keys compare equal.
    pub(super) fn random_compare(&self, a: &[u8], b: &[u8]) -> Ordering {
        let digest = |key: &[u8]| self.random_seed.clone().chain_update(key).finalize();
        digest(a).cmp(&digest(b)).then_with(|| a.cmp(b))
    }

    /// The last-resort comparison of whole lines.
    pub(super) fn last_resort(&self, a: &[u8], b: &[u8]) -> Ordering {
        let ord = a.cmp(b);
        if self.reverse { ord.reverse() } else { ord }
    }

    /// GNU `compare`, for lines whose first key (`keybeg`/`keylim`) was
    /// found already.
    pub(super) fn compare_pre(&self, a: &[u8], ka: &[u8], b: &[u8], kb: &[u8]) -> Ordering {
        if let Some(first) = self.keys.first() {
            let ord = self
                .key_order(first, ka, kb)
                .then_with(|| self.keycompare_from(1, a, b));
            if ord != Ordering::Equal || self.no_last_resort {
                return ord;
            }
        }
        self.last_resort(a, b)
    }
}

/// A key after `-d`/`-i` deletion and `-f` folding.
pub(super) fn filtered(key: &[u8], o: &SortOpts) -> Vec<u8> {
    key.iter()
        .filter(|&&c| !o.ignore.ignores(c))
        .map(|&c| {
            if o.translate {
                c.to_ascii_uppercase()
            } else {
                c
            }
        })
        .collect()
}

/// A number as GNU `strnumcmp` sees it: sign, integer digits without leading
/// zeros, fraction digits without trailing zeros.
pub(super) struct SortNum<'a> {
    neg: bool,
    int: &'a [u8],
    frac: &'a [u8],
}

impl SortNum<'_> {
    /// The significant digits kept (for the fuzz targets' checks).
    #[cfg(feature = "fuzzing")]
    pub(super) fn digits(&self) -> usize {
        self.int.len() + self.frac.len()
    }
}

/// Parses the leading `[blanks][-]digits[.digits]` of a key.
pub(super) fn parse_sort_num(s: &[u8]) -> SortNum<'_> {
    let mut i = 0;
    while i < s.len() && blank(s[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && s[i] == b'-' {
        neg = true;
        i += 1;
    }
    let st = i;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    let mut int = &s[st..i];
    while let [b'0', rest @ ..] = int {
        int = rest;
    }
    let mut frac: &[u8] = &[];
    if i < s.len() && s[i] == b'.' {
        i += 1;
        let fs = i;
        while i < s.len() && s[i].is_ascii_digit() {
            i += 1;
        }
        frac = &s[fs..i];
        while let [rest @ .., b'0'] = frac {
            frac = rest;
        }
    }
    if int.is_empty() && frac.is_empty() {
        neg = false; // -0 == 0, and non-numbers are zero
    }
    SortNum { neg, int, frac }
}

pub(super) fn numeric_compare(a: &[u8], b: &[u8]) -> Ordering {
    num_order(&parse_sort_num(a), &parse_sort_num(b))
}

pub(super) fn num_order(x: &SortNum<'_>, y: &SortNum<'_>) -> Ordering {
    let mag = |p: &SortNum, q: &SortNum| {
        p.int
            .len()
            .cmp(&q.int.len())
            .then_with(|| p.int.cmp(q.int))
            .then_with(|| p.frac.cmp(q.frac))
    };
    match (x.neg, y.neg) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        (false, false) => mag(x, y),
        (true, true) => mag(y, x),
    }
}

/// GNU `traverse_raw_number` (C locale: no thousands separator): the end of
/// the digits, decimal point and fraction digits starting `s[from..]`;
/// whether a nonzero digit was seen (`'0' < max_digit`), and any digit.
pub(super) fn traverse_raw_number(s: &[u8], from: usize) -> (usize, bool, bool) {
    let mut i = from;
    let mut max = 0u8;
    while i < s.len() && s[i].is_ascii_digit() {
        max = max.max(s[i]);
        i += 1;
    }
    if i < s.len() && s[i] == b'.' {
        i += 1;
        while i < s.len() && s[i].is_ascii_digit() {
            max = max.max(s[i]);
            i += 1;
        }
    }
    (i, max > b'0', max >= b'0')
}

/// GNU `unit_order`: the magnitude of an SI suffix letter.
pub(super) fn unit_order(c: Option<&u8>) -> i32 {
    match c {
        Some(b'K' | b'k') => 1,
        Some(b'M') => 2,
        Some(b'G') => 3,
        Some(b'T') => 4,
        Some(b'P') => 5,
        Some(b'E') => 6,
        Some(b'Z') => 7,
        Some(b'Y') => 8,
        Some(b'R') => 9,
        Some(b'Q') => 10,
        _ => 0,
    }
}

/// GNU `find_unit_order` (after leading blanks): signed suffix magnitude of
/// a human-readable number.
fn human_unit_order(s: &[u8]) -> i32 {
    let start = s.iter().take_while(|&&c| blank(c)).count();
    let neg = s.get(start) == Some(&b'-');
    let (end, nonzero, _) = traverse_raw_number(s, start + usize::from(neg));
    if !nonzero {
        return 0;
    }
    let order = unit_order(s.get(end));
    if neg { -order } else { order }
}

/// GNU `human_numcompare`.
pub(super) fn human_compare(a: &[u8], b: &[u8]) -> Ordering {
    human_unit_order(a)
        .cmp(&human_unit_order(b))
        .then_with(|| numeric_compare(a, b))
}

/// GNU `general_numcompare`, as a key: see [`ldbl`].
pub(super) fn general_key(s: &[u8]) -> i128 {
    ldbl::strtold_key(s).0
}

/// GNU `getmonth`: 0 for unknown, else 1..=12 for a key that starts (after
/// blanks) with a case-insensitive `JAN`..`DEC`; and where the name ends.
pub(super) fn month_of(key: &[u8]) -> (u8, usize) {
    const MONTHS: [&[u8]; 12] = [
        b"JAN", b"FEB", b"MAR", b"APR", b"MAY", b"JUN", b"JUL", b"AUG", b"SEP", b"OCT", b"NOV",
        b"DEC",
    ];
    let start = key.iter().take_while(|&&c| blank(c)).count();
    let Some(abbr) = key[start..].get(..3) else {
        return (0, start);
    };
    MONTHS
        .iter()
        .position(|m| abbr.eq_ignore_ascii_case(m))
        .map_or((0, start), |i| (i as u8 + 1, start + 3))
}

/// gnulib `file_prefixlen`: the length of `s` without its file suffix, the
/// longest tail matching `(\.[A-Za-z~][A-Za-z0-9~]*)*$` that leaves at least
/// one leading byte.
fn file_prefix_len(s: &[u8]) -> usize {
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

/// gnulib `order`: the version-sort weight of the byte at `pos`. `~` sorts
/// first, then the end of the string, then letters, then all other bytes.
fn version_weight(s: &[u8], pos: usize) -> i32 {
    match s.get(pos) {
        None => -1,
        Some(b'~') => -2,
        Some(c) if c.is_ascii_digit() => 0,
        Some(c) if c.is_ascii_alphabetic() => i32::from(*c),
        Some(&c) => i32::from(c) + 256,
    }
}

/// gnulib `verrevcmp`: Debian-style comparison of alternating non-digit and
/// digit runs, the latter compared numerically.
fn verrevcmp(a: &[u8], b: &[u8]) -> Ordering {
    let digit_at = |s: &[u8], i: usize| s.get(i).is_some_and(u8::is_ascii_digit);
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        while (i < a.len() && !digit_at(a, i)) || (j < b.len() && !digit_at(b, j)) {
            let ord = version_weight(a, i).cmp(&version_weight(b, j));
            if ord != Ordering::Equal {
                return ord;
            }
            i += 1;
            j += 1;
        }
        while a.get(i) == Some(&b'0') {
            i += 1;
        }
        while b.get(j) == Some(&b'0') {
            j += 1;
        }
        let mut first_diff = Ordering::Equal;
        while digit_at(a, i) && digit_at(b, j) {
            first_diff = first_diff.then(a[i].cmp(&b[j]));
            i += 1;
            j += 1;
        }
        if digit_at(a, i) {
            return Ordering::Greater;
        }
        if digit_at(b, j) {
            return Ordering::Less;
        }
        if first_diff != Ordering::Equal {
            return first_diff;
        }
    }
    Ordering::Equal
}

/// gnulib `filenvercmp`, the `-V` ordering: empty first, then `.`, `..` and
/// other dot-files, then a version comparison that ignores file suffixes
/// (`.tar.gz`) unless they are needed to break a tie.
pub(super) fn filevercmp(a: &[u8], b: &[u8]) -> Ordering {
    match (a, b) {
        ([], []) => return Ordering::Equal,
        ([], _) => return Ordering::Less,
        (_, []) => return Ordering::Greater,
        ([b'.', ..], [b'.', ..]) => {
            for special in [&b"."[..], b".."] {
                match (a == special, b == special) {
                    (true, true) => return Ordering::Equal,
                    (true, false) => return Ordering::Less,
                    (false, true) => return Ordering::Greater,
                    (false, false) => {}
                }
            }
        }
        ([b'.', ..], _) => return Ordering::Less,
        (_, [b'.', ..]) => return Ordering::Greater,
        _ => {}
    }
    let (pa, pb) = (file_prefix_len(a), file_prefix_len(b));
    let ord = verrevcmp(&a[..pa], &b[..pb]);
    if ord != Ordering::Equal || (pa == a.len() && pb == b.len()) {
        ord
    } else {
        verrevcmp(a, b)
    }
}
