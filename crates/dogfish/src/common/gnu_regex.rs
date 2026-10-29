//! A small GNU regex work-alike for the default syntax `re_compile_pattern`
//! uses (`RE_SYNTAX_EMACS`, i.e. no syntax bits), as `tac -r` needs it,
//! and for POSIX basic syntax (`nl -bpBRE`, see [`Regex::new_basic`]).
//!
//! Syntax: `.` (any byte but newline), `[...]` / `[^...]` with ranges (no
//! `[:class:]`, backslash is literal inside), `*` `+` `?` (literal where
//! nothing precedes them), `^` / `$` (anchors only at the start / end of
//! the pattern or of a `\(...\)` / `\|` branch), `\(...\)`, `\|`, back
//! references `\1`..`\9`, and GNU's `\w \W \s \S \b \B \< \> \` \'`. Any
//! other escaped character is literal (`\{` too: no intervals). Matching
//! is POSIX-style: the longest match at a starting position. `^` and `$`
//! also match after and before a newline (`newline_anchor`).

#[derive(Debug, Clone)]
enum Node {
    Byte(u8),
    Any,
    /// `.` in POSIX syntax: any byte but NUL (newline included).
    AnyNotNul,
    Set(Box<[bool; 256]>),
    LineStart,
    LineEnd,
    BufStart,
    BufEnd,
    WordBoundary,
    NotWordBoundary,
    WordStart,
    WordEnd,
    Word(bool),
    Space(bool),
    Group(Vec<Vec<Node>>, usize),
    Backref(usize),
    Repeat(Box<Node>, usize, Option<usize>),
}

impl Node {
    fn is_anchor(&self) -> bool {
        matches!(
            self,
            Self::LineStart
                | Self::LineEnd
                | Self::BufStart
                | Self::BufEnd
                | Self::WordBoundary
                | Self::NotWordBoundary
                | Self::WordStart
                | Self::WordEnd
        )
    }
}

/// A compiled pattern.
#[derive(Debug, Clone)]
pub(crate) struct Regex {
    alts: Vec<Vec<Node>>,
}

struct Parser<'a> {
    p: &'a [u8],
    i: usize,
    groups: usize,
    /// Groups whose `\)` has been seen (valid back-reference targets).
    closed: Vec<usize>,
    /// POSIX basic syntax: `\+ \? \{m,n\}` are operators (`+ ?` are
    /// literal), `.` matches newline but not NUL, lists take `[:class:]`.
    basic: bool,
}

