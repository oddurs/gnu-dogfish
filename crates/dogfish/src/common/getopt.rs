//! Shared GNU command-line plumbing for the shell and system tools: usage
//! errors, `argmatch`, `--help`, and a `getopt_long`-style parser
//! (date, env, seq, timeout).

use std::io::{self, Write};

use anyhow::Result;
use dogfish_core::{gnu_quote, io::ignore_broken_pipe};

/// Prints a GNU usage diagnostic (`tool: msg` plus the `--help` hint) and
/// returns `status` for the caller to exit with.
pub(crate) fn usage_error(tool: &str, msg: impl std::fmt::Display, status: i32) -> i32 {
    eprintln!("{tool}: {msg}\nTry '{tool} --help' for more information.");
    status
}

/// Prints the GNU "Try 'tool --help'" hint alone, as GNU does for some
/// operand-count errors, and returns `status`.
pub(crate) fn try_help(tool: &str, status: i32) -> i32 {
    eprintln!("Try '{tool} --help' for more information.");
    status
}

/// Resolves `value` against `valid` like gnulib `argmatch`: an exact match or a
/// unique prefix. On failure prints GNU's diagnostic (with the list of valid
/// arguments) and returns the usage `status` as the error.
pub(crate) fn argmatch(
    tool: &str,
    option: &str,
    value: &str,
    valid: &[&str],
    status: i32,
) -> std::result::Result<usize, i32> {
    if let Some(i) = valid.iter().position(|v| *v == value) {
        return Ok(i);
    }
    let matches: Vec<usize> = (0..valid.len())
        .filter(|&i| !value.is_empty() && valid[i].starts_with(value))
        .collect();
    if let [only] = matches.as_slice() {
        return Ok(*only);
    }
    let kind = if matches.is_empty() {
        "invalid"
    } else {
        "ambiguous"
    };
    let mut msg = format!(
        "{kind} argument {} for '{option}'\nValid arguments are:",
        gnu_quote(value)
    );
    for v in valid {
        msg.push_str(&format!("\n  - '{v}'"));
    }
    Err(usage_error(tool, msg, status))
}

/// Whether an option takes an argument, as in `getopt_long`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HasArg {
    No,
    Required,
    /// Only attached: `-xVALUE` or `--name=VALUE`.
    Optional,
}

/// One option a [`GetOpt`] parser accepts; `key` identifies it to the caller.
pub(crate) struct OptSpec {
    pub(crate) short: Option<char>,
    pub(crate) long: Option<&'static str>,
    pub(crate) arg: HasArg,
    pub(crate) key: &'static str,
}

pub(crate) const fn opt(
    short: Option<char>,
    long: Option<&'static str>,
    arg: HasArg,
    key: &'static str,
) -> OptSpec {
    OptSpec {
        short,
        long,
        arg,
        key,
    }
}

/// A small `getopt_long` work-alike for the tools whose GNU parsing clap cannot
/// express: options that stop at the first operand (`env`, `timeout`, `seq`),
/// argument vectors rewritten mid-parse (`env -S`), and option arguments that
/// are only ever attached (`date -I`). Diagnostics use glibc's exact wording.
pub(crate) struct GetOpt {
    pub(crate) args: Vec<String>,
    pub(crate) idx: usize,
    /// Byte offset of the next short option inside `args[idx]`, or 0.
    cluster: usize,
    /// Accept options after operands (GNU's default permuting mode).
    permute: bool,
    pub(crate) operands: Vec<String>,
    done: bool,
    specs: &'static [OptSpec],
}

