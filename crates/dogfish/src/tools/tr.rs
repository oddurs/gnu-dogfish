//! tr — GNU `tr.c`: byte-oriented translate / squeeze / delete, with GNU's
//! parsing of `[:class:]`, `[=c=]`, `[c*n]` and ranges, and its validation
//! of SET1 and SET2.

use std::io::Write;

use anyhow::Result;
use clap::Parser;
use dogfish_core::{DogfishContext, gnu_io_error, io::ignore_broken_pipe};

use crate::common::input::{RawStdout, safe_read, stdin_file};

use crate::common::quote::quote_locale;

#[derive(Parser)]
#[command(
    name = "tr",
    about = "Translate, squeeze, and/or delete characters from standard input"
)]
struct TrArgs {
    /// Use the complement of SET1
    #[arg(short = 'c', visible_short_alias = 'C', long = "complement")]
    complement: bool,

    /// Delete characters in SET1, do not translate
    #[arg(short = 'd', long = "delete")]
    delete: bool,

    /// Replace each sequence of a repeated character with a single occurrence
    #[arg(short = 's', long = "squeeze-repeats")]
    squeeze: bool,

    /// First truncate SET1 to length of SET2
    #[arg(short = 't', long = "truncate-set1")]
    truncate: bool,

    /// Like GNU (`getopt "+..."`), options end at the first set: `tr a -b` maps `a`
    /// to `-b`, while `tr --bogus a b` is an unrecognized option.
    #[arg(trailing_var_arg = true)]
    sets: Vec<String>,
}

const N_CHARS: usize = 256;
/// `get_next` is about to start on the first element.
const BEGIN_STATE: u64 = u64::MAX - 1;
/// `get_next` is about to start on the current element.
const NEW_ELEMENT: u64 = u64::MAX;
/// The largest repeat count (and set length) GNU accepts.
const REPEAT_COUNT_MAXIMUM: u64 = BEGIN_STATE;

/// The twelve classes, in GNU's order.
const CLASS_NAMES: [&str; 12] = [
    "alnum", "alpha", "blank", "cntrl", "digit", "graph", "lower", "print", "punct", "space",
    "upper", "xdigit",
];
const CC_LOWER: usize = 6;
const CC_UPPER: usize = 10;

/// C-locale `<ctype.h>` membership for class `class` (an index into
/// [`CLASS_NAMES`]).
fn is_class_member(class: usize, c: u8) -> bool {
    match class {
        0 => c.is_ascii_alphanumeric(),
        1 => c.is_ascii_alphabetic(),
        2 => c == b' ' || c == b'\t',
        3 => c < 0x20 || c == 0x7f,
        4 => c.is_ascii_digit(),
        5 => c.is_ascii_graphic(),
        6 => c.is_ascii_lowercase(),
        7 => (0x20..0x7f).contains(&c),
        8 => c.is_ascii_punctuation(),
        9 => matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'),
        10 => c.is_ascii_uppercase(),
        _ => c.is_ascii_hexdigit(),
    }
}

/// Which case class a character came from (only `[:upper:]` / `[:lower:]`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum CaseClass {
    Lower,
    Upper,
    None,
}

/// One construct of a SET.
#[derive(Clone, Copy)]
enum Elem {
    Char(u8),
    Range(u8, u8),
    Class(usize),
    Equiv(u8),
    /// `[c*n]`; a count of 0 is `[c*]` (indefinite).
    Repeat(u8, u64),
}

/// A parsed SET and GNU's statistics about it (`struct Spec_list`).
struct SpecList {
    elems: Vec<Elem>,
    /// Traversal state for [`SpecList::get_next`].
    tail: usize,
    state: u64,
    length: u64,
    n_indefinite_repeats: usize,
    indefinite_repeat: usize,
    has_equiv_class: bool,
    has_char_class: bool,
    has_restricted_char_class: bool,
}

/// A SET after the escape pass: the bytes and which of them were escaped.
struct EString {
    s: Vec<u8>,
    escaped: Vec<bool>,
}

impl EString {
    fn is(&self, i: usize, c: u8) -> bool {
        self.s.get(i) == Some(&c) && !self.escaped[i]
    }
}