impl Parser<'_> {
    fn peek_is(&self, a: u8, b: Option<u8>) -> bool {
        self.p.get(self.i) == Some(&a) && b.is_none_or(|b| self.p.get(self.i + 1) == Some(&b))
    }

    /// At the end of a branch: end of pattern, `\)` or `\|`.
    fn at_branch_end(&self, i: usize) -> bool {
        i >= self.p.len()
            || (self.p[i] == b'\\' && matches!(self.p.get(i + 1), Some(b')') | Some(b'|')))
    }

    fn alternatives(&mut self, depth: usize) -> Result<Vec<Vec<Node>>, &'static str> {
        let mut alts = vec![self.branch(depth)?];
        while self.peek_is(b'\\', Some(b'|')) {
            self.i += 2;
            alts.push(self.branch(depth)?);
        }
        Ok(alts)
    }

    fn branch(&mut self, depth: usize) -> Result<Vec<Node>, &'static str> {
        let mut seq: Vec<Node> = Vec::new();
        // Whether a `*` here would have something to repeat.
        let mut can_repeat = false;
        while self.i < self.p.len() {
            let c = self.p[self.i];
            if c == b'\\' && matches!(self.p.get(self.i + 1), Some(b'|')) {
                break;
            }
            if c == b'\\' && matches!(self.p.get(self.i + 1), Some(b')')) {
                if depth == 0 {
                    return Err("Unmatched ) or \\)");
                }
                break;
            }
            // POSIX basic: \+ \? \{m,n\} repeat what precedes them.
            if self.basic
                && can_repeat
                && c == b'\\'
                && matches!(self.p.get(self.i + 1), Some(b'+' | b'?' | b'{'))
            {
                let op = self.p[self.i + 1];
                self.i += 2;
                let (min, max) = match op {
                    b'+' => (1, None),
                    b'?' => (0, Some(1)),
                    _ => self.interval()?,
                };
                if let Some(prev) = seq.pop() {
                    seq.push(Node::Repeat(Box::new(prev), min, max));
                }
                continue;
            }
            self.i += 1;
            let node = match c {
                b'+' | b'?' if self.basic => Node::Byte(c),
                b'*' | b'+' | b'?' if can_repeat => {
                    let Some(prev) = seq.pop() else {
                        continue;
                    };
                    let (min, max) = match c {
                        b'*' => (0, None),
                        b'+' => (1, None),
                        _ => (0, Some(1)),
                    };
                    seq.push(Node::Repeat(Box::new(prev), min, max));
                    continue;
                }
                b'^' if seq.is_empty() => {
                    seq.push(Node::LineStart);
                    // `^*` repeats a literal star.
                    can_repeat = false;
                    continue;
                }
                b'$' if self.at_branch_end(self.i) => Node::LineEnd,
                b'.' if self.basic => Node::AnyNotNul,
                b'.' => Node::Any,
                b'[' => self.bracket()?,
                b'\\' => {
                    let Some(&e) = self.p.get(self.i) else {
                        return Err("Trailing backslash");
                    };
                    self.i += 1;
                    match e {
                        b'(' => {
                            self.groups += 1;
                            let idx = self.groups;
                            let alts = self.alternatives(depth + 1)?;
                            if !self.peek_is(b'\\', Some(b')')) {
                                return Err("Unmatched ( or \\(");
                            }
                            self.i += 2;
                            self.closed.push(idx);
                            Node::Group(alts, idx)
                        }
                        b'1'..=b'9' => {
                            let n = usize::from(e - b'0');
                            if !self.closed.contains(&n) {
                                return Err("Invalid back reference");
                            }
                            Node::Backref(n)
                        }
                        b'w' => Node::Word(true),
                        b'W' => Node::Word(false),
                        b's' => Node::Space(true),
                        b'S' => Node::Space(false),
                        b'b' => Node::WordBoundary,
                        b'B' => Node::NotWordBoundary,
                        b'<' => Node::WordStart,
                        b'>' => Node::WordEnd,
                        b'`' => Node::BufStart,
                        b'\'' => Node::BufEnd,
                        other => Node::Byte(other),
                    }
                }
                other => Node::Byte(other),
            };
            // After an anchor a repetition operator is a literal (GNU: "^*").
            can_repeat = !node.is_anchor();
            seq.push(node);
        }
        Ok(seq)
    }

    /// `\{m,n\}`, the `\{` already consumed.
    fn interval(&mut self) -> Result<(usize, Option<usize>), &'static str> {
        const BAD: &str = "Invalid content of \\{\\}";
        let number = |me: &mut Self| {
            let start = me.i;
            while me.p.get(me.i).is_some_and(u8::is_ascii_digit) {
                me.i += 1;
            }
            std::str::from_utf8(&me.p[start..me.i])
                .ok()
                .and_then(|d| d.parse::<usize>().ok())
        };
        let min = number(self);
        let max = if self.p.get(self.i) == Some(&b',') {
            self.i += 1;
            number(self)
        } else {
            Some(min.ok_or(BAD)?)
        };
        if !self.peek_is(b'\\', Some(b'}')) {
            return Err(if self.i >= self.p.len() {
                "Unmatched \\{"
            } else {
                BAD
            });
        }
        self.i += 2;
        let min = min.unwrap_or(0);
        if max.is_some_and(|m| m < min) {
            return Err(BAD);
        }
        if min > 0x7fff || max.is_some_and(|m| m > 0x7fff) {
            return Err("Regular expression too big");
        }
        Ok((min, max))
    }

    /// `[...]`, the `[` already consumed.
    fn bracket(&mut self) -> Result<Node, &'static str> {
        const UNMATCHED: &str = "Unmatched [, [^, [:, [., or [=";
        let mut set = Box::new([false; 256]);
        let negate = self.p.get(self.i) == Some(&b'^');
        if negate {
            self.i += 1;
        }
        let mut first = true;
        loop {
            let Some(&c) = self.p.get(self.i) else {
                return Err(UNMATCHED);
            };
            if c == b']' && !first {
                self.i += 1;
                break;
            }
            first = false;
            self.i += 1;
            // POSIX lists: [:class:], [=c=], [.c.].
            if self.basic && c == b'[' && matches!(self.p.get(self.i), Some(b':' | b'=' | b'.')) {
                let kind = self.p[self.i];
                let body = self.i + 1;
                let close = (body..self.p.len().saturating_sub(1))
                    .find(|&j| self.p[j] == kind && self.p[j + 1] == b']')
                    .ok_or(UNMATCHED)?;
                let name = &self.p[body..close];
                self.i = close + 2;
                if kind == b':' {
                    let pred = class_predicate(name).ok_or("Invalid character class name")?;
                    for b in 0..=255u8 {
                        if pred(b) {
                            set[b as usize] = true;
                        }
                    }
                } else if let [one] = name {
                    set[*one as usize] = true;
                } else {
                    return Err("Invalid collation character");
                }
                continue;
            }
            if self.p.get(self.i) == Some(&b'-')
                && self.p.get(self.i + 1).is_some_and(|&e| e != b']')
            {
                let end = self.p[self.i + 1];
                self.i += 2;
                // Without RE_NO_EMPTY_RANGES a reversed range is empty.
                for b in c..=end {
                    set[b as usize] = true;
                }
            } else {
                set[c as usize] = true;
            }
        }
        // Without RE_HAT_LISTS_NOT_NEWLINE, `[^a]` matches newline too.
        if negate {
            for member in set.iter_mut() {
                *member = !*member;
            }
        }
        Ok(Node::Set(set))
    }
}

