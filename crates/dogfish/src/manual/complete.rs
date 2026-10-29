//! Completion scripts for bash, zsh and fish. Each covers `dogfish
//! UTILITY ...`, the `df-UTILITY` links `install-links --prefix df-` makes,
//! and (bash and fish) a plain `UTILITY` when the command of that name on
//! `$PATH` is the dogfish binary itself.

use std::fmt::Write as _;

use super::help::Help;
use super::{Doc, Manual};
use crate::common::getopt::HasArg;

/// What an option's argument is completed with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ArgKind {
    /// One of the option's fixed words.
    Words,
    Files,
    Dirs,
    /// Nothing to offer (a number, a size, a format).
    Nothing,
}

/// One option spelling, as the completions offer it.
struct CompOpt {
    /// `-a` or `--all`.
    spelling: String,
    arg: HasArg,
    kind: ArgKind,
    words: Vec<&'static str>,
    /// The argument's name, lower case (`size`), or `value`.
    arg_name: String,
    desc: String,
}

/// Whether the `.` at byte `i` of `s` ends a sentence (`word.`, not the
/// `.` of "entries starting with ." or "implied . and ..").
fn ends_sentence(s: &str, i: usize) -> bool {
    i > 0
        && s[..i]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == ')')
        && s[i + 1..].chars().next().is_none_or(|c| c == ' ')
}

/// Completion menus show a line per option: each description is cut to its
/// first clause, and to 80 characters.
fn short_desc(s: &str) -> String {
    let mut s = s.trim();
    for sep in ["; ", " (e.g.", " [alias", " [default"] {
        if let Some(i) = s.find(sep) {
            s = &s[..i];
        }
    }
    if let Some(i) = s
        .match_indices('.')
        .map(|(i, _)| i)
        .find(|&i| ends_sentence(s, i))
    {
        s = &s[..i];
    }
    let s = s.trim_end_matches([';', ',', ':']).trim_end();
    if s.chars().count() <= 80 {
        return s.to_string();
    }
    let cut: String = s.chars().take(77).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(a, _)| a);
    format!("{cut}...")
}

fn arg_kind(name: Option<&str>) -> ArgKind {
    let Some(name) = name else {
        return ArgKind::Files;
    };
    let n = name.to_ascii_uppercase();
    if n.contains("DIR") && !n.contains("FILE") {
        ArgKind::Dirs
    } else if [
        "FILE", "PATH", "SOURCE", "DEST", "TEMPLATE", "COMMAND", "PROGRAM",
    ]
    .iter()
    .any(|w| n.contains(w))
        || n == "F"
    {
        ArgKind::Files
    } else {
        ArgKind::Nothing
    }
}

/// Every spelling of every option `m` takes, switches included.
fn comp_opts(m: &Manual, help: &Help) -> Vec<CompOpt> {
    let mut out = Vec::new();
    for o in &m.opts {
        let words = m.choices_of(o).to_vec();
        let arg_name = help.arg_name(o);
        let kind = if !words.is_empty() {
            ArgKind::Words
        } else {
            arg_kind(arg_name.as_deref())
        };
        let desc = match m.doc(help, o) {
            Doc::Entry(e) => short_desc(&e.desc_line()),
            Doc::Default(d) => d.to_string(),
            Doc::Other => String::new(),
            Doc::Hidden => continue,
        };
        // A name, not a list of words (`{name|descriptor}`).
        let arg_name = arg_name
            .filter(|n| !n.contains(['{', '|']))
            .map(|n| {
                n.chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect::<String>()
                    .to_ascii_lowercase()
            })
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "value".into());
        for spelling in o.spellings() {
            out.push(CompOpt {
                spelling,
                arg: o.arg,
                kind,
                words: words.clone(),
                arg_name: arg_name.clone(),
                desc: desc.clone(),
            });
        }
    }
    for s in &m.switches {
        out.push(CompOpt {
            spelling: format!("--{}", s.long),
            arg: s.arg,
            kind: if s.choices.is_empty() {
                ArgKind::Nothing
            } else {
                ArgKind::Words
            },
            words: s.choices.to_vec(),
            arg_name: "when".into(),
            desc: short_desc(s.desc),
        });
    }
    out
}

