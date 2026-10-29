//! `getopt_long`-style option parsing for stat, df, du and ls.

use std::io::{self, Write};

use anyhow::Result;
use dogfish_core::io::ignore_broken_pipe;

use crate::common::quote::{Quoting, quote_bytes};

/// Whether a GNU option takes an argument.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum OptArg {
    No,
    Required,
    /// Long form only, and only as `--name=VALUE`.
    Optional,
}

/// One accepted option: `key` identifies it to the tool, `short`/`long` are its spellings.
pub(crate) struct OptSpec {
    pub(crate) key: &'static str,
    pub(crate) short: Option<char>,
    pub(crate) long: Option<&'static str>,
    pub(crate) arg: OptArg,
}

impl OptSpec {
    pub(crate) const fn new(
        key: &'static str,
        short: Option<char>,
        long: Option<&'static str>,
        arg: OptArg,
    ) -> Self {
        Self {
            key,
            short,
            long,
            arg,
        }
    }
}

/// One option occurrence, in command-line order.
pub(crate) struct GnuOpt {
    pub(crate) key: &'static str,
    pub(crate) value: Option<String>,
    /// How GNU names the option in diagnostics: `-B` or `--block-size`.
    pub(crate) spelled: String,
}

pub(crate) struct GnuArgs {
    pub(crate) opts: Vec<GnuOpt>,
    pub(crate) operands: Vec<String>,
}

/// Parses `args` (without `argv[0]`) the way glibc's `getopt_long` does: short
/// option clusters, attached or separate option arguments, unambiguous long
/// option prefixes, `--` and argument permutation (unless `POSIXLY_CORRECT`).
/// The error is the diagnostic glibc would print after `tool: `.
pub(crate) fn gnu_getopt(specs: &[OptSpec], args: &[String]) -> Result<GnuArgs, String> {
    let posixly_correct = std::env::var_os("POSIXLY_CORRECT").is_some();
    let mut opts = Vec::new();
    let mut operands = Vec::new();
    let mut only_operands = false;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        if only_operands || arg == "-" || !arg.starts_with('-') {
            operands.push(arg.clone());
            only_operands |= posixly_correct;
            continue;
        }
        if arg == "--" {
            only_operands = true;
            continue;
        }
        if let Some(body) = arg.strip_prefix("--") {
            let (name, inline) = match body.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (body, None),
            };
            let spec = match specs.iter().find(|s| s.long == Some(name)) {
                Some(s) => s,
                None => {
                    let candidates: Vec<&OptSpec> = specs
                        .iter()
                        .filter(|s| s.long.is_some_and(|l| l.starts_with(name)))
                        .collect();
                    match candidates.as_slice() {
                        [] => return Err(format!("unrecognized option '{arg}'")),
                        [first, rest @ ..] if rest.iter().all(|s| s.key == first.key) => *first,
                        many => {
                            let mut msg = format!("option '{arg}' is ambiguous; possibilities:");
                            for s in many {
                                msg.push_str(&format!(" '--{}'", s.long.unwrap_or_default()));
                            }
                            return Err(msg);
                        }
                    }
                }
            };
            let long = spec.long.unwrap_or_default();
            let value = match spec.arg {
                OptArg::No => {
                    if inline.is_some() {
                        return Err(format!("option '--{long}' doesn't allow an argument"));
                    }
                    None
                }
                OptArg::Optional => inline.map(str::to_string),
                OptArg::Required => match inline {
                    Some(v) => Some(v.to_string()),
                    None if i < args.len() => {
                        i += 1;
                        Some(args[i - 1].clone())
                    }
                    None => return Err(format!("option '--{long}' requires an argument")),
                },
            };
            opts.push(GnuOpt {
                key: spec.key,
                value,
                spelled: format!("--{long}"),
            });
            continue;
        }
        let cluster = &arg[1..];
        for (pos, c) in cluster.char_indices() {
            let Some(spec) = specs.iter().find(|s| s.short == Some(c)) else {
                return Err(format!("invalid option -- '{c}'"));
            };
            let spelled = format!("-{c}");
            // Optional arguments exist only as `--name=VALUE`.
            if spec.arg != OptArg::Required {
                opts.push(GnuOpt {
                    key: spec.key,
                    value: None,
                    spelled,
                });
                continue;
            }
            let rest = &cluster[pos + c.len_utf8()..];
            let value = if !rest.is_empty() {
                rest.to_string()
            } else if i < args.len() {
                i += 1;
                args[i - 1].clone()
            } else {
                return Err(format!("option requires an argument -- '{c}'"));
            };
            opts.push(GnuOpt {
                key: spec.key,
                value: Some(value),
                spelled,
            });
            break;
        }
    }
    Ok(GnuArgs { opts, operands })
}

/// Prints a GNU usage diagnostic and returns the exit status to use.
pub(crate) fn gnu_usage_error(tool: &str, msg: &str, status: i32) -> i32 {
    eprintln!("{tool}: {msg}\nTry '{tool} --help' for more information.");
    status
}

pub(crate) fn print_tool_version(tool: &str) {
    println!("{} (GNU/Dogfish) {}", tool, env!("CARGO_PKG_VERSION"));
}

/// Writes `--help` text, tolerating a closed pipe.
pub(crate) fn print_help_text(text: &str) -> Result<i32> {
    ignore_broken_pipe(io::stdout().lock().write_all(text.as_bytes()))?;
    Ok(0)
}

/// Prints GNU's "Try 'TOOL --help'" hint and returns `status`.
pub(crate) fn try_help(tool: &str, status: i32) -> i32 {
    eprintln!("Try '{tool} --help' for more information.");
    status
}

/// Resolves `--opt=WORD` like GNU `argmatch`: an exact match or a prefix that
/// selects one meaning. `choices` pairs each accepted word with its meaning;
/// synonyms are adjacent. On failure, prints GNU's "invalid/ambiguous argument"
/// block (the caller adds the `--help` hint).
pub(crate) fn gnu_argmatch<'a>(
    tool: &str,
    what: &str,
    value: &str,
    choices: &[(&str, &'a str)],
) -> Option<&'a str> {
    if let Some((_, meaning)) = choices.iter().find(|(word, _)| *word == value) {
        return Some(meaning);
    }
    let matches: Vec<&'a str> = choices
        .iter()
        .filter(|(word, _)| word.starts_with(value))
        .map(|(_, meaning)| *meaning)
        .collect();
    if let Some(first) = matches.first()
        && matches.iter().all(|m| m == first)
    {
        return Some(first);
    }
    let kind = if matches.is_empty() {
        "invalid"
    } else {
        "ambiguous"
    };
    let mut msg = format!(
        "{tool}: {kind} argument {} for {}\nValid arguments are:",
        quote_bytes(value.as_bytes(), Quoting::Locale),
        quote_bytes(what.as_bytes(), Quoting::Locale)
    );
    let mut previous: Option<&str> = None;
    for (word, meaning) in choices {
        let quoted = quote_bytes(word.as_bytes(), Quoting::Locale);
        if previous == Some(*meaning) {
            msg.push_str(&format!(", {quoted}"));
        } else {
            msg.push_str(&format!("\n  - {quoted}"));
        }
        previous = Some(meaning);
    }
    eprintln!("{msg}");
    None
}