/// C-locale `<ctype.h>` classes by name.
fn class_predicate(name: &[u8]) -> Option<fn(u8) -> bool> {
    Some(match name {
        b"alnum" => |c: u8| c.is_ascii_alphanumeric(),
        b"alpha" => |c: u8| c.is_ascii_alphabetic(),
        b"blank" => |c: u8| c == b' ' || c == b'\t',
        b"cntrl" => |c: u8| c < 0x20 || c == 0x7f,
        b"digit" => |c: u8| c.is_ascii_digit(),
        b"graph" => |c: u8| c.is_ascii_graphic(),
        b"lower" => |c: u8| c.is_ascii_lowercase(),
        b"print" => |c: u8| (0x20..0x7f).contains(&c),
        b"punct" => |c: u8| c.is_ascii_punctuation(),
        b"space" => |c: u8| matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'),
        b"upper" => |c: u8| c.is_ascii_uppercase(),
        b"xdigit" => |c: u8| c.is_ascii_hexdigit(),
        _ => return None,
    })
}

impl Regex {
    /// Compiles `pattern` in GNU's default (Emacs) syntax, or returns GNU's
    /// message (`Unmatched ( or \(`, ...).
    pub(crate) fn new(pattern: &[u8]) -> Result<Self, &'static str> {
        Self::compile(pattern, false)
    }

    /// Compiles `pattern` as a POSIX basic regular expression (GNU's
    /// `RE_SYNTAX_POSIX_BASIC`, with empty ranges allowed).
    pub(crate) fn new_basic(pattern: &[u8]) -> Result<Self, &'static str> {
        Self::compile(pattern, true)
    }

    fn compile(pattern: &[u8], basic: bool) -> Result<Self, &'static str> {
        let mut parser = Parser {
            p: pattern,
            i: 0,
            groups: 0,
            closed: Vec::new(),
            basic,
        };
        let alts = parser.alternatives(0)?;
        if parser.i < pattern.len() {
            return Err("Unmatched ) or \\)");
        }
        Ok(Self { alts })
    }

    /// The end of the longest match starting at `start` within `text`
    /// (GNU `re_match`), if any.
    pub(crate) fn match_at(&self, text: &[u8], start: usize) -> Option<usize> {
        let mut m = Matcher {
            text,
            caps: [None; 10],
            best: None,
        };
        m.alternatives(&self.alts, start, &Cont::Done);
        m.best
    }

    /// Whether the pattern matches anywhere in `text` (`re_search` forward).
    pub(crate) fn is_match(&self, text: &[u8]) -> bool {
        (0..=text.len()).any(|s| self.match_at(text, s).is_some())
    }

    /// Searches backward like `re_search (..., start, -start)`: the match
    /// that starts last at or before `from`, as `(start, end)`, with every
    /// match confined to `text`.
    pub(crate) fn rsearch(&self, text: &[u8], from: usize) -> Option<(usize, usize)> {
        (0..=from.min(text.len()))
            .rev()
            .find_map(|s| self.match_at(text, s).map(|e| (s, e)))
    }
}