/// GNU's `make_printable_str`, for messages.
fn printable(s: &[u8]) -> String {
    let mut out = String::new();
    for &c in s {
        match c {
            b'\\' => out.push('\\'),
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x0b => out.push_str("\\v"),
            0x20..=0x7e => out.push(c as char),
            _ => out.push_str(&format!("\\{c:03o}")),
        }
    }
    out
}

/// GNU's `make_printable_char`.
fn printable_char(c: u8) -> String {
    if (0x20..0x7f).contains(&c) {
        (c as char).to_string()
    } else {
        format!("\\{c:03o}")
    }
}

/// GNU's `unquote`: resolves backslash escapes, warning (on stderr) about
/// ambiguous octal escapes and a trailing backslash.
fn unquote(s: &[u8]) -> EString {
    let mut es = EString {
        s: Vec::with_capacity(s.len()),
        escaped: Vec::with_capacity(s.len()),
    };
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut i = 0;
    while i < s.len() {
        if s[i] != b'\\' {
            es.s.push(s[i]);
            es.escaped.push(false);
            i += 1;
            continue;
        }
        let mut escaped = true;
        let mut step = 2;
        let c = match at(i + 1) {
            b'\\' => b'\\',
            b'a' => 0x07,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 0x0b,
            d @ b'0'..=b'7' => {
                let mut c = u32::from(d - b'0');
                if let o @ b'0'..=b'7' = at(i + 2) {
                    c = 8 * c + u32::from(o - b'0');
                    i += 1;
                    if let o @ b'0'..=b'7' = at(i + 2) {
                        if 8 * c + u32::from(o - b'0') < N_CHARS as u32 {
                            c = 8 * c + u32::from(o - b'0');
                            i += 1;
                        } else {
                            let (a, b, d) = (at(i) as char, at(i + 1) as char, at(i + 2) as char);
                            eprintln!(
                                "tr: warning: the ambiguous octal escape \\{a}{b}{d} is being\n\
                                 \tinterpreted as the 2-byte sequence \\0{a}{b}, {d}"
                            );
                        }
                    }
                }
                c as u8
            }
            0 if i + 1 >= s.len() => {
                eprintln!("tr: warning: an unescaped backslash at end of string is not portable");
                escaped = false;
                step = 1;
                b'\\'
            }
            other => other,
        };
        i += step;
        es.s.push(c);
        es.escaped.push(escaped);
    }
    es
}

