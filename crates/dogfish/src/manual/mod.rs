//! Manual pages and shell completions, generated from the option tables the
//! parsers use, so that neither can drift from what a tool accepts.
//!
//! Each tool describes itself with a [`Manual`]: its `--help` text and the
//! `getopt_long` table its parser reads ([`OptSpec`]s), plus the words an
//! option argument may be where they are fixed (`--color=WHEN`). The help
//! text is split into its usage lines, prose and one entry per option
//! ([`help`]); every option in the table is matched to the entry that
//! documents it. From that, [`man`] writes `dogfish-TOOL.1` and
//! `dogfish.1`, and [`complete`] the bash, zsh and fish completions.
//!
//! `dogfish --generate KIND [TOOL]` prints them ([`generate`]);
//! `cargo xtask docs-gen` writes them all into a `share/` tree, and
//! `cargo xtask dist` ships that tree in the release archives.

mod complete;
mod help;
mod man;

use std::ffi::OsString;

use crate::common::getopt::{HasArg, OptSpec, Switch, exit_failure, switches_for};
use crate::tools;

pub(crate) use help::Help;

/// One spelling of an option, with its parser's key reduced to a group: the
/// options of one group are synonyms (`-r`, `--reverse`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Opt {
    pub(crate) short: Option<u8>,
    pub(crate) long: Option<&'static str>,
    pub(crate) arg: HasArg,
    pub(crate) group: usize,
}

impl Opt {
    /// `-x` or `--name`.
    #[cfg(test)]
    pub(crate) fn spelling(&self) -> String {
        match (self.short, self.long) {
            (_, Some(l)) => format!("--{l}"),
            (Some(s), None) => format!("-{}", char::from(s)),
            (None, None) => String::new(),
        }
    }

    /// Each spelling of this option: `-x` and `--name` for one that has both.
    pub(crate) fn spellings(&self) -> Vec<String> {
        let mut v = Vec::new();
        if let Some(s) = self.short {
            v.push(format!("-{}", char::from(s)));
        }
        if let Some(l) = self.long {
            v.push(format!("--{l}"));
        }
        v
    }
}

/// What a tool's manual page and completions are made from.
pub(crate) struct Manual {
    pub(crate) tool: &'static str,
    pub(crate) help: String,
    pub(crate) opts: Vec<Opt>,
    /// Option groups whose argument is one of a fixed set of words, with
    /// every word the parser accepts (synonyms included).
    pub(crate) choices: Vec<(usize, Vec<&'static str>)>,
    /// Dogfish's own switches (`--stats`, `--color`): the tool's
    /// [`switches_for`], minus any its own table shadows.
    pub(crate) switches: Vec<SwitchDoc>,
}

/// How an option is documented.
pub(crate) enum Doc<'h> {
    /// By an entry of the help text.
    Entry(&'h help::Entry),
    /// `--help` or `--version`, which some help texts leave out.
    Default(&'static str),
    /// Accepted as GNU accepts it (some only to say that Dogfish does not
    /// support them yet), but not described by `--help`: listed, without a
    /// description.
    Other,
    /// A parsing device rather than an option: neither listed nor offered.
    Hidden,
}

/// A Dogfish switch, as documented on every page that accepts it.
#[derive(Clone, Copy)]
pub(crate) struct SwitchDoc {
    pub(crate) long: &'static str,
    pub(crate) arg: HasArg,
    pub(crate) head: &'static str,
    pub(crate) desc: &'static str,
    pub(crate) choices: &'static [&'static str],
}

/// The words `--color=WHEN` accepts (`ColorMode::from_when`).
pub(crate) const COLOR_WHEN: &[&str] = &[
    "always", "yes", "force", "never", "no", "none", "auto", "tty", "if-tty",
];

const SWITCH_DOCS: &[(Switch, SwitchDoc)] = &[
    (
        Switch::STATS,
        SwitchDoc {
            long: "stats",
            arg: HasArg::No,
            head: "--stats",
            desc: "print timing and I/O statistics to standard error on exit \
                   (a Dogfish extension; also DOGFISH_STATS in the environment)",
            choices: &[],
        },
    ),
    (
        Switch::NO_COLOR,
        SwitchDoc {
            long: "no-color",
            arg: HasArg::No,
            head: "--no-color",
            desc: "plain output on a terminal, without colours or tables \
                   (a Dogfish extension)",
            choices: &[],
        },
    ),
    (
        Switch::COLOR,
        SwitchDoc {
            long: "color",
            arg: HasArg::Optional,
            head: "--color[=WHEN]",
            desc: "colour the rich terminal output: always (the default), never or \
                   auto (a Dogfish extension)",
            choices: COLOR_WHEN,
        },
    ),
];

impl Manual {
    /// A tool's manual: `help` is what `TOOL --help` prints and `specs` the
    /// table its parser reads.
    pub(crate) fn new<K: Copy + PartialEq>(
        tool: &'static str,
        help: impl Into<String>,
        specs: &[OptSpec<K>],
    ) -> Self {
        let opts = specs
            .iter()
            .map(|s| Opt {
                short: s.short,
                long: s.long,
                arg: s.arg,
                group: specs
                    .iter()
                    .position(|t| t.key == s.key)
                    .unwrap_or_default(),
            })
            .collect::<Vec<_>>();
        let allowed = switches_for(tool);
        let switches = SWITCH_DOCS
            .iter()
            .filter(|(sw, doc)| allowed.has(*sw) && !opts.iter().any(|o| o.long == Some(doc.long)))
            .map(|(_, doc)| *doc)
            .collect();
        Self {
            tool,
            help: help.into(),
            opts,
            choices: Vec::new(),
            switches,
        }
    }