/// What to match after the current node.
enum Cont<'a> {
    Done,
    Seq(&'a [Node], &'a Cont<'a>),
    GroupEnd(usize, usize, &'a Cont<'a>),
    Repeat {
        node: &'a Node,
        min: usize,
        max: Option<usize>,
        count: usize,
        start: usize,
        next: &'a Cont<'a>,
    },
}

struct Matcher<'t> {
    text: &'t [u8],
    caps: [Option<(usize, usize)>; 10],
    best: Option<usize>,
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

impl Matcher<'_> {
    /// Whether a longer match is impossible (stop exploring).
    fn done(&self) -> bool {
        self.best == Some(self.text.len())
    }

    fn alternatives(&mut self, alts: &[Vec<Node>], pos: usize, k: &Cont<'_>) {
        for alt in alts {
            if self.done() {
                return;
            }
            self.seq(alt, pos, k);
        }
    }

    fn seq(&mut self, nodes: &[Node], pos: usize, k: &Cont<'_>) {
        match nodes.split_first() {
            None => self.cont(k, pos),
            Some((node, rest)) => self.node(node, pos, &Cont::Seq(rest, k)),
        }
    }

    fn cont(&mut self, k: &Cont<'_>, pos: usize) {
        if self.done() {
            return;
        }
        match *k {
            Cont::Done => self.best = Some(self.best.map_or(pos, |b| b.max(pos))),
            Cont::Seq(rest, next) => self.seq(rest, pos, next),
            Cont::GroupEnd(idx, start, next) => {
                let saved = self.caps[idx];
                self.caps[idx] = Some((start, pos));
                self.cont(next, pos);
                self.caps[idx] = saved;
            }
            Cont::Repeat {
                node,
                min,
                max,
                count,
                start,
                next,
            } => {
                // An iteration that matched nothing cannot lead anywhere new.
                if pos == start && count > min {
                    return;
                }
                self.repeat(node, min, max, count, pos, next);
            }
        }
    }

    fn repeat(
        &mut self,
        node: &Node,
        min: usize,
        max: Option<usize>,
        count: usize,
        pos: usize,
        k: &Cont<'_>,
    ) {
        if max.is_none_or(|m| count < m) {
            let again = Cont::Repeat {
                node,
                min,
                max,
                count: count + 1,
                start: pos,
                next: k,
            };
            self.node(node, pos, &again);
        }
        if count >= min {
            self.cont(k, pos);
        }
    }

    fn node(&mut self, node: &Node, pos: usize, k: &Cont<'_>) {
        let t = self.text;
        let at = |i: usize| t.get(i).copied();
        let before = pos.checked_sub(1).and_then(at);
        let word_before = before.is_some_and(is_word);
        let word_after = at(pos).is_some_and(is_word);
        let ok_here = |cond: bool, me: &mut Self| {
            if cond {
                me.cont(k, pos);
            }
        };
        let ok_byte = |cond: bool, me: &mut Self| {
            if cond {
                me.cont(k, pos + 1);
            }
        };
        match node {
            Node::Byte(c) => ok_byte(at(pos) == Some(*c), self),
            Node::Any => ok_byte(at(pos).is_some_and(|c| c != b'\n'), self),
            Node::AnyNotNul => ok_byte(at(pos).is_some_and(|c| c != 0), self),
            Node::Set(set) => ok_byte(at(pos).is_some_and(|c| set[c as usize]), self),
            Node::Word(want) => ok_byte(at(pos).is_some_and(|c| is_word(c) == *want), self),
            Node::Space(want) => ok_byte(
                at(pos).is_some_and(|c| (c.is_ascii_whitespace() || c == 0x0b) == *want),
                self,
            ),
            Node::LineStart => ok_here(pos == 0 || before == Some(b'\n'), self),
            Node::LineEnd => ok_here(pos == t.len() || at(pos) == Some(b'\n'), self),
            Node::BufStart => ok_here(pos == 0, self),
            Node::BufEnd => ok_here(pos == t.len(), self),
            Node::WordBoundary => ok_here(word_before != word_after, self),
            Node::NotWordBoundary => ok_here(word_before == word_after, self),
            Node::WordStart => ok_here(!word_before && word_after, self),
            Node::WordEnd => ok_here(word_before && !word_after, self),
            Node::Backref(n) => {
                if let Some((s, e)) = self.caps[*n]
                    && t[pos..].starts_with(&t[s..e])
                {
                    self.cont(k, pos + (e - s));
                }
            }
            Node::Group(alts, idx) => {
                let end = Cont::GroupEnd(*idx, pos, k);
                self.alternatives(alts, pos, &end);
            }
            Node::Repeat(inner, min, max) => self.repeat(inner, *min, *max, 0, pos, k),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Regex;

    fn longest(p: &str, t: &str, at: usize) -> Option<usize> {
        Regex::new(p.as_bytes()).unwrap().match_at(t.as_bytes(), at)
    }

    #[test]
    fn matches_longest() {
        assert_eq!(longest("\\._+", "._2", 0), Some(2));
        assert_eq!(longest("\\._+", ".___4", 0), Some(4));
        assert_eq!(longest("a*", "aaab", 0), Some(3));
        assert_eq!(longest("\\(ab\\|a\\)c", "abc", 0), Some(3));
        assert_eq!(longest("\\(a\\)\\1", "aa", 0), Some(2));
        assert_eq!(longest("[^a]", "b", 0), Some(1));
        assert_eq!(longest("*a", "*a", 0), Some(2));
        assert_eq!(longest("a\\{2\\}", "a{2}", 0), Some(4));
        assert_eq!(longest("x$", "x\ny", 0), Some(1));
        assert_eq!(longest("^b", "a\nb", 2), Some(3));
    }

    #[test]
    fn basic_syntax() {
        let m = |p: &str, t: &str| {
            Regex::new_basic(p.as_bytes())
                .unwrap()
                .is_match(t.as_bytes())
        };
        assert!(m("a\\{2\\}", "xaa"));
        assert!(!m("a\\{3\\}", "aa"));
        assert!(m("a+", "a+"));
        assert!(!m("a+", "aa"));
        assert!(m("ab\\+c", "abbc"));
        assert!(m("[[:digit:]]x", "7x"));
        assert!(m("^b", "b"));
        assert!(!m("^b", "ab"));
        assert_eq!(Regex::new_basic(b"a\\{2").unwrap_err(), "Unmatched \\{");
    }

    #[test]
    fn errors() {
        assert_eq!(Regex::new(b"\\(").unwrap_err(), "Unmatched ( or \\(");
        assert_eq!(Regex::new(b"a\\)").unwrap_err(), "Unmatched ) or \\)");
        assert_eq!(
            Regex::new(b"[a").unwrap_err(),
            "Unmatched [, [^, [:, [., or [="
        );
        assert_eq!(Regex::new(b"a\\").unwrap_err(), "Trailing backslash");
        assert_eq!(Regex::new(b"\\1").unwrap_err(), "Invalid back reference");
    }

    #[test]
    fn backward_search() {
        let re = Regex::new(b"\\._+").unwrap();
        assert_eq!(re.rsearch(b"1._2.__3", 7), Some((4, 7)));
    }
}