/// A shell function name for `tool` (`install-links` -> `install_links`).
fn ident(tool: &str) -> String {
    tool.replace('-', "_")
}

/// The tools whose plain names are shell builtins: never taken over.
const BUILTINS: &[&str] = &["echo", "printf", "true", "false"];

fn tool_names(manuals: &[Manual]) -> String {
    manuals.iter().map(|m| m.tool).collect::<Vec<_>>().join(" ")
}

/// The bash completion (`share/bash-completion/completions/dogfish`).
/// Written for bash 3.2 too (macOS): no associative arrays, and `compopt`
/// only where it exists.
pub(crate) fn bash(manuals: &[Manual]) -> String {
    let mut out = String::new();
    out.push_str(
        "# bash completion for GNU/Dogfish: `dogfish UTILITY ...`, and the links
# `dogfish install-links` makes (df-UTILITY, UTILITY) where the command found
# on $PATH is dogfish itself.
# Generated by `dogfish --generate bash`; do not edit.

",
    );
    let _ = writeln!(out, "_dogfish_tools='{}'\n", tool_names(manuals));
    out.push_str(
        "# _dogfish_spec_UTILITY OPTION sets _dogfish_opts (every option; a long option
# that takes an argument also ends in =), _dogfish_args (the options whose
# argument may be the next word) and, for OPTION, _dogfish_kind (words, dirs or
# files: how its argument is completed) and _dogfish_words.
",
    );
    for m in manuals {
        let help = m.parsed_help();
        let opts = comp_opts(m, &help);
        let mut words = Vec::new();
        for o in &opts {
            words.push(o.spelling.clone());
            if o.arg != HasArg::No && o.spelling.starts_with("--") {
                words.push(format!("{}=", o.spelling));
            }
        }
        let separate: Vec<&str> = opts
            .iter()
            .filter(|o| o.arg == HasArg::Required)
            .map(|o| o.spelling.as_str())
            .collect();
        let _ = writeln!(out, "_dogfish_spec_{}() {{", ident(m.tool));
        let _ = writeln!(out, "    _dogfish_opts='{}'", words.join(" "));
        let _ = writeln!(out, "    _dogfish_args=' {} '", separate.join(" "));
        out.push_str("    _dogfish_words= _dogfish_kind=files\n    case $1 in\n");
        // One case per distinct completion, its options joined.
        let mut cases: Vec<(String, Vec<String>)> = Vec::new();
        for o in opts.iter().filter(|o| o.arg != HasArg::No) {
            let body = match o.kind {
                ArgKind::Words => {
                    format!("_dogfish_kind=words _dogfish_words='{}'", o.words.join(" "))
                }
                ArgKind::Dirs => "_dogfish_kind=dirs".to_string(),
                ArgKind::Nothing => "_dogfish_kind=none".to_string(),
                ArgKind::Files => continue,
            };
            match cases.iter_mut().find(|(b, _)| *b == body) {
                Some((_, pats)) => pats.push(format!("'{}'", o.spelling)),
                None => cases.push((body, vec![format!("'{}'", o.spelling)])),
            }
        }
        for (body, pats) in cases {
            let _ = writeln!(out, "        {}) {body} ;;", pats.join("|"));
        }
        out.push_str("    esac\n}\n");
    }
    out.push_str(
        r#"
# Completes the arguments of UTILITY ($1), whose name is COMP_WORDS[$2].
_dogfish_complete() {
    local spec=_dogfish_spec_${1//-/_} first=$2 cur=${COMP_WORDS[COMP_CWORD]}
    local prev= opt= prefix= i
    local _dogfish_opts _dogfish_args _dogfish_words _dogfish_kind
    declare -F "$spec" >/dev/null || return 0
    (( COMP_CWORD > first )) && prev=${COMP_WORDS[COMP_CWORD-1]}
    for (( i = first + 1; i < COMP_CWORD; i++ )); do
        [[ ${COMP_WORDS[i]} == -- ]] && return 0
    done
    "$spec" ""
    if [[ $cur == = ]] && (( COMP_CWORD - 1 > first )); then
        # --option=<TAB>, with = in COMP_WORDBREAKS (the default).
        opt=$prev cur=
    elif [[ $prev == = ]] && (( COMP_CWORD - 2 > first )); then
        opt=${COMP_WORDS[COMP_CWORD-2]}
    elif [[ $cur == --*=* ]]; then
        opt=${cur%%=*} prefix=${cur%%=*}= cur=${cur#*=}
    elif [[ -n $prev && $_dogfish_args == *" $prev "* ]]; then
        opt=$prev
    elif [[ $cur == -* ]]; then
        COMPREPLY=($(compgen -W "$_dogfish_opts" -- "$cur"))
        if [[ ${#COMPREPLY[@]} == 1 && ${COMPREPLY[0]} == *= ]] && type compopt >/dev/null 2>&1; then
            compopt -o nospace
        fi
        return 0
    else
        return 0
    fi
    "$spec" "$opt"
    case $_dogfish_kind in
        words) COMPREPLY=($(compgen -P "$prefix" -W "$_dogfish_words" -- "$cur")) ;;
        dirs) COMPREPLY=($(compgen -P "$prefix" -d -- "$cur")) ;;
        none) COMPREPLY=() ; type compopt >/dev/null 2>&1 && compopt +o default +o bashdefault ;;
    esac
    return 0
}

_dogfish() {
    if (( COMP_CWORD == 1 )); then
        COMPREPLY=($(compgen -W "$_dogfish_tools --help --version --list" -- "${COMP_WORDS[1]}"))
        return 0
    fi
    _dogfish_complete "${COMP_WORDS[1]#df-}" 1
}

_dogfish_link() {
    local name=${COMP_WORDS[0]##*/}
    _dogfish_complete "${name#df-}" 0
}

complete -o bashdefault -o default -F _dogfish dogfish gnu-dogfish

# df-UTILITY and UTILITY, wherever the first command of that name on $PATH is
# dogfish itself (shell builtins excepted).
_dogfish_links() {
    local self dir tool name seen=' ' IFS=:
    self=$(type -P dogfish) || return 0
    for dir in $PATH; do
        [[ -n $dir ]] || continue
        for tool in ${_dogfish_tools// /:}; do
            for name in "df-$tool" "$tool"; do
                case $name in @BUILTINS@) continue ;; esac
                [[ $seen == *" $name "* ]] && continue
                if [[ -x $dir/$name && ! -d $dir/$name ]]; then
                    seen+="$name "
                    if [[ $dir/$name -ef $self ]]; then
                        complete -o bashdefault -o default -F _dogfish_link "$name"
                    fi
                fi
            done
        done
    done
}
_dogfish_links
unset -f _dogfish_links
"#,
    );
    out.replace("@BUILTINS@", &BUILTINS.join("|"))
}

/// Escapes text for a zsh `_arguments` spec in single quotes.
fn zsh_quote(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '\'' => out.push_str("'\\''"),
            '[' | ']' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

/// The zsh completion (`share/zsh/site-functions/_dogfish`).
pub(crate) fn zsh(manuals: &[Manual]) -> String {
    let mut out = String::new();
    out.push_str(
        "#compdef dogfish gnu-dogfish -p df-*

# zsh completion for GNU/Dogfish: `dogfish UTILITY ...` and the df-UTILITY
# links. For links named UTILITY: compdef _dogfish ls cp ... (aliases made by
# `dogfish init` complete without it).
# Generated by `dogfish --generate zsh`; do not edit.

",
    );
    for m in manuals {
        let help = m.parsed_help();
        let _ = writeln!(
            out,
            "_dogfish_{}() {{\n  _arguments -s -S : \\",
            ident(m.tool)
        );
        for o in comp_opts(m, &help) {
            let short = !o.spelling.starts_with("--");
            let marker = match (o.arg, short) {
                (HasArg::No, _) => "",
                (HasArg::Required, true) => "+",
                (HasArg::Required, false) => "=",
                (HasArg::Optional, true) => "-",
                (HasArg::Optional, false) => "=-",
            };
            let action = match o.kind {
                ArgKind::Words => format!("({})", o.words.join(" ")),
                ArgKind::Files => "_files".into(),
                ArgKind::Dirs => "_files -/".into(),
                ArgKind::Nothing => " ".into(),
            };
            let arg = if o.arg == HasArg::No {
                String::new()
            } else {
                format!(":{}:{action}", o.arg_name)
            };
            let _ = writeln!(
                out,
                "    '{}{marker}[{}]{arg}' \\",
                o.spelling,
                zsh_quote(&o.desc)
            );
        }
        out.push_str("    '*:file:_files'\n}\n\n");
    }
    out.push_str("_dogfish() {\n  local tool\n  case $service in\n    (dogfish|gnu-dogfish)\n      if (( CURRENT == 2 )); then\n        if [[ $PREFIX == -* ]]; then\n          compadd -- --help --version --list\n          return\n        fi\n        local -a tools\n        tools=(\n");
    for m in manuals {
        let desc = crate::ALL_TOOLS
            .iter()
            .find(|(n, _, _)| *n == m.tool)
            .map_or("", |(_, _, d)| d);
        let _ = writeln!(
            out,
            "          '{}:{}'",
            m.tool,
            desc.replace('\'', "'\\''").replace(':', "\\:")
        );
    }
    out.push_str(
        "        )
        _describe -t commands 'dogfish utility' tools
        return
      fi
      tool=${words[2]#df-}
      shift words
      (( CURRENT-- ))
      ;;
    (*)
      tool=${${service##*/}#df-}
      ;;
  esac
  tool=_dogfish_${tool//-/_}
  if (( $+functions[$tool] )); then
    $tool
  else
    _files
  fi
}

if [[ $zsh_eval_context[-1] == loadautofunc ]]; then
  _dogfish \"$@\"
else
  compdef _dogfish dogfish gnu-dogfish -p 'df-*'
fi
",
    );
    out
}

/// Escapes text for fish in single quotes.
fn fish_quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

/// The fish completion (`share/fish/vendor_completions.d/dogfish.fish`).
pub(crate) fn fish(manuals: &[Manual]) -> String {
    let mut out = String::new();
    out.push_str(
        "# fish completion for GNU/Dogfish: `dogfish UTILITY ...`, the df-UTILITY
# links, and UTILITY where the command found on $PATH is dogfish itself.
# Generated by `dogfish --generate fish`; do not edit.

function __dogfish_tool --description 'The Dogfish utility being completed'
    set -l cmd (commandline -opc)
    set -q cmd[1]; or return 1
    set -l name (string replace -r '.*/' '' -- $cmd[1])
    if contains -- $name dogfish gnu-dogfish
        set -q cmd[2]; or return 1
        set name $cmd[2]
    end
    # string replace prints its input unchanged, and fails, without a match.
    string replace -r '^df-' '' -- $name; or true
end

function __dogfish_is --argument-names tool
    set -l t (__dogfish_tool); or return 1
    test \"$t\" = \"$tool\"
end

function __dogfish_needs_tool
    set -l cmd (commandline -opc)
    test (count $cmd) -eq 1; or return 1
    contains -- (string replace -r '.*/' '' -- $cmd[1]) dogfish gnu-dogfish
end

complete -c dogfish -n __dogfish_needs_tool -l help -d 'Print a summary of the utilities'
complete -c dogfish -n __dogfish_needs_tool -l version -d 'Print the version'
complete -c dogfish -n __dogfish_needs_tool -l list -d 'Print the name of each utility'
",
    );
    for m in manuals {
        let desc = crate::ALL_TOOLS
            .iter()
            .find(|(n, _, _)| *n == m.tool)
            .map_or("", |(_, _, d)| d);
        let _ = writeln!(
            out,
            "complete -c dogfish -f -n __dogfish_needs_tool -a {} -d '{}'",
            m.tool,
            fish_quote(desc)
        );
    }
    for m in manuals {
        let help = m.parsed_help();
        out.push('\n');
        for o in comp_opts(m, &help) {
            let mut line = format!("complete -c dogfish -n '__dogfish_is {}'", m.tool);
            match o.spelling.strip_prefix("--") {
                Some(long) => {
                    let _ = write!(line, " -l {long}");
                }
                None => {
                    let _ = write!(line, " -s {}", &o.spelling[1..]);
                }
            }
            match (o.arg, o.kind) {
                (HasArg::No, _) => {}
                (_, ArgKind::Words) => {
                    let _ = write!(line, " -x -a '{}'", o.words.join(" "));
                }
                (HasArg::Required, ArgKind::Files) => line.push_str(" -r -F"),
                (HasArg::Required, ArgKind::Dirs) => {
                    line.push_str(" -x -a '(__fish_complete_directories)'")
                }
                (HasArg::Required, ArgKind::Nothing) => line.push_str(" -x"),
                (HasArg::Optional, _) => {}
            }
            if !o.desc.is_empty() {
                let _ = write!(line, " -d '{}'", fish_quote(&o.desc));
            }
            out.push_str(&line);
            out.push('\n');
        }
    }
    let _ = write!(
        out,
        "
complete -c gnu-dogfish -w dogfish
set -l __dogfish_self (command -s dogfish)
and set __dogfish_self (realpath -- $__dogfish_self)
for tool in {}
    complete -c df-$tool -w \"dogfish $tool\"
    contains -- $tool {} ; and continue
    set -l found (command -s -- $tool); or continue
    test -n \"$__dogfish_self\" -a (realpath -- $found) = \"$__dogfish_self\"
    and complete -c $tool -w \"dogfish $tool\"
end
",
        tool_names(manuals),
        BUILTINS.join(" ")
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_are_one_short_clause() {
        assert_eq!(
            short_desc("with -l, scale sizes by SIZE; see below. More"),
            "with -l, scale sizes by SIZE"
        );
        assert_eq!(short_desc("Sort descending [alias: -r]"), "Sort descending");
        assert_eq!(
            short_desc("do not ignore entries starting with ."),
            "do not ignore entries starting with ."
        );
        assert_eq!(
            short_desc("do not list implied . and .."),
            "do not list implied . and .."
        );
        assert_eq!(
            short_desc("set width to COLS.  0 means none"),
            "set width to COLS"
        );
        let long = "word ".repeat(30);
        assert!(short_desc(&long).len() <= 80);
        assert!(short_desc(&long).ends_with("..."));
    }

    #[test]
    fn argument_kinds_follow_their_names() {
        assert_eq!(arg_kind(Some("FILE")), ArgKind::Files);
        assert_eq!(arg_kind(Some("DIRECTORY")), ArgKind::Dirs);
        assert_eq!(arg_kind(Some("SIZE")), ArgKind::Nothing);
        assert_eq!(arg_kind(None), ArgKind::Files);
    }

    #[test]
    fn scripts_cover_every_option_of_every_tool() {
        let all = super::super::all();
        let (b, z, f) = (bash(&all), zsh(&all), fish(&all));
        for m in &all {
            let id = ident(m.tool);
            assert!(
                b.contains(&format!("_dogfish_spec_{id}() {{")),
                "{}",
                m.tool
            );
            assert!(z.contains(&format!("_dogfish_{id}() {{")), "{}", m.tool);
            let help = m.parsed_help();
            for o in &m.opts {
                if matches!(m.doc(&help, o), Doc::Hidden) {
                    continue;
                }
                for s in o.spellings() {
                    let bash_word = [format!(" {s} "), format!("'{s} "), format!(" {s}'")];
                    assert!(
                        bash_word.iter().any(|w| b.contains(w)),
                        "bash {} {s}",
                        m.tool
                    );
                    assert!(z.contains(&format!("'{s}")), "zsh {} {s}", m.tool);
                    let fish_opt = match s.strip_prefix("--") {
                        Some(l) => format!(" -l {l}"),
                        None => format!(" -s {}", &s[1..]),
                    };
                    assert!(
                        f.contains(&format!("'__dogfish_is {}'{fish_opt}", m.tool)),
                        "fish {} {s}",
                        m.tool
                    );
                }
            }
        }
    }

    #[test]
    fn value_choices_are_offered() {
        let all = super::super::all();
        let b = bash(&all);
        assert!(b.contains("'--color'|'--hyperlink'|'--classify') _dogfish_kind=words _dogfish_words='always yes force never no none auto tty if-tty' ;;")
            || b.contains("_dogfish_words='always yes force never no none auto tty if-tty'"));
        let z = zsh(&all);
        assert!(z.contains("'--sort=["), "{z}");
        let f = fish(&all);
        assert!(f.contains("-l quoting-style -x -a 'literal"));
    }
}
