//! A tool's `--help` text, taken apart: usage lines, prose, one entry per
//! option, and the exit statuses. Two layouts occur, GNU's
//! (`  -a, --all     do not ignore ...`) and clap's (`Options:` and
//! `  -n, --lines <NUM>  Output ...`); both put an option's spellings at the
//! start of a line and continue its description on lines indented further.

use super::{Manual, Opt};

/// The parts of a help text.
pub(crate) struct Help {
    /// The usage lines without `Usage:` or `or:` (`ls [OPTION]... [FILE]...`).
    pub(crate) usage: Vec<String>,
    /// Prose before the first option.
    pub(crate) description: Vec<Block>,
    /// GNU's "Mandatory arguments to long options are mandatory for short
    /// options too."
    pub(crate) options_intro: Vec<String>,
    /// From the first option on: the option entries, and the prose between
    /// and after them (which explains their arguments).
    pub(crate) options: Vec<Item>,
    /// The `Exit status:` list, as (status, meaning).
    pub(crate) exit_status: Vec<(String, String)>,
}

/// A paragraph of prose.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Block {
    /// Running text, to be filled.
    Text(Vec<String>),
    /// Laid out by hand (a table, a list): kept line for line.
    Pre(Vec<String>),
}

pub(crate) enum Item {
    Entry(Entry),
    Block(Block),
}

/// An entry's spellings, each with its argument as written.
pub(crate) type Head = Vec<(String, String)>;

/// One option's entry in the help text.
pub(crate) struct Entry {
    /// The spellings it starts with, each with its argument as written
    /// (`=SIZE`, `[=WHEN]`, ` <FILE>`, or empty).
    pub(crate) head: Head,
    /// The description, one help line per element, trimmed.
    pub(crate) desc: Vec<String>,
    /// The option groups (see [`Opt::group`]) the spellings belong to.
    pub(crate) groups: Vec<usize>,
}

impl Entry {
    /// The description as one line.
    pub(crate) fn desc_line(&self) -> String {
        self.desc.join(" ")
    }
}

impl Help {
    /// Splits `text`, the help of the tool `m` describes; only spellings `m`
    /// accepts start an option entry.
    pub(crate) fn parse(text: &str, m: &Manual) -> Self {
        let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
        let mut help = Help {
            usage: Vec::new(),
            description: Vec::new(),
            options_intro: Vec::new(),
            options: Vec::new(),
            exit_status: Vec::new(),
        };
        let mut items: Vec<Item> = Vec::new();
        let mut para: Vec<&str> = Vec::new();
        let flush = |para: &mut Vec<&str>, items: &mut Vec<Item>| {
            if !para.is_empty() {
                items.push(Item::Block(block(para)));
                para.clear();
            }
        };
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i];
            let trimmed = line.trim_start();
            i += 1;
            if trimmed.is_empty() {
                flush(&mut para, &mut items);
            } else if let Some(u) = trimmed.strip_prefix("Usage:") {
                flush(&mut para, &mut items);
                help.usage.push(u.trim().to_string());
                while let Some(u) = lines
                    .get(i)
                    .and_then(|l| l.trim_start().strip_prefix("or:"))
                {
                    help.usage.push(u.trim().to_string());
                    i += 1;
                }
            } else if trimmed.eq_ignore_ascii_case("exit status:") {
                flush(&mut para, &mut items);
                while let Some(l) = lines.get(i).filter(|l| !l.trim().is_empty()) {
                    let l = l.trim();
                    let (code, rest) = l.split_once(char::is_whitespace).unwrap_or((l, ""));
                    let is_code = code == "-" || code.bytes().all(|b| b.is_ascii_digit());
                    match help.exit_status.last_mut() {
                        Some(last) if !is_code => {
                            last.1.push(' ');
                            last.1.push_str(l);
                        }
                        _ => help
                            .exit_status
                            .push((code.to_string(), rest.trim().to_string())),
                    }
                    i += 1;
                }
            } else if trimmed == "Options:" {
                flush(&mut para, &mut items);
            } else if trimmed.starts_with("Mandatory arguments to long options") {
                flush(&mut para, &mut items);
                help.options_intro.push(trimmed.to_string());
            } else if let Some((indent, head, rest)) = entry_head(line, m) {
                flush(&mut para, &mut items);
                let mut desc = Vec::new();
                if !rest.is_empty() {
                    desc.push(rest.to_string());
                }
                // Continuation lines are indented further and start no entry.
                while let Some(l) = lines.get(i) {
                    let ind = l.len() - l.trim_start().len();
                    if l.trim().is_empty() || ind <= indent || entry_head(l, m).is_some() {
                        break;
                    }
                    desc.push(l.trim().to_string());
                    i += 1;
                }
                let mut groups = Vec::new();
                for (spelling, _) in &head {
                    for o in m.opts.iter().filter(|o| o.spellings().contains(spelling)) {
                        if !groups.contains(&o.group) {
                            groups.push(o.group);
                        }
                    }
                }
                items.push(Item::Entry(Entry { head, desc, groups }));
            } else {
                para.push(line);
            }
        }
        flush(&mut para, &mut items);
        let first = items
            .iter()
            .position(|it| matches!(it, Item::Entry(_)))
            .unwrap_or(items.len());
        help.options = items.split_off(first);
        help.description = items
            .into_iter()
            .filter_map(|it| match it {
                Item::Block(b) => Some(b),
                Item::Entry(_) => None,
            })
            .collect();
        help
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.options.iter().filter_map(|it| match it {
            Item::Entry(e) => Some(e),
            Item::Block(_) => None,
        })
    }

    /// Whether some entry documents `o` or a synonym of it.
    #[cfg(test)]
    pub(crate) fn documents(&self, o: &Opt) -> bool {
        self.entries().any(|e| e.groups.contains(&o.group))
    }

    /// Whether some entry starts with `spelling`.
    pub(crate) fn mentions(&self, spelling: &str) -> bool {
        self.entries()
            .any(|e| e.head.iter().any(|(s, _)| s == spelling))
    }

    /// The first entry that documents `o`.
    pub(crate) fn entry_of(&self, o: &Opt) -> Option<&Entry> {
        self.entries().find(|e| e.groups.contains(&o.group))
    }

    /// The name of `o`'s argument as the help writes it (`SIZE` for
    /// `--block-size=SIZE`), from its own spelling in the entry or else a
    /// synonym's.
    pub(crate) fn arg_name(&self, o: &Opt) -> Option<String> {
        let e = self.entry_of(o)?;
        let spellings = o.spellings();
        let own = e
            .head
            .iter()
            .find(|(s, a)| spellings.contains(s) && !a.is_empty());
        let (_, arg) = own.or_else(|| e.head.iter().find(|(_, a)| !a.is_empty()))?;
        let name: String = arg
            .chars()
            .filter(|c| !matches!(c, '=' | '[' | ']' | '<' | '>' | ' '))
            .collect();
        (!name.is_empty()).then_some(name)
    }

    /// The first sentence of the description: what NAME says.
    pub(crate) fn summary(&self) -> String {
        let text = self
            .description
            .iter()
            .find_map(|b| match b {
                Block::Text(lines) => Some(lines.join(" ")),
                Block::Pre(_) => None,
            })
            .unwrap_or_default();
        let mut s = match text.find(". ") {
            Some(end) => text[..end].to_string(),
            None => text.trim_end_matches('.').to_string(),
        };
        // "List information about the FILEs (the current directory by default)"
        if let Some(paren) = s.find(" (")
            && s.ends_with(')')
        {
            s.truncate(paren);
        }
        s.trim().trim_end_matches(['.', ':']).to_string()
    }
}