    /// Records that `--long`'s argument is one of `words` (and so is that of
    /// every synonym of `--long`). Panics if the table has no `--long`: the
    /// manual tests build every manual.
    pub(crate) fn choices(
        mut self,
        long: &str,
        words: impl IntoIterator<Item = &'static str>,
    ) -> Self {
        let group = self
            .opts
            .iter()
            .find(|o| o.long == Some(long))
            .map(|o| o.group)
            .unwrap_or_else(|| panic!("{}: no option --{long}", self.tool));
        self.choices.push((group, words.into_iter().collect()));
        self
    }

    /// The fixed words `opt`'s argument may be, if any.
    pub(crate) fn choices_of(&self, opt: &Opt) -> &[&'static str] {
        self.choices
            .iter()
            .find(|(g, _)| *g == opt.group)
            .map_or(&[], |(_, w)| w.as_slice())
    }

    /// The help text, split up and matched to the options.
    pub(crate) fn parsed_help(&self) -> Help {
        Help::parse(&self.help, self)
    }

    /// How `o` is documented, given the tool's parsed `help`.
    pub(crate) fn doc<'h>(&self, help: &'h Help, o: &Opt) -> Doc<'h> {
        if let Some(e) = help.entry_of(o) {
            return Doc::Entry(e);
        }
        match o.long {
            Some("help") => return Doc::Default("display this help and exit"),
            Some("version") => return Doc::Default("output version information and exit"),
            // glibc's own `---presume-input-tty` and the like.
            Some(l) if l.starts_with('-') => return Doc::Hidden,
            Some(_) => return Doc::Other,
            None => {}
        }
        // A short option without a long synonym that is not a letter (`-5`
        // for `-n 5`, env's `-\t` on a shebang line), or one of several that
        // spell a single thing (chmod's `-w` as a MODE), is a parsing device,
        // not an option to offer.
        let group = || self.opts.iter().filter(move |p| p.group == o.group);
        let letter = o.short.is_some_and(|c| c.is_ascii_alphabetic());
        let has_long = group().any(|p| p.long.is_some());
        let shorts = group().filter(|p| p.short.is_some()).count();
        if !letter || (!has_long && shorts > 1) {
            Doc::Hidden
        } else {
            Doc::Other
        }
    }

    /// Whether `spelling` (`-x` or `--name`) is one of the tool's options or
    /// switches.
    pub(crate) fn knows(&self, spelling: &str) -> bool {
        self.opts
            .iter()
            .any(|o| o.spellings().iter().any(|s| s == spelling))
            || self
                .switches
                .iter()
                .any(|s| spelling.strip_prefix("--") == Some(s.long))
    }
}