/// The repeat count of `[c*n]`: octal when it starts with 0, else decimal
/// (C `strtoumax`, which also takes leading blanks and a `+`).
fn parse_repeat_count(digits: &[u8]) -> Option<u64> {
    let base = if digits.first() == Some(&b'0') { 8 } else { 10 };
    let mut i = digits
        .iter()
        .take_while(|&&b| matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
        .count();
    match digits.get(i) {
        Some(b'-') => return None,
        Some(b'+') => i += 1,
        _ => {}
    }
    let start = i;
    let mut n: u64 = 0;
    while let Some(&d) = digits.get(i) {
        let v = match d {
            b'0'..=b'9' if u64::from(d - b'0') < base => u64::from(d - b'0'),
            _ => break,
        };
        n = n.checked_mul(base)?.checked_add(v)?;
        i += 1;
    }
    (i > start && i == digits.len() && n <= REPEAT_COUNT_MAXIMUM).then_some(n)
}

/// GNU's `find_bracketed_repeat` for `[` at `start - 1`: `Ok(None)` when
/// this is no `[c*n]`, `Err` for an invalid count.
fn find_bracketed_repeat(es: &EString, start: usize) -> Result<Option<(u8, u64, usize)>, String> {
    if !es.is(start + 1, b'*') {
        return Ok(None);
    }
    let mut i = start + 2;
    while i < es.s.len() && !es.escaped[i] {
        if es.s[i] == b']' {
            let digits = &es.s[start + 2..i];
            let count = if digits.is_empty() {
                0
            } else {
                parse_repeat_count(digits).ok_or_else(|| {
                    format!(
                        "invalid repeat count {} in [c*n] construct",
                        quote_locale(printable(digits))
                    )
                })?
            };
            return Ok(Some((es.s[start], count, i)));
        }
        i += 1;
    }
    Ok(None)
}

/// GNU's `star_digits_closebracket`: `es[idx..]` matches `\*[0-9]*]`.
fn star_digits_closebracket(es: &EString, idx: usize) -> bool {
    if !es.is(idx, b'*') {
        return false;
    }
    for i in idx + 1..es.s.len() {
        if !es.s[i].is_ascii_digit() || es.escaped[i] {
            return es.is(i, b']');
        }
    }
    false
}

/// GNU's `build_spec_list`.
fn build_spec_list(es: &EString) -> Result<Vec<Elem>, String> {
    let p = &es.s;
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 < p.len() {
        if es.is(i, b'[') {
            if es.is(i + 1, b':') || es.is(i + 1, b'=') {
                let delim = p[i + 1];
                let close = (i + 2..p.len() - 1).find(|&j| {
                    p[j] == delim && p[j + 1] == b']' && !es.escaped[j] && !es.escaped[j + 1]
                });
                if let Some(close) = close {
                    let opnd = &p[i + 2..close];
                    if opnd.is_empty() {
                        return Err(if delim == b':' {
                            "missing character class name '[::]'".to_string()
                        } else {
                            "missing equivalence class character '[==]'".to_string()
                        });
                    }
                    let elem = if delim == b':' {
                        CLASS_NAMES
                            .iter()
                            .position(|n| n.as_bytes() == opnd)
                            .map(Elem::Class)
                    } else {
                        (opnd.len() == 1).then(|| Elem::Equiv(opnd[0]))
                    };
                    if let Some(elem) = elem {
                        out.push(elem);
                        i = close + 2;
                        continue;
                    }
                    // Not a class: maybe [:*N] or [=*N], a repeat of ':' or '='.
                    if !star_digits_closebracket(es, i + 2) {
                        return Err(if delim == b':' {
                            format!("invalid character class {}", quote_locale(printable(opnd)))
                        } else {
                            format!(
                                "{}: equivalence class operand must be a single character",
                                printable(opnd)
                            )
                        });
                    }
                }
            }
            if let Some((c, n, close)) = find_bracketed_repeat(es, i + 1)? {
                out.push(Elem::Repeat(c, n));
                i = close + 1;
                continue;
            }
        }
        // A range like a-z, or a normal character.
        if es.is(i + 1, b'-') {
            let (first, last) = (p[i], p[i + 2]);
            if last < first {
                return Err(format!(
                    "range-endpoints of '{}-{}' are in reverse collating sequence order",
                    printable_char(first),
                    printable_char(last)
                ));
            }
            out.push(Elem::Range(first, last));
            i += 3;
        } else {
            out.push(Elem::Char(p[i]));
            i += 1;
        }
    }
    out.extend(p[i..].iter().map(|&c| Elem::Char(c)));
    Ok(out)
}

impl SpecList {
    fn parse(arg: &str) -> Result<Self, String> {
        let elems = build_spec_list(&unquote(arg.as_bytes()))?;
        Ok(Self {
            elems,
            tail: 0,
            state: BEGIN_STATE,
            length: 0,
            n_indefinite_repeats: 0,
            indefinite_repeat: 0,
            has_equiv_class: false,
            has_char_class: false,
            has_restricted_char_class: false,
        })
    }

    fn rewind(&mut self) {
        self.state = BEGIN_STATE;
    }

    fn skip_construct(&mut self) {
        self.tail += 1;
        self.state = NEW_ELEMENT;
    }

    /// GNU's `get_next`: the next character of the expansion, with the case
    /// class it came from.
    fn get_next(&mut self) -> (Option<u8>, CaseClass) {
        if self.state == BEGIN_STATE {
            self.tail = 0;
            self.state = NEW_ELEMENT;
        }
        loop {
            let Some(&elem) = self.elems.get(self.tail) else {
                return (None, CaseClass::None);
            };
            let next = |s: &mut Self| {
                s.tail += 1;
                s.state = NEW_ELEMENT;
            };
            return match elem {
                Elem::Char(c) | Elem::Equiv(c) => {
                    next(self);
                    (Some(c), CaseClass::None)
                }
                Elem::Range(first, last) => {
                    self.state = if self.state == NEW_ELEMENT {
                        u64::from(first)
                    } else {
                        self.state + 1
                    };
                    let c = self.state as u8;
                    if c == last {
                        next(self);
                    }
                    (Some(c), CaseClass::None)
                }
                Elem::Class(class) => {
                    let case = match class {
                        CC_LOWER => CaseClass::Lower,
                        CC_UPPER => CaseClass::Upper,
                        _ => CaseClass::None,
                    };
                    if self.state == NEW_ELEMENT {
                        self.state = (0..=255u8)
                            .find(|&c| is_class_member(class, c))
                            .map_or(0, u64::from);
                    }
                    let c = self.state as u8;
                    match (c.saturating_add(1)..=255).find(|&n| n > c && is_class_member(class, n))
                    {
                        Some(n) => self.state = u64::from(n),
                        None => next(self),
                    }
                    (Some(c), case)
                }
                Elem::Repeat(c, n) => {
                    if n == 0 {
                        next(self);
                        continue;
                    }
                    if self.state == NEW_ELEMENT {
                        self.state = 0;
                    }
                    self.state += 1;
                    if self.state == n {
                        next(self);
                    }
                    (Some(c), CaseClass::None)
                }
            };
        }
    }

    /// GNU's `get_spec_stats`.
    fn compute_stats(&mut self) -> Result<(), String> {
        self.n_indefinite_repeats = 0;
        self.has_equiv_class = false;
        self.has_restricted_char_class = false;
        self.has_char_class = false;
        let mut length: u64 = 0;
        for (idx, elem) in self.elems.iter().enumerate() {
            let len = match *elem {
                Elem::Char(_) => 1,
                Elem::Range(a, b) => u64::from(b - a) + 1,
                Elem::Class(class) => {
                    self.has_char_class = true;
                    if class != CC_UPPER && class != CC_LOWER {
                        self.has_restricted_char_class = true;
                    }
                    (0..=255u8).filter(|&c| is_class_member(class, c)).count() as u64
                }
                Elem::Equiv(_) => {
                    self.has_equiv_class = true;
                    1
                }
                Elem::Repeat(_, n) if n > 0 => n,
                Elem::Repeat(..) => {
                    self.indefinite_repeat = idx;
                    self.n_indefinite_repeats += 1;
                    0
                }
            };
            length = match length.checked_add(len) {
                Some(l) if l <= REPEAT_COUNT_MAXIMUM => l,
                _ => return Err("too many characters in set".to_string()),
            };
        }
        self.length = length;
        Ok(())
    }

    /// GNU's `card_of_complement`.
    fn card_of_complement(&mut self) -> u64 {
        let mut in_set = [false; N_CHARS];
        self.rewind();
        while let (Some(c), _) = self.get_next() {
            in_set[c as usize] = true;
        }
        in_set.iter().filter(|&&b| !b).count() as u64
    }

    /// GNU's `homogeneous_spec_list`.
    fn is_homogeneous(&mut self) -> bool {
        self.rewind();
        let Some(first) = self.get_next().0 else {
            return false;
        };
        while let (Some(c), _) = self.get_next() {
            if c != first {
                return false;
            }
        }
        true
    }

    /// GNU's `set_initialize`.
    fn membership(&mut self, complement: bool) -> [bool; N_CHARS] {
        let mut in_set = [false; N_CHARS];
        self.rewind();
        while let (Some(c), _) = self.get_next() {
            in_set[c as usize] = true;
        }
        if complement {
            for b in &mut in_set {
                *b = !*b;
            }
        }
        in_set
    }
}

/// GNU's `validate_case_classes`: case classes in SET2 must line up with
/// case classes in SET1, and only count as one character each.
fn validate_case_classes(s1: &mut SpecList, s2: &mut SpecList) -> Result<(), String> {
    let n_upper = (0..=255u8).filter(|c| c.is_ascii_uppercase()).count() as u64;
    let n_lower = (0..=255u8).filter(|c| c.is_ascii_lowercase()).count() as u64;
    s1.rewind();
    s2.rewind();
    let (mut s1_new, mut s2_new) = (true, true);
    loop {
        let (c1, class1) = s1.get_next();
        let (c2, class2) = s2.get_next();
        if s2_new && class2 != CaseClass::None && !(s1_new && class1 != CaseClass::None) {
            return Err("misaligned [:upper:] and/or [:lower:] construct".to_string());
        }
        if class2 != CaseClass::None {
            s1.skip_construct();
            s2.skip_construct();
            let size = |c| {
                if c == CaseClass::Upper {
                    n_upper
                } else {
                    n_lower
                }
            };
            s1.length -= size(class1) - 1;
            s2.length -= size(class2) - 1;
        }
        s1_new = s1.state == NEW_ELEMENT;
        s2_new = s2.state == NEW_ELEMENT;
        if c1.is_none() || c2.is_none() {
            return Ok(());
        }
    }
}

/// GNU's `validate`: checks SET1 and SET2 against the options, resolves
/// `[c*]` in SET2 and extends SET2 to SET1's length when translating.
fn validate(
    s1: &mut SpecList,
    s2: Option<&mut SpecList>,
    opts: &TrArgs,
    translating: bool,
) -> Result<(), String> {
    s1.compute_stats()?;
    if opts.complement {
        s1.length = s1.card_of_complement();
    }
    if s1.n_indefinite_repeats > 0 {
        return Err("the [c*] repeat construct may not appear in string1".to_string());
    }
    let Some(s2) = s2 else {
        return Ok(());
    };
    s2.compute_stats()?;
    if s1.length >= s2.length && s2.n_indefinite_repeats == 1 {
        let gap = s1.length - s2.length;
        if let Elem::Repeat(_, n) = &mut s2.elems[s2.indefinite_repeat] {
            *n = gap;
        }
        s2.length = s1.length;
    }
    if s2.n_indefinite_repeats > 1 {
        return Err("only one [c*] repeat construct may appear in string2".to_string());
    }
    if !translating {
        if s2.n_indefinite_repeats > 0 {
            return Err(
                "the [c*] construct may appear in string2 only when translating".to_string(),
            );
        }
        return Ok(());
    }
    if s2.has_equiv_class {
        return Err("[=c=] expressions may not appear in string2 when translating".to_string());
    }
    if s2.has_restricted_char_class {
        return Err(
            "when translating, the only character classes that may appear in\n\
                    string2 are 'upper' and 'lower'"
                .to_string(),
        );
    }
    if !opts.complement && s2.has_char_class {
        validate_case_classes(s1, s2)?;
    }
    if s1.length > s2.length && !opts.truncate {
        if s2.length == 0 {
            return Err("when not truncating set1, string2 must be non-empty".to_string());
        }
        // string2_extend: repeat the last character of SET2.
        let last = match s2.elems.last() {
            Some(Elem::Char(c)) | Some(Elem::Range(_, c)) | Some(Elem::Repeat(c, _)) => *c,
            Some(Elem::Class(_)) => {
                return Err("when translating with string1 longer than string2,\n\
                            the latter string must not end with a character class"
                    .to_string());
            }
            _ => return Err("when not truncating set1, string2 must be non-empty".to_string()),
        };
        s2.elems.push(Elem::Repeat(last, s1.length - s2.length));
        s2.length = s1.length;
    }
    if opts.complement && s1.has_char_class && !(s2.length == s1.length && s2.is_homogeneous()) {
        return Err("when translating with complemented character classes,\n\
                    string2 must map all characters in the domain to one"
            .to_string());
    }
    Ok(())
}

/// The translation table (GNU's `xlate`).
fn build_xlate(s1: &mut SpecList, s2: &mut SpecList, complement: bool) -> [u8; N_CHARS] {
    let mut xlate: [u8; N_CHARS] = std::array::from_fn(|i| i as u8);
    if complement {
        let in_s1 = s1.membership(false);
        s2.rewind();
        for i in 0..N_CHARS {
            if !in_s1[i] {
                match s2.get_next().0 {
                    Some(c) => xlate[i] = c,
                    None => break,
                }
            }
        }
        return xlate;
    }
    s1.rewind();
    s2.rewind();
    loop {
        let (c1, class1) = s1.get_next();
        let (c2, class2) = s2.get_next();
        match (class1, class2) {
            (CaseClass::Lower, CaseClass::Upper) => {
                for c in b'a'..=b'z' {
                    xlate[c as usize] = c.to_ascii_uppercase();
                }
            }
            (CaseClass::Upper, CaseClass::Lower) => {
                for c in b'A'..=b'Z' {
                    xlate[c as usize] = c.to_ascii_lowercase();
                }
            }
            _ => match (c1, c2) {
                (Some(a), Some(b)) => xlate[a as usize] = b,
                _ => break,
            },
        }
        if class2 != CaseClass::None {
            s1.skip_construct();
            s2.skip_construct();
        }
    }
    xlate
}

pub fn run_tr(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = TrArgs::try_parse_from(args)?;
    let usage_err = |msg: String| {
        eprintln!("tr: {}\nTry 'tr --help' for more information.", msg);
        Ok(1)
    };
    let nsets = parsed.sets.len();
    let translating = nsets == 2 && !parsed.delete;
    let min_operands = 1 + usize::from(parsed.delete == parsed.squeeze);
    let max_operands = 1 + usize::from(parsed.delete <= parsed.squeeze);
    if nsets < min_operands {
        if nsets == 0 {
            return usage_err("missing operand".to_string());
        }
        let why = if parsed.squeeze {
            "Two strings must be given when both deleting and squeezing repeats."
        } else {
            "Two strings must be given when translating."
        };
        return usage_err(format!(
            "missing operand after {}\n{why}",
            quote_locale(&parsed.sets[nsets - 1])
        ));
    }
    if max_operands < nsets {
        let mut msg = format!("extra operand {}", quote_locale(&parsed.sets[max_operands]));
        if nsets == 2 {
            msg.push_str("\nOnly one string may be given when deleting without squeezing repeats.");
        }
        return usage_err(msg);
    }

    let fail = |msg: String| {
        eprintln!("tr: {msg}");
        Ok(1)
    };
    let mut s1 = match SpecList::parse(&parsed.sets[0]) {
        Ok(s) => s,
        Err(m) => return fail(m),
    };
    let mut s2 = match parsed.sets.get(1).map(|s| SpecList::parse(s)) {
        None => None,
        Some(Ok(s)) => Some(s),
        Some(Err(m)) => return fail(m),
    };
    if let Err(m) = validate(&mut s1, s2.as_mut(), &parsed, translating) {
        return fail(m);
    }

    let mut xlate: [u8; N_CHARS] = std::array::from_fn(|i| i as u8);
    let mut delete = [false; N_CHARS];
    let mut squeeze = [false; N_CHARS];
    match s2.as_mut() {
        None if parsed.squeeze => squeeze = s1.membership(parsed.complement),
        None => delete = s1.membership(parsed.complement),
        Some(s2) if parsed.delete => {
            delete = s1.membership(parsed.complement);
            squeeze = s2.membership(false);
        }
        Some(s2) => {
            xlate = build_xlate(&mut s1, s2, parsed.complement);
            if parsed.squeeze {
                squeeze = s2.membership(false);
            }
        }
    }

    ctx.telemetry.set_strategy("256-byte O(1) LUT stream");
    let squeezing = squeeze.iter().any(|&b| b);
    let deleting = delete.iter().any(|&b| b);
    let mut stdin = stdin_file();
    let mut out = RawStdout;
    let mut buf = vec![0u8; IO_SIZE];
    let mut last: Option<u8> = None;
    loop {
        let n = match safe_read(&mut *stdin, &mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                eprintln!("tr: read error: {}", gnu_io_error(&e));
                return Ok(1);
            }
        };
        ctx.telemetry.add_read(n as u64);
        let data = &mut buf[..n];
        // Each case filters the buffer in place, then writes it once.
        let kept = if !squeezing && !deleting {
            for b in data.iter_mut() {
                *b = xlate[*b as usize];
            }
            n
        } else if !squeezing {
            let mut k = 0;
            for i in 0..n {
                let b = data[i];
                data[k] = b;
                k += usize::from(!delete[b as usize]);
            }
            k
        } else {
            let mut k = 0;
            for i in 0..n {
                let b = data[i];
                if delete[b as usize] {
                    continue;
                }
                let c = xlate[b as usize];
                if squeeze[c as usize] && last == Some(c) {
                    continue;
                }
                data[k] = c;
                k += 1;
                last = Some(c);
            }
            k
        };
        ignore_broken_pipe(out.write_all(&data[..kept]))?;
        ctx.telemetry.add_written(kept as u64);
    }
    Ok(0)
}

/// Read size: large enough that syscalls are not the cost.
const IO_SIZE: usize = 128 * 1024;