/// A paragraph: running text unless any line is indented.
fn block(lines: &[&str]) -> Block {
    if lines.iter().any(|l| l.starts_with(' ')) {
        Block::Pre(lines.iter().map(|l| l.to_string()).collect())
    } else {
        Block::Text(lines.iter().map(|l| l.trim().to_string()).collect())
    }
}

/// If `line` starts an option entry of `m`'s tool: its indentation, its
/// spellings with their arguments, and the description that follows them on
/// the line.
fn entry_head<'a>(line: &'a str, m: &Manual) -> Option<(usize, Head, &'a str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if !(1..=10).contains(&indent) {
        return None;
    }
    let b = line.as_bytes();
    let mut pos = indent;
    let mut head = Vec::new();
    loop {
        let (spelling, end) = spelling_at(b, pos)?;
        pos = end;
        let takes_arg = m.opts.iter().any(|o| {
            o.arg != crate::common::getopt::HasArg::No && o.spellings().contains(&spelling)
        }) || m.switches.iter().any(|s| {
            s.arg != crate::common::getopt::HasArg::No && spelling == format!("--{}", s.long)
        });
        let arg_start = pos;
        match b.get(pos) {
            // `=SIZE`, `[=WHEN]`, `={name|descriptor}`: up to a blank.
            Some(b'=' | b'[') => {
                while pos < b.len() && b[pos] != b' ' && b[pos] != b',' {
                    pos += 1;
                }
            }
            // clap's ` <FILE>`, or GNU's `-t SEP` for an option taking one.
            Some(b' ') if b.get(pos + 1) == Some(&b'<') => {
                pos = line[pos..].find('>').map_or(b.len(), |e| pos + e + 1);
            }
            Some(b' ')
                if takes_arg
                    && b.get(pos + 1).is_some_and(|c| c.is_ascii_uppercase())
                    && line[pos + 1..].split(' ').next().is_some_and(|w| {
                        w.bytes().all(|c| c.is_ascii_uppercase() || c == b'_')
                    }) =>
            {
                pos += 1;
                while pos < b.len() && b[pos] != b' ' {
                    pos += 1;
                }
            }
            _ => {}
        }
        head.push((spelling, line[arg_start..pos].to_string()));
        if b.get(pos) == Some(&b',')
            && b.get(pos + 1) == Some(&b' ')
            && b.get(pos + 2) == Some(&b'-')
        {
            pos += 2;
        } else {
            break;
        }
    }
    if !head.iter().any(|(s, _)| m.knows(s)) {
        return None;
    }
    Some((indent, head, line[pos..].trim()))
}