/// The manual of `tool`, for every name `dogfish --list` prints.
pub(crate) fn manual(tool: &str) -> Option<Manual> {
    Some(match tool {
        "ls" => tools::ls::manual(),
        "cp" => tools::cp::manual(),
        "mv" => tools::mv::manual(),
        "rm" => tools::rm::manual(),
        "mkdir" => tools::mkdir::manual(),
        "ln" => tools::ln::manual(),
        "chmod" => tools::chmod::manual(),
        "chown" => tools::chown::manual(),
        "stat" => tools::stat::manual(),
        "touch" => tools::touch::manual(),
        "df" => tools::df::manual(),
        "du" => tools::du::manual(),
        "realpath" => tools::realpath::manual(),
        "basename" => tools::basename::manual(),
        "dirname" => tools::dirname::manual(),
        "mktemp" => tools::mktemp::manual(),
        "cat" => tools::cat::manual(),
        "tac" => tools::tac::manual(),
        "head" => tools::head::manual(),
        "tail" => tools::tail::manual(),
        "wc" => tools::wc::manual(),
        "sort" => tools::sort::manual(),
        "uniq" => tools::uniq::manual(),
        "cut" => tools::cut::manual(),
        "tr" => tools::tr::manual(),
        "paste" => tools::paste::manual(),
        "nl" => tools::nl::manual(),
        "shuf" => tools::shuf::manual(),
        "query" => tools::query::manual(),
        "b2sum" => tools::checksum::manual("b2sum"),
        "sha256sum" => tools::checksum::manual("sha256sum"),
        "md5sum" => tools::checksum::manual("md5sum"),
        "base64" => tools::basenc::manual("base64"),
        "basenc" => tools::basenc::manual("basenc"),
        "echo" => tools::echo::manual(),
        "printf" => tools::printf::manual(),
        "env" => tools::env::manual(),
        "printenv" => tools::printenv::manual(),
        "id" => tools::id::manual(),
        "uname" => tools::uname::manual(),
        "hostname" => tools::hostname::manual(),
        "nproc" => tools::nproc::manual(),
        "whoami" => tools::whoami::manual(),
        "yes" => tools::yes::manual(),
        "true" => tools::true_false::manual("true"),
        "false" => tools::true_false::manual("false"),
        "sleep" => tools::sleep::manual(),
        "timeout" => tools::timeout::manual(),
        "watch" => tools::watch::manual(),
        "seq" => tools::seq::manual(),
        "date" => tools::date::manual(),
        "tee" => tools::tee::manual(),
        "bench" => tools::bench::manual(),
        "undo" => tools::undo::manual(),
        "history" => tools::history::manual(),
        "init" => tools::init::manual(),
        "install-links" => tools::install_links::manual(),
        _ => return None,
    })
}

/// Every tool's manual, in `dogfish --list` order.
pub(crate) fn all() -> Vec<Manual> {
    crate::ALL_TOOLS
        .iter()
        .filter_map(|(name, _, _)| manual(name))
        .collect()
}

/// The tools GNU coreutils has too, whose GNU manual page is worth a pointer.
pub(crate) fn in_gnu_coreutils(tool: &str) -> bool {
    !matches!(
        tool,
        "query" | "bench" | "undo" | "history" | "init" | "install-links" | "watch" | "hostname"
    )
}

/// The status a tool exits with on failure (for the EXIT STATUS section).
pub(crate) fn failure_status(tool: &str) -> i32 {
    exit_failure(tool)
}

const GENERATE_USAGE: &str = "usage: dogfish --generate man TOOL|dogfish\n       \
     dogfish --generate bash|zsh|fish\n       \
     dogfish --generate options TOOL";

/// `dogfish --generate KIND [TOOL]`: prints a manual page (`man TOOL`, or
/// `man dogfish` for the multicall binary's own), a completion script
/// (`bash`, `zsh`, `fish`), or a tool's options one per line (`options
/// TOOL`: spelling, argument, and the words it may be, tab-separated; the
/// tests read it). Returns the exit status.
pub(crate) fn generate(args: &[OsString]) -> i32 {
    let args: Vec<&str> = args.iter().filter_map(|a| a.to_str()).collect();
    let text = match args.as_slice() {
        ["man", "dogfish"] => man::multicall_page(&all()),
        ["man", tool] => match manual(tool) {
            Some(m) => man::page(&m),
            None => return generate_error(&format!("unknown utility '{tool}'")),
        },
        ["bash"] => complete::bash(&all()),
        ["zsh"] => complete::zsh(&all()),
        ["fish"] => complete::fish(&all()),
        ["options", tool] => match manual(tool) {
            Some(m) => options_listing(&m),
            None => return generate_error(&format!("unknown utility '{tool}'")),
        },
        _ => return generate_error("missing or unknown arguments"),
    };
    dogfish_core::stdout::print_str(&text);
    0
}