type OptResult = std::result::Result<(&'static str, Option<String>), String>;

impl GetOpt {
    /// `args` excludes the program name.
    pub(crate) fn new(args: Vec<String>, specs: &'static [OptSpec], permute: bool) -> Self {
        Self {
            args,
            idx: 0,
            cluster: 0,
            permute,
            operands: Vec::new(),
            done: false,
            specs,
        }
    }

    /// The argument the parser would examine next, when not inside a cluster.
    pub(crate) fn peek(&self) -> Option<&str> {
        if self.done || self.cluster != 0 {
            return None;
        }
        self.args.get(self.idx).map(String::as_str)
    }

    /// Replaces the arguments consumed so far with `items` and resumes parsing
    /// at the first of them (how GNU `env -S` re-injects its split string).
    pub(crate) fn splice_front(&mut self, items: Vec<String>) {
        let rest = self.args.split_off(self.idx);
        self.args = items;
        self.args.extend(rest);
        self.idx = 0;
        self.cluster = 0;
    }

    /// Operands collected so far plus every argument not yet parsed.
    pub(crate) fn into_operands(mut self) -> Vec<String> {
        let rest = self.args.split_off(self.idx.min(self.args.len()));
        self.operands.extend(rest);
        self.operands
    }

    pub(crate) fn next_opt(&mut self) -> Option<OptResult> {
        if self.cluster != 0 {
            return Some(self.short_opt());
        }
        loop {
            if self.done {
                return None;
            }
            let arg = self.args.get(self.idx)?;
            if arg == "--" {
                self.idx += 1;
                self.done = true;
                return None;
            }
            if arg.len() > 2 && arg.starts_with("--") {
                return Some(self.long_opt());
            }
            if arg.len() > 1 && arg.starts_with('-') {
                self.cluster = 1;
                return Some(self.short_opt());
            }
            if !self.permute {
                self.done = true;
                return None;
            }
            self.operands.push(arg.clone());
            self.idx += 1;
        }
    }

    fn long_opt(&mut self) -> OptResult {
        let arg = self.args[self.idx].clone();
        self.idx += 1;
        let body = &arg[2..];
        let (name, attached) = match body.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (body, None),
        };
        let spec = match self.specs.iter().find(|s| s.long == Some(name)) {
            Some(s) => s,
            None => {
                let candidates: Vec<&OptSpec> = self
                    .specs
                    .iter()
                    .filter(|s| s.long.is_some_and(|l| l.starts_with(name)))
                    .collect();
                match candidates.as_slice() {
                    [] => return Err(format!("unrecognized option '{arg}'")),
                    [first, rest @ ..]
                        if rest
                            .iter()
                            .all(|s| s.key == first.key && s.arg == first.arg) =>
                    {
                        *first
                    }
                    many => {
                        let names: Vec<String> = many
                            .iter()
                            .map(|s| format!("'--{}'", s.long.unwrap_or_default()))
                            .collect();
                        return Err(format!(
                            "option '--{name}' is ambiguous; possibilities: {}",
                            names.join(" ")
                        ));
                    }
                }
            }
        };
        let full = spec.long.unwrap_or_default();
        match (spec.arg, attached) {
            (HasArg::No, Some(_)) => Err(format!("option '--{full}' doesn't allow an argument")),
            (HasArg::No, None) | (HasArg::Optional, None) => Ok((spec.key, None)),
            (_, Some(v)) => Ok((spec.key, Some(v))),
            (HasArg::Required, None) => match self.args.get(self.idx) {
                Some(v) => {
                    let v = v.clone();
                    self.idx += 1;
                    Ok((spec.key, Some(v)))
                }
                None => Err(format!("option '--{full}' requires an argument")),
            },
        }
    }

    fn short_opt(&mut self) -> OptResult {
        let arg = self.args[self.idx].clone();
        let c = arg[self.cluster..].chars().next().unwrap_or('?');
        self.cluster += c.len_utf8();
        let rest = arg[self.cluster..].to_string();
        let at_end = rest.is_empty();
        let finish_cluster = |me: &mut Self| {
            me.cluster = 0;
            me.idx += 1;
        };
        let Some(spec) = self.specs.iter().find(|s| s.short == Some(c)) else {
            if at_end {
                finish_cluster(self);
            }
            return Err(format!("invalid option -- '{c}'"));
        };
        match spec.arg {
            HasArg::No => {
                if at_end {
                    finish_cluster(self);
                }
                Ok((spec.key, None))
            }
            HasArg::Optional => {
                finish_cluster(self);
                Ok((spec.key, (!at_end).then_some(rest)))
            }
            HasArg::Required => {
                finish_cluster(self);
                if !at_end {
                    return Ok((spec.key, Some(rest)));
                }
                match self.args.get(self.idx) {
                    Some(v) => {
                        let v = v.clone();
                        self.idx += 1;
                        Ok((spec.key, Some(v)))
                    }
                    None => Err(format!("option requires an argument -- '{c}'")),
                }
            }
        }
    }
}

pub(crate) fn print_help(text: &str) -> Result<i32> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    ignore_broken_pipe(out.write_all(text.as_bytes()))?;
    Ok(0)
}