/// The option spelling (`-x`, `--name`) at `pos` and where it ends.
fn spelling_at(b: &[u8], pos: usize) -> Option<(String, usize)> {
    let word = |c: &u8| c.is_ascii_alphanumeric() || *c == b'-';
    if b.get(pos) != Some(&b'-') {
        return None;
    }
    if b.get(pos + 1) == Some(&b'-') {
        let start = pos + 2;
        if !b.get(start).is_some_and(u8::is_ascii_alphanumeric) {
            return None;
        }
        let len = b[start..].iter().take_while(|c| word(c)).count();
        let end = start + len;
        return Some((String::from_utf8_lossy(&b[pos..end]).into_owned(), end));
    }
    let c = *b.get(pos + 1)?;
    if !c.is_ascii_graphic() || c == b'-' {
        return None;
    }
    // `-x` stands alone: `-bt` is prose.
    let end = pos + 2;
    match b.get(end) {
        None | Some(b' ' | b',' | b'=' | b'[' | b'<') => Some((format!("-{}", char::from(c)), end)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::getopt::{HasArg, OptSpec};

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum K {
        All,
        Size,
        Color,
        Lines,
        Zero,
        Rec,
        Help,
    }

    const SPECS: &[OptSpec<K>] = &[
        OptSpec::both(b'a', "all", HasArg::No, K::All),
        OptSpec::long("block-size", HasArg::Required, K::Size),
        OptSpec::long("color", HasArg::Optional, K::Color),
        OptSpec::both(b'n', "lines", HasArg::Required, K::Lines),
        OptSpec::both(b'z', "zero-terminated", HasArg::No, K::Zero),
        OptSpec::both(b'R', "recursive", HasArg::No, K::Rec),
        OptSpec::short(b'r', HasArg::No, K::Rec),
        OptSpec::long("help", HasArg::No, K::Help),
    ];

    const HELP: &str = "\
Usage: t [OPTION]... [FILE]...
  or:  t OPTION
List information about the FILEs (the current directory by default).
Sort entries alphabetically.

Mandatory arguments to long options are mandatory for short options too.
  -a, --all                  do not ignore entries starting with .
      --block-size=SIZE      scale sizes by SIZE
      --color[=WHEN]         color the output WHEN; more info below
                               and here
  -n, --lines <[+]NUM>  Output the last NUM lines
  -z, --zero-terminated Line delimiter is NUL
  -R, -r, --recursive        list subdirectories recursively
      --help     display this help and exit

WHEN is one of:
  always  never

Exit status:
 0  if OK,
 1  if minor problems (e.g., cannot access subdirectory),
 2  if serious trouble.
";

    fn parsed() -> (Manual, Help) {
        let m = Manual::new("t", HELP, SPECS);
        let h = m.parsed_help();
        (m, h)
    }

    #[test]
    fn splits_usage_prose_options_and_exit_status() {
        let (_, h) = parsed();
        assert_eq!(h.usage, ["t [OPTION]... [FILE]...", "t OPTION"]);
        assert_eq!(h.summary(), "List information about the FILEs");
        assert_eq!(h.options_intro.len(), 1);
        assert_eq!(h.entries().count(), 7);
        assert!(matches!(h.options.last(), Some(Item::Block(Block::Pre(_)))));
        assert_eq!(h.exit_status.len(), 3);
        assert_eq!(h.exit_status[2], ("2".into(), "if serious trouble.".into()));
    }

    #[test]
    fn reads_spellings_arguments_and_continuations() {
        let (m, h) = parsed();
        let e: Vec<&Entry> = h.entries().collect();
        assert_eq!(
            e[0].head,
            [("-a".into(), "".into()), ("--all".into(), "".into())]
        );
        assert_eq!(e[1].head, [("--block-size".into(), "=SIZE".into())]);
        assert_eq!(
            e[2].desc,
            ["color the output WHEN; more info below", "and here"]
        );
        assert_eq!(e[3].head[1], ("--lines".into(), " <[+]NUM>".into()));
        assert_eq!(e[4].desc, ["Line delimiter is NUL"]);
        assert_eq!(e[5].head.len(), 3);
        assert_eq!(e[5].groups.len(), 1);
        let lines = m.opts.iter().find(|o| o.short == Some(b'n')).unwrap();
        assert_eq!(h.arg_name(lines).as_deref(), Some("+NUM"));
        for o in &m.opts {
            assert!(h.documents(o), "{}", o.spelling());
        }
    }

    #[test]
    fn prose_is_not_an_option() {
        let (m, _) = parsed();
        assert!(entry_head("  -    the exit status of COMMAND otherwise", &m).is_none());
        assert!(entry_head("  -bt -d'\\:'", &m).is_none());
        assert!(entry_head("  --unknown  not ours", &m).is_none());
        assert!(entry_head("-a at the margin", &m).is_none());
    }
}