fn generate_error(msg: &str) -> i32 {
    eprintln!("dogfish: --generate: {msg}\n{GENERATE_USAGE}");
    2
}

/// `--generate options TOOL`: `SPELLING\tARG\tWORD,WORD...` per option
/// spelling, then the switches; ARG is `none`, `required` or `optional`.
fn options_listing(m: &Manual) -> String {
    let arg = |a: HasArg| match a {
        HasArg::No => "none",
        HasArg::Required => "required",
        HasArg::Optional => "optional",
    };
    let mut out = String::new();
    for o in &m.opts {
        for s in o.spellings() {
            out.push_str(&format!(
                "{s}\t{}\t{}\n",
                arg(o.arg),
                m.choices_of(o).join(",")
            ));
        }
    }
    for s in &m.switches {
        out.push_str(&format!(
            "--{}\t{}\t{}\n",
            s.long,
            arg(s.arg),
            s.choices.join(",")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_a_manual() {
        for (name, _, _) in crate::ALL_TOOLS {
            let m = manual(name).unwrap_or_else(|| panic!("no manual for {name}"));
            assert_eq!(m.tool, *name);
            assert!(!m.opts.is_empty(), "{name}: no options");
        }
    }

    #[test]
    fn synonyms_share_a_group() {
        let m = tools::cat::manual();
        let group = |s: &str| {
            m.opts
                .iter()
                .find(|o| o.spellings().contains(&s.to_string()))
        };
        let n = group("-n").unwrap();
        assert_eq!(n.long, Some("number"));
        assert_ne!(group("-b").unwrap().group, n.group);
    }

    #[test]
    fn switches_follow_switches_for() {
        let names = |t: &str| {
            manual(t)
                .unwrap()
                .switches
                .iter()
                .map(|s| s.long)
                .collect::<Vec<_>>()
        };
        assert!(names("echo").is_empty());
        assert_eq!(names("cat"), ["stats", "no-color"]);
        assert_eq!(names("du"), ["stats", "no-color", "color"]);
        // ls has GNU's own --color.
        assert_eq!(names("ls"), ["stats", "no-color"]);
    }

    #[test]
    fn options_are_documented_listed_or_hidden() {
        let kind = |tool: &str, spelling: &str| {
            let m = manual(tool).unwrap();
            let help = m.parsed_help();
            let o = m
                .opts
                .iter()
                .find(|o| o.spellings().iter().any(|s| s == spelling))
                .unwrap_or_else(|| panic!("{tool} {spelling}"));
            match m.doc(&help, o) {
                Doc::Entry(_) => "entry",
                Doc::Default(_) => "default",
                Doc::Other => "other",
                Doc::Hidden => "hidden",
            }
        };
        assert_eq!(kind("ls", "--all"), "entry");
        assert_eq!(kind("ls", "-a"), "entry");
        assert_eq!(kind("cat", "--version"), "default");
        assert_eq!(kind("ls", "--show-control-chars"), "other");
        assert_eq!(kind("rm", "---presume-input-tty"), "hidden");
        assert_eq!(kind("head", "-5"), "hidden");
        assert_eq!(kind("chmod", "-w"), "hidden");
        // Most options are documented by an entry of the help text.
        let (mut entries, mut total) = (0, 0);
        for m in all() {
            let help = m.parsed_help();
            for o in &m.opts {
                total += 1;
                entries += usize::from(matches!(m.doc(&help, o), Doc::Entry(_)));
            }
        }
        assert!(entries * 10 > total * 8, "{entries} of {total}");
    }

    #[test]
    fn options_listing_names_arguments_and_choices() {
        let m = manual("ls").unwrap();
        let listing = options_listing(&m);
        assert!(listing.contains("--all\tnone\t\n"), "{listing}");
        assert!(
            listing.contains("--color\toptional\talways,yes,force,never,no,none,auto,tty,if-tty\n")
        );
        assert!(listing.contains("--stats\tnone\t\n"));
    }

    #[test]
    fn generate_refuses_unknown_requests() {
        let os = |a: &[&str]| a.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(generate(&os(&["man", "nonesuch"])), 2);
        assert_eq!(generate(&os(&["csh"])), 2);
        assert_eq!(generate(&os(&[])), 2);
    }
}
