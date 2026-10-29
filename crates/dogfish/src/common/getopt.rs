//! GNU command-line handling shared by every tool: glibc's `getopt_long`
//! ([`Getopt`]), gnulib's `argmatch` ([`argmatch`]), usage diagnostics with the
//! `Try 'TOOL --help'` hint and each tool's usage exit status
//! ([`usage_error`], [`exit_failure`]), and `--help`/`--version` output.
//!
//! Arguments are `OsString`s: a command line need not be UTF-8, and option
//! arguments and operands reach the tool byte for byte.
//!
//! Dogfish's own options never change what GNU accepts or how it words a
//! diagnostic: a tool's extras are [`OptSpec::exact`] (never abbreviated, never
//! listed as an ambiguity), and the switches every tool may take (`--stats`,
//! `--no-color`, `--color`) are [`Switch`]es, granted per tool by
//! [`switches_for`] and matched only when spelled in full.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::sync::{Arc, OnceLock};

use dogfish_core::{ColorMode, Switches};

use crate::common::quote::quote_locale;

/// Whether an option takes an argument (`has_arg` in `struct option`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HasArg {
    No,
    /// `-xVALUE`, `-x VALUE`, `--name=VALUE` or `--name VALUE`.
    Required,
    /// Only attached: `-xVALUE` or `--name=VALUE`.
    Optional,
}

/// One accepted option: a short option character, a long name, or both.
/// `key` identifies it to the tool; long names sharing a key, `arg` and short
/// option are synonyms (their abbreviations are never ambiguous with each
/// other).
pub(crate) struct OptSpec<K> {
    pub(crate) short: Option<u8>,
    pub(crate) long: Option<&'static str>,
    pub(crate) arg: HasArg,
    pub(crate) key: K,
    /// A Dogfish extension: its long name matches only in full, so GNU's
    /// abbreviations and ambiguity diagnostics are unchanged.
    pub(crate) exact: bool,
}

impl<K> OptSpec<K> {
    /// An option with a short and a long spelling (`-n`, `--number`).
    pub(crate) const fn both(short: u8, long: &'static str, arg: HasArg, key: K) -> Self {
        Self {
            short: Some(short),
            long: Some(long),
            arg,
            key,
            exact: false,
        }
    }

    /// A short-only option (`-e`).
    pub(crate) const fn short(short: u8, arg: HasArg, key: K) -> Self {
        Self {
            short: Some(short),
            long: None,
            arg,
            key,
            exact: false,
        }
    }

    /// A long-only option (`--help`).
    pub(crate) const fn long(long: &'static str, arg: HasArg, key: K) -> Self {
        Self {
            short: None,
            long: Some(long),
            arg,
            key,
            exact: false,
        }
    }
}

impl<K: Copy> OptSpec<K> {
    /// Marks a Dogfish extension (`--json`, `--tree`): GNU has no such
    /// option, so its long name is never matched by a prefix.
    pub(crate) const fn exact(self) -> Self {
        Self {
            exact: true,
            ..self
        }
    }
}

/// The Dogfish switches a tool accepts on top of its own options.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Switch(u8);

impl Switch {
    pub(crate) const NONE: Self = Self(0);
    /// `--stats`: the telemetry footer on stderr.
    pub(crate) const STATS: Self = Self(1);
    /// `--no-color`: plain output on a terminal.
    pub(crate) const NO_COLOR: Self = Self(2);
    /// `--color[=WHEN]`, for tools with rich terminal output and no GNU
    /// `--color` of their own.
    pub(crate) const COLOR: Self = Self(4);

    pub(crate) const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub(crate) const fn has(self, other: Self) -> bool {
        self.0 & other.0 == other.0 && other.0 != 0
    }
}

/// Which Dogfish switches `tool` accepts. None for the tools whose GNU
/// versions take every argument as an operand or ignore them all (`echo`,
/// `printf`, `true`, `false`, `yes`): scripts rely on those. `ls` has GNU's
/// own `--color`; the rest of the tools with rich terminal output take
/// `--color[=WHEN]` as well.
pub(crate) fn switches_for(tool: &str) -> Switch {
    let common = Switch::STATS.with(Switch::NO_COLOR);
    match tool {
        "echo" | "printf" | "true" | "false" | "yes" => Switch::NONE,
        "bench" | "b2sum" | "md5sum" | "sha256sum" | "cut" | "df" | "du" | "env" | "head"
        | "history" | "id" | "install-links" | "nl" | "query" | "filter" | "realpath" | "rm"
        | "shuf" | "sort" | "stat" | "tail" | "undo" | "uniq" | "watch" | "wc" => {
            common.with(Switch::COLOR)
        }
        _ => common,
    }
}

/// The switches the running tool accepts, and where they are recorded.
static SWITCHES: OnceLock<(Switch, Arc<Switches>)> = OnceLock::new();

/// Lets the running tool's parsers accept `allowed`, recording what they see
/// in `into` (the tool's context). Called once, by `main`; without it (in
/// unit tests) a parser accepts only the options it is given.
pub(crate) fn install_switches(allowed: Switch, into: Arc<Switches>) {
    let _ = SWITCHES.set((allowed, into));
}

/// How options and operands may be mixed (the first character of a getopt
/// option string).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Order {
    /// Options may follow operands (GNU's default); `POSIXLY_CORRECT` in the
    /// environment turns this into [`Order::StopAtOperand`].
    Permute,
    /// Options end at the first operand (`+`), as for tools that run a COMMAND.
    StopAtOperand,
    /// Operands are returned in place, between options (`-`), as for `sort`.
    InPlace,
}

/// A getopt diagnostic, without the `tool: ` prefix. It is bytes, not text:
/// glibc echoes the argument as typed, whatever its encoding.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct GetoptError(Vec<u8>);

impl GetoptError {
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Display for GetoptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.0))
    }
}

impl fmt::Debug for GetoptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GetoptError({self})")
    }
}

/// One option as text, for tools whose option handling works on `str`: the
/// argument converted with [`lossy`], and the option as GNU names it in
/// diagnostics ([`Getopt::last_spelling`]).
pub(crate) struct TextOpt<K> {
    pub(crate) key: K,
    pub(crate) value: Option<String>,
    pub(crate) spelled: String,
}

/// One step of a [`Order::InPlace`] parse.
pub(crate) enum Arg<K> {
    Opt(K, Option<OsString>),
    Operand(OsString),
}

/// glibc's `getopt_long`, step by step: each call to [`Getopt::next`]
/// returns the next option in command-line order with its argument, so a
/// tool handles options (and `--help`, and errors) in the order GNU does and
/// lets a repeated option override an earlier one. Operands are collected
/// on the side ([`Getopt::operands`]).
///
/// The rules are glibc's: short options cluster (`-abc`, `-n5`); a required
/// argument may be attached or the next word, an optional one only attached;
/// a long option may be abbreviated to any unique prefix, an exact name wins
/// over longer ones, and two prefixes of options that differ give glibc's
/// ambiguity diagnostic; `--` ends the options and `-` is an operand.
pub(crate) struct Getopt<'a, K> {
    args: Vec<OsString>,
    /// Index of the argument being examined.
    idx: usize,
    /// Offset of the next short option inside `args[idx]`, or 0.
    cluster: usize,
    order: Order,
    specs: &'a [OptSpec<K>],
    operands: Vec<OsString>,
    /// No more options: everything from `idx` on is an operand.
    done: bool,
    /// Index into `specs` of the option last returned, and whether it was
    /// spelled long.
    last: Option<(usize, bool)>,
    /// Index into `args` of the word that option came from.
    last_word: Option<usize>,
    /// The Dogfish switches accepted, and where they are recorded.
    switches: Option<(Switch, Arc<Switches>)>,
}

impl<'a, K: Copy + PartialEq> Getopt<'a, K> {
    /// A parser for `args` (without the program name) in GNU's default
    /// permuting order.
    pub(crate) fn new(args: Vec<OsString>, specs: &'a [OptSpec<K>]) -> Self {
        Self::with_order(args, specs, Order::Permute)
    }

    pub(crate) fn with_order(args: Vec<OsString>, specs: &'a [OptSpec<K>], order: Order) -> Self {
        let order = match order {
            Order::Permute if std::env::var_os("POSIXLY_CORRECT").is_some() => Order::StopAtOperand,
            o => o,
        };
        Self {
            args,
            idx: 0,
            cluster: 0,
            order,
            specs,
            operands: Vec::new(),
            done: false,
            last: None,
            last_word: None,
            switches: SWITCHES.get().cloned(),
        }
    }

    /// Accepts the Dogfish switches `allowed`, recording them in `into`,
    /// instead of those installed for the running tool.
    #[cfg(test)]
    pub(crate) fn with_switches(mut self, allowed: Switch, into: Arc<Switches>) -> Self {
        self.switches = Some((allowed, into));
        self
    }

    /// The next option and its argument, `None` once options are exhausted,
    /// or the diagnostic for an invalid one.
    #[allow(clippy::should_implement_trait)]
    pub(crate) fn next(&mut self) -> Option<Result<(K, Option<OsString>), GetoptError>> {
        loop {
            match self.step()? {
                Ok(Arg::Opt(k, v)) => return Some(Ok((k, v))),
                Ok(Arg::Operand(o)) => self.operands.push(o),
                Err(e) => return Some(Err(e)),
            }
        }
    }

    /// Like [`Getopt::next`], with the option as a [`TextOpt`].
    pub(crate) fn next_text(&mut self) -> Option<Result<TextOpt<K>, GetoptError>> {
        Some(self.next()?.map(|(key, value)| TextOpt {
            key,
            value: value.as_deref().map(lossy),
            spelled: self.last_spelling(),
        }))
    }

    /// Like [`Getopt::next`], but operands come back in place
    /// ([`Order::InPlace`] parsers).
    pub(crate) fn next_in_order(&mut self) -> Option<Result<Arg<K>, GetoptError>> {
        self.step()
    }

    /// The operands: those skipped so far plus every argument not yet parsed.
    pub(crate) fn operands(mut self) -> Vec<OsString> {
        let start = (self.idx + usize::from(self.cluster != 0)).min(self.args.len());
        self.operands.extend(self.args.drain(start..));
        self.operands
    }

    /// The argument the parser would examine next, when not inside a cluster.
    pub(crate) fn peek(&self) -> Option<&OsStr> {
        if self.done || self.cluster != 0 {
            return None;
        }
        self.args.get(self.idx).map(OsString::as_os_str)
    }

    /// Takes the argument [`Getopt::peek`] shows, as an operand's own
    /// argument (`sort +1 -3`).
    pub(crate) fn take_next(&mut self) -> Option<OsString> {
        let arg = self.peek()?.to_os_string();
        self.idx += 1;
        Some(arg)
    }

    /// Replaces the arguments parsed so far with `items` and resumes at the
    /// first of them (how GNU `env -S` re-injects its split string).
    pub(crate) fn splice_front(&mut self, items: Vec<OsString>) {
        let rest = self.args.split_off(self.idx.min(self.args.len()));
        self.args = items;
        self.args.extend(rest);
        self.idx = 0;
        self.cluster = 0;
        self.last_word = None;
    }

    /// How GNU names the option last returned in diagnostics: its full long
    /// name (`--block-size`) when given long, else `-B`.
    pub(crate) fn last_spelling(&self) -> String {
        match self.last {
            Some((i, true)) => format!("--{}", self.specs[i].long.unwrap_or_default()),
            Some((i, false)) => format!("-{}", self.specs[i].short.map_or('?', char::from)),
            None => String::new(),
        }
    }

    /// The command-line word the option last returned was taken from
    /// (`--no-pres` for `--no-preserve-root`), for tools that refuse an
    /// abbreviation.
    pub(crate) fn last_word(&self) -> Option<&OsStr> {
        self.last_word
            .and_then(|i| self.args.get(i))
            .map(OsString::as_os_str)
    }

    fn step(&mut self) -> Option<Result<Arg<K>, GetoptError>> {
        if self.cluster != 0 {
            return Some(self.short_opt());
        }
        if self.done {
            return None;
        }
        let arg = self.args.get(self.idx)?.as_bytes();
        if arg == b"--" {
            self.idx += 1;
            self.done = true;
            return None;
        }
        if arg.len() < 2 || arg[0] != b'-' {
            match self.order {
                Order::StopAtOperand => {
                    self.done = true;
                    return None;
                }
                _ => {
                    let operand = self.args[self.idx].clone();
                    self.idx += 1;
                    return Some(Ok(Arg::Operand(operand)));
                }
            }
        }
        if arg[1] == b'-' {
            match self.switch() {
                Ok(true) => return self.step(),
                Ok(false) => {}
                Err(e) => return Some(Err(e)),
            }
            return Some(self.long_opt());
        }
        self.cluster = 1;
        Some(self.short_opt())
    }

    /// Handles `args[idx]` if it is a Dogfish switch the tool accepts, spelled
    /// in full and not shadowed by one of the tool's own options.
    fn switch(&mut self) -> Result<bool, GetoptError> {
        let Some((allowed, into)) = &self.switches else {
            return Ok(false);
        };
        let body = &self.args[self.idx].as_bytes()[2..];
        let (name, when) = match body.iter().position(|&b| b == b'=') {
            Some(eq) => (&body[..eq], Some(&body[eq + 1..])),
            None => (body, None),
        };
        if self
            .specs
            .iter()
            .any(|s| s.long.is_some_and(|l| l.as_bytes() == name))
        {
            return Ok(false);
        }
        match (name, when) {
            (b"stats", None) if allowed.has(Switch::STATS) => into.set_stats(true),
            (b"no-color", None) if allowed.has(Switch::NO_COLOR) => {
                into.set_color(ColorMode::Never)
            }
            (b"color", when) if allowed.has(Switch::COLOR) => {
                let mode = match when {
                    None => ColorMode::Always,
                    Some(w) => std::str::from_utf8(w)
                        .ok()
                        .filter(|w| !w.is_empty())
                        .and_then(ColorMode::from_when)
                        .ok_or_else(|| {
                            let mut m = b"invalid argument ".to_vec();
                            m.extend_from_slice(quote_locale(OsStr::from_bytes(w)).as_bytes());
                            m.extend_from_slice(b" for ");
                            m.extend_from_slice(quote_locale(OsStr::new("--color")).as_bytes());
                            m.extend_from_slice(
                                b"\nValid arguments are:\n  \
                                  - 'always', 'yes', 'force'\n  \
                                  - 'never', 'no', 'none'\n  \
                                  - 'auto', 'tty', 'if-tty'",
                            );
                            GetoptError(m)
                        })?,
                };
                into.set_color(mode);
            }
            _ => return Ok(false),
        }
        self.idx += 1;
        Ok(true)
    }

    fn long_opt(&mut self) -> Result<Arg<K>, GetoptError> {
        let word = self.idx;
        let arg = self.args[self.idx].as_bytes();
        self.idx += 1;
        let body = &arg[2..];
        let (name, attached) = match body.iter().position(|&b| b == b'=') {
            Some(eq) => (&body[..eq], Some(&body[eq + 1..])),
            None => (body, None),
        };
        let typed = |msg: &str| {
            let mut m = msg.as_bytes().to_vec();
            m.extend_from_slice(b" '--");
            m.extend_from_slice(body);
            m.push(b'\'');
            m
        };
        let longs = || {
            self.specs
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.long.map(|l| (i, l)))
        };
        let found = match longs().find(|(_, l)| l.as_bytes() == name) {
            Some((i, _)) => i,
            None => {
                let mut first: Option<usize> = None;
                let mut ambiguous: Vec<usize> = Vec::new();
                for (i, l) in longs() {
                    if self.specs[i].exact || !l.as_bytes().starts_with(name) {
                        continue;
                    }
                    match first {
                        None => first = Some(i),
                        Some(f) => {
                            let (a, b) = (&self.specs[f], &self.specs[i]);
                            // glibc compares `val`, which is the short option when
                            // there is one: same key, argument and letter is a synonym.
                            if a.key != b.key || a.arg != b.arg || a.short != b.short {
                                if ambiguous.is_empty() {
                                    ambiguous.push(f);
                                }
                                ambiguous.push(i);
                            }
                        }
                    }
                }
                if !ambiguous.is_empty() {
                    let mut msg = typed("option");
                    msg.extend_from_slice(b" is ambiguous; possibilities:");
                    for i in ambiguous {
                        msg.extend_from_slice(b" '--");
                        msg.extend_from_slice(self.specs[i].long.unwrap_or_default().as_bytes());
                        msg.push(b'\'');
                    }
                    return Err(GetoptError(msg));
                }
                match first {
                    Some(i) => i,
                    None => return Err(GetoptError(typed("unrecognized option"))),
                }
            }
        };
        let spec = &self.specs[found];
        let full = spec.long.unwrap_or_default();
        let value = match (spec.arg, attached) {
            (HasArg::No, Some(_)) => {
                return Err(GetoptError(
                    format!("option '--{full}' doesn't allow an argument").into_bytes(),
                ));
            }
            (_, Some(v)) => Some(OsString::from_vec(v.to_vec())),
            (HasArg::No | HasArg::Optional, None) => None,
            (HasArg::Required, None) => match self.args.get(self.idx) {
                Some(v) => {
                    let v = v.clone();
                    self.idx += 1;
                    Some(v)
                }
                None => {
                    return Err(GetoptError(
                        format!("option '--{full}' requires an argument").into_bytes(),
                    ));
                }
            },
        };
        self.last = Some((found, true));
        self.last_word = Some(word);
        Ok(Arg::Opt(spec.key, value))
    }

    fn short_opt(&mut self) -> Result<Arg<K>, GetoptError> {
        let word = self.idx;
        let arg = self.args[self.idx].as_bytes();
        let c = arg[self.cluster];
        self.cluster += 1;
        let rest = &arg[self.cluster..];
        let at_end = rest.is_empty();
        let rest = OsString::from_vec(rest.to_vec());
        let finish = |me: &mut Self| {
            me.cluster = 0;
            me.idx += 1;
        };
        let found = self
            .specs
            .iter()
            .position(|s| s.short == Some(c))
            .filter(|_| c != b':' && c != b';');
        let Some(found) = found else {
            if at_end {
                finish(self);
            }
            let mut msg = b"invalid option -- '".to_vec();
            msg.push(c);
            msg.push(b'\'');
            return Err(GetoptError(msg));
        };
        let spec = &self.specs[found];
        let value = match spec.arg {
            HasArg::No => {
                if at_end {
                    finish(self);
                }
                None
            }
            HasArg::Optional => {
                finish(self);
                (!at_end).then_some(rest)
            }
            HasArg::Required => {
                finish(self);
                if !at_end {
                    Some(rest)
                } else {
                    match self.args.get(self.idx) {
                        Some(v) => {
                            let v = v.clone();
                            self.idx += 1;
                            Some(v)
                        }
                        None => {
                            let mut msg = b"option requires an argument -- '".to_vec();
                            msg.push(c);
                            msg.push(b'\'');
                            return Err(GetoptError(msg));
                        }
                    }
                }
            }
        };
        self.last = Some((found, false));
        self.last_word = Some(word);
        Ok(Arg::Opt(spec.key, value))
    }
}

/// The status a tool exits with for a usage error or a write error on
/// standard output (gnulib's `exit_failure`): 1, except for the tools whose
/// own results use 1 (`ls`, `sort`, `printenv`) or that run a command
/// (`env`, `timeout`: 125, like GNU's `EXIT_CANCELED`).
pub(crate) fn exit_failure(tool: &str) -> i32 {
    match tool {
        "ls" | "sort" | "printenv" => 2,
        "env" | "timeout" => 125,
        _ => 1,
    }
}

/// Writes `tool: ` and `msg` (bytes, as given) and a newline to stderr.
pub(crate) fn error_bytes(tool: &str, msg: &[u8]) {
    let mut line = Vec::with_capacity(tool.len() + msg.len() + 3);
    line.extend_from_slice(tool.as_bytes());
    line.extend_from_slice(b": ");
    line.extend_from_slice(msg);
    line.push(b'\n');
    let _ = std::io::stderr().write_all(&line);
}

/// Prints GNU's "Try 'TOOL --help'" hint and returns the tool's usage status.
pub(crate) fn try_help(tool: &str) -> i32 {
    eprintln!("Try '{tool} --help' for more information.");
    exit_failure(tool)
}

/// Prints a usage diagnostic (`tool: msg` and the `--help` hint) and returns
/// the tool's usage exit status.
pub(crate) fn usage_error(tool: &str, msg: impl fmt::Display) -> i32 {
    eprintln!("{tool}: {msg}");
    try_help(tool)
}

/// Prints a getopt diagnostic and the `--help` hint; returns the usage status.
pub(crate) fn getopt_error(tool: &str, err: &GetoptError) -> i32 {
    error_bytes(tool, err.as_bytes());
    try_help(tool)
}

/// Refuses a GNU option Dogfish does not implement yet. Such options are in
/// the tools' option tables all the same, so that abbreviations resolve (and
/// are ambiguous) exactly as in GNU. Returns exit status 1.
pub(crate) fn unsupported(tool: &str, spelled: &str) -> i32 {
    eprintln!("{tool}: option '{spelled}' is not supported yet");
    1
}

/// Prints `--help` text; returns 0. A write error is reported at exit.
pub(crate) fn print_help(text: &str) -> i32 {
    dogfish_core::stdout::print_str(text);
    0
}

/// Prints the `--version` line; returns 0. A write error is reported at exit.
pub(crate) fn print_version(tool: &str) -> i32 {
    dogfish_core::outln!("{tool} (GNU/Dogfish) {}", env!("CARGO_PKG_VERSION"));
    0
}

/// Resolves `value` against `table` like gnulib `argmatch`: an exact match,
/// or a prefix that selects one meaning (synonyms share a value and are
/// adjacent). On failure prints GNU's "invalid/ambiguous argument" block and
/// the `--help` hint, and returns GNU's `ARGMATCH_DIE` status, 1.
pub(crate) fn argmatch<V: Copy + PartialEq>(
    tool: &str,
    option: &str,
    value: impl AsRef<OsStr>,
    table: &[(&str, V)],
) -> Result<V, i32> {
    let value = value.as_ref();
    let bytes = value.as_bytes();
    if let Some((_, v)) = table.iter().find(|(name, _)| name.as_bytes() == bytes) {
        return Ok(*v);
    }
    let matches: Vec<V> = table
        .iter()
        .filter(|(name, _)| name.as_bytes().starts_with(bytes))
        .map(|(_, v)| *v)
        .collect();
    if let Some(first) = matches.first()
        && matches.iter().all(|v| v == first)
    {
        return Ok(*first);
    }
    let problem = if matches.is_empty() {
        "invalid"
    } else {
        "ambiguous"
    };
    let mut msg = format!(
        "{tool}: {problem} argument {} for {}\nValid arguments are:",
        quote_locale(value),
        quote_locale(option)
    );
    let mut prev: Option<V> = None;
    for (name, v) in table {
        if prev == Some(*v) {
            msg.push_str(&format!(", {}", quote_locale(name)));
        } else {
            msg.push_str(&format!("\n  - {}", quote_locale(name)));
        }
        prev = Some(*v);
    }
    eprintln!("{msg}");
    eprintln!("Try '{tool} --help' for more information.");
    Err(1)
}

/// gnulib `xdectoimax`: `arg` as a decimal integer within `min..=max`;
/// otherwise prints GNU's `tool: WHAT: 'ARG'` diagnostic (with the reason
/// when the number is out of range) and returns exit status 1.
pub(crate) fn xdectoimax(
    tool: &str,
    arg: &str,
    min: i64,
    max: i64,
    what: &str,
) -> Result<i64, i32> {
    const EOVERFLOW: &str = "Value too large for defined data type";
    const ERANGE: &str = "Numerical result out of range";
    let fail = |why: Option<&str>| {
        match why {
            Some(why) => eprintln!("{tool}: {what}: {}: {why}", quote_locale(arg)),
            None => eprintln!("{tool}: {what}: {}", quote_locale(arg)),
        }
        1
    };
    let t = arg.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let len = digits.bytes().take_while(u8::is_ascii_digit).count();
    if len == 0 {
        return Err(fail(None));
    }
    let value = format!("{}{}", if neg { "-" } else { "" }, &digits[..len]).parse::<i64>();
    if len < digits.len() {
        return Err(fail(None));
    }
    let n = value.map_err(|_| fail(Some(EOVERFLOW)))?;
    if n < min || n > max {
        let type_overflow = n > i64::from(i32::MAX / 2) || n < i64::from(i32::MIN / 2);
        return Err(fail(Some(if type_overflow { EOVERFLOW } else { ERANGE })));
    }
    Ok(n)
}

/// A count for a Dogfish option (`undo -n 2`): a decimal number, else a
/// usage diagnostic naming the option.
pub(crate) fn count_arg(tool: &str, option: &str, value: &str) -> Result<usize, i32> {
    value.parse().map_err(|_| {
        usage_error(
            tool,
            format!(
                "invalid argument {} for {}",
                quote_locale(value),
                quote_locale(option)
            ),
        )
    })
}

/// An argument as text, for tools whose logic works on `str`. Bytes that
/// are not UTF-8 become U+FFFD; tools that must keep them use the bytes.
pub(crate) fn lossy(arg: &OsStr) -> String {
    arg.to_string_lossy().into_owned()
}

/// [`lossy`] over a list of arguments.
pub(crate) fn lossy_all(args: Vec<OsString>) -> Vec<String> {
    args.into_iter()
        .map(|a| a.into_string().unwrap_or_else(|a| lossy(&a)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum K {
        All,
        Number,
        Delim,
        Width,
        ShowAll,
        Squeeze,
        ShowEnds,
        Help,
        Version,
        VersionSort,
        Tty,
    }

    const SPECS: &[OptSpec<K>] = &[
        OptSpec::both(b'a', "all", HasArg::No, K::All),
        OptSpec::both(b'n', "number", HasArg::Required, K::Number),
        OptSpec::both(b'd', "delimiter", HasArg::Required, K::Delim),
        OptSpec::both(b'w', "width", HasArg::Optional, K::Width),
        OptSpec::long("show-all", HasArg::No, K::ShowAll),
        OptSpec::both(b's', "squeeze-blank", HasArg::No, K::Squeeze),
        OptSpec::long("show-ends", HasArg::No, K::ShowEnds),
        OptSpec::long("show-everything", HasArg::No, K::ShowAll),
        OptSpec::long("version-sort", HasArg::No, K::VersionSort),
        OptSpec::long("help", HasArg::No, K::Help),
        OptSpec::long("version", HasArg::No, K::Version),
        OptSpec::long("-presume-input-tty", HasArg::No, K::Tty),
    ];

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    type Parsed = (Vec<(K, Option<String>)>, Vec<String>);

    fn parse_with(args: Vec<OsString>, order: Order) -> Result<Parsed, String> {
        let mut g = Getopt::with_order(args, SPECS, order);
        let mut opts = Vec::new();
        while let Some(o) = g.next() {
            let (k, v) = o.map_err(|e| e.to_string())?;
            opts.push((k, v.map(|v| lossy(&v))));
        }
        Ok((opts, lossy_all(g.operands())))
    }

    fn parse(args: &[&str]) -> Result<Parsed, String> {
        parse_with(os(args), Order::Permute)
    }

    fn some(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    #[test]
    fn permutes_operands_after_options() {
        let (opts, operands) = parse(&["f1", "-a", "f2", "--number", "5", "f3"]).unwrap();
        assert_eq!(opts, vec![(K::All, None), (K::Number, some("5"))]);
        assert_eq!(operands, ["f1", "f2", "f3"]);
    }

    #[test]
    fn require_order_stops_at_first_operand() {
        let (opts, operands) = parse_with(
            os(&["-a", "cmd", "-n", "1", "--", "x"]),
            Order::StopAtOperand,
        )
        .unwrap();
        assert_eq!(opts, vec![(K::All, None)]);
        assert_eq!(operands, ["cmd", "-n", "1", "--", "x"]);
    }

    #[test]
    fn return_in_order_yields_operands_in_place() {
        let mut g = Getopt::with_order(os(&["x", "-a", "y"]), SPECS, Order::InPlace);
        let mut seen = Vec::new();
        while let Some(item) = g.next_in_order() {
            seen.push(match item.unwrap() {
                Arg::Opt(k, _) => format!("{k:?}"),
                Arg::Operand(o) => lossy(&o),
            });
        }
        assert_eq!(seen, ["x", "All", "y"]);
    }

    #[test]
    fn clusters_and_attached_arguments() {
        let (opts, _) = parse(&["-an5", "-n", "6", "-ad:", "-w", "-w3"]).unwrap();
        assert_eq!(
            opts,
            vec![
                (K::All, None),
                (K::Number, some("5")),
                (K::Number, some("6")),
                (K::All, None),
                (K::Delim, some(":")),
                (K::Width, None),
                (K::Width, some("3")),
            ]
        );
    }

    #[test]
    fn optional_long_argument_only_with_equals() {
        let (opts, operands) = parse(&["--width", "7", "--width=8", "--wid="]).unwrap();
        assert_eq!(
            opts,
            vec![
                (K::Width, None),
                (K::Width, some("8")),
                (K::Width, some(""))
            ]
        );
        assert_eq!(operands, ["7"]);
    }

    #[test]
    fn required_long_argument_attached_or_separate() {
        let (opts, _) = parse(&["--number=1", "--number", "2", "--num", "-3"]).unwrap();
        assert_eq!(
            opts,
            vec![
                (K::Number, some("1")),
                (K::Number, some("2")),
                (K::Number, some("-3"))
            ]
        );
    }

    #[test]
    fn double_dash_ends_options_and_dash_is_an_operand() {
        let (opts, operands) = parse(&["-", "-a", "--", "-n", "--"]).unwrap();
        assert_eq!(opts, vec![(K::All, None)]);
        assert_eq!(operands, ["-", "-n", "--"]);
    }

    #[test]
    fn abbreviations_and_exact_match() {
        // `--version` is exact, though also a prefix of `--version-sort`.
        let (opts, _) = parse(&["--al", "--squ", "--version", "--h"]).unwrap();
        assert_eq!(
            opts,
            vec![
                (K::All, None),
                (K::Squeeze, None),
                (K::Version, None),
                (K::Help, None)
            ]
        );
        // A prefix shared only by synonyms (same key and argument) is no ambiguity.
        let (opts, _) = parse(&["--show-a"]).unwrap();
        assert_eq!(opts, vec![(K::ShowAll, None)]);
        assert_eq!(
            parse(&["--show-e"]).unwrap_err(),
            "option '--show-e' is ambiguous; possibilities: '--show-ends' '--show-everything'"
        );
    }

    #[test]
    fn ambiguity_lists_possibilities_in_table_order() {
        assert_eq!(
            parse(&["--s"]).unwrap_err(),
            "option '--s' is ambiguous; possibilities: '--show-all' '--squeeze-blank' \
             '--show-ends'"
        );
        assert_eq!(
            parse(&["--ver=x"]).unwrap_err(),
            "option '--ver=x' is ambiguous; possibilities: '--version-sort' '--version'"
        );
        // Synonyms of the first match are left out of the list.
        assert_eq!(
            parse(&["--show-"]).unwrap_err(),
            "option '--show-' is ambiguous; possibilities: '--show-all' '--show-ends'"
        );
    }

    #[test]
    fn glibc_diagnostics() {
        assert_eq!(parse(&["-x"]).unwrap_err(), "invalid option -- 'x'");
        assert_eq!(parse(&["-a:"]).unwrap_err(), "invalid option -- ':'");
        assert_eq!(
            parse(&["--bogus=1"]).unwrap_err(),
            "unrecognized option '--bogus=1'"
        );
        assert_eq!(
            parse(&["-n"]).unwrap_err(),
            "option requires an argument -- 'n'"
        );
        assert_eq!(
            parse(&["--num"]).unwrap_err(),
            "option '--number' requires an argument"
        );
        assert_eq!(
            parse(&["--al=1"]).unwrap_err(),
            "option '--all' doesn't allow an argument"
        );
    }

    #[test]
    fn hidden_three_dash_options() {
        let (opts, _) = parse(&["---presume-input-tty", "---pre"]).unwrap();
        assert_eq!(opts, vec![(K::Tty, None), (K::Tty, None)]);
    }

    #[test]
    fn non_utf8_arguments_pass_through() {
        let raw = |b: &[u8]| OsString::from_vec(b.to_vec());
        let args = vec![raw(b"-d\xff"), raw(b"f\xb0"), raw(b"--number=\xfe")];
        let mut g = Getopt::new(args, SPECS);
        let (k, v) = g.next().unwrap().unwrap();
        assert_eq!((k, v.unwrap().as_bytes()), (K::Delim, &b"\xff"[..]));
        let (k, v) = g.next().unwrap().unwrap();
        assert_eq!((k, v.unwrap().as_bytes()), (K::Number, &b"\xfe"[..]));
        assert!(g.next().is_none());
        assert_eq!(g.operands(), vec![raw(b"f\xb0")]);

        let mut g = Getopt::new(vec![raw(b"-\xc3"), raw(b"--\xff")], SPECS);
        assert_eq!(
            g.next().unwrap().unwrap_err().as_bytes(),
            b"invalid option -- '\xc3'"
        );
        assert_eq!(
            g.next().unwrap().unwrap_err().as_bytes(),
            b"unrecognized option '--\xff'"
        );
    }

    #[test]
    fn last_spelling_names_the_full_option() {
        let mut g = Getopt::new(os(&["--num=1", "-n2"]), SPECS);
        g.next();
        assert_eq!(g.last_spelling(), "--number");
        g.next();
        assert_eq!(g.last_spelling(), "-n");
    }

    #[test]
    fn operands_after_an_unfinished_parse() {
        let mut g = Getopt::with_order(os(&["-a", "x", "-a"]), SPECS, Order::StopAtOperand);
        assert!(g.next().is_some());
        assert_eq!(g.peek(), Some(OsStr::new("x")));
        assert!(g.next().is_none());
        assert_eq!(lossy_all(g.operands()), ["x", "-a"]);
    }

    #[test]
    fn splice_front_resumes_with_new_arguments() {
        let mut g = Getopt::with_order(os(&["-a", "rest"]), SPECS, Order::StopAtOperand);
        assert!(g.next().is_some());
        g.splice_front(os(&["-n", "1"]));
        assert_eq!(g.next().unwrap().unwrap(), (K::Number, Some("1".into())));
        assert!(g.next().is_none());
        assert_eq!(lossy_all(g.operands()), ["rest"]);
    }

    #[test]
    fn argmatch_prefixes() {
        let table = [("never", 0), ("no", 0), ("none", 0), ("once", 1)];
        assert_eq!(argmatch("t", "--x", "on", &table), Ok(1));
        assert_eq!(argmatch("t", "--x", "n", &table), Ok(0));
        assert_eq!(argmatch("t", "--x", "o", &table), Ok(1));
        assert_eq!(argmatch("t", "--x", "x", &table), Err(1));
        assert_eq!(argmatch("t", "--x", "", &table), Err(1));
    }

    #[test]
    fn xdectoimax_ranges() {
        assert_eq!(xdectoimax("t", " +7", 1, 9, "w"), Ok(7));
        assert_eq!(xdectoimax("t", "-3", i64::MIN, i64::MAX, "w"), Ok(-3));
        assert_eq!(xdectoimax("t", "0", 1, 9, "w"), Err(1));
        assert_eq!(xdectoimax("t", "5x", 1, 9, "w"), Err(1));
        assert_eq!(xdectoimax("t", "99999999999999999999", 1, 9, "w"), Err(1));
    }

    #[test]
    fn exit_failure_statuses() {
        assert_eq!(exit_failure("ls"), 2);
        assert_eq!(exit_failure("env"), 125);
        assert_eq!(exit_failure("cat"), 1);
    }

    /// Parses `args` accepting the Dogfish switches `allowed`; returns the
    /// parse and the switches it recorded.
    fn parse_switches(
        args: &[&str],
        specs: &[OptSpec<K>],
        allowed: Switch,
    ) -> (Result<Parsed, String>, Arc<Switches>) {
        let ctx = dogfish_core::DogfishContext::new("test");
        ctx.switches.set_stats(false);
        let mut g = Getopt::new(os(args), specs).with_switches(allowed, ctx.switches.clone());
        let mut opts = Vec::new();
        let res = loop {
            match g.next() {
                Some(Ok((k, v))) => opts.push((k, v.map(|v| lossy(&v)))),
                Some(Err(e)) => break Err(e.to_string()),
                None => break Ok(()),
            }
        };
        (
            res.map(|()| (opts, lossy_all(g.operands()))),
            ctx.switches.clone(),
        )
    }

    #[test]
    fn switches_match_only_in_full() {
        let all = Switch::STATS.with(Switch::NO_COLOR).with(Switch::COLOR);
        let (res, sw) = parse_switches(&["--stats", "f", "--no-color", "-a"], SPECS, all);
        assert_eq!(res.unwrap(), (vec![(K::All, None)], vec!["f".to_string()]));
        assert!(sw.stats());
        assert_eq!(sw.color(), ColorMode::Never);
        // An abbreviation is left to the tool's own options, as in GNU.
        let (res, sw) = parse_switches(&["--stat"], SPECS, all);
        assert_eq!(res.unwrap_err(), "unrecognized option '--stat'");
        assert!(!sw.stats());
        let (res, sw) = parse_switches(&["--color=never", "--color"], SPECS, all);
        assert!(res.is_ok());
        assert_eq!(sw.color(), ColorMode::Always);
        // Quoted by locale, as GNU's quote() does: 'x' in C, curly quotes in UTF-8.
        let (res, _) = parse_switches(&["--color=sometimes"], SPECS, all);
        let want = format!(
            "invalid argument {} for {}\n",
            quote_locale(OsStr::new("sometimes")),
            quote_locale(OsStr::new("--color"))
        );
        assert!(res.unwrap_err().starts_with(&want));
    }

    #[test]
    fn switches_not_granted_are_gnu_errors_or_operands() {
        let (res, sw) = parse_switches(&["--stats"], SPECS, Switch::NONE);
        assert_eq!(res.unwrap_err(), "unrecognized option '--stats'");
        assert!(!sw.stats());
        let (res, _) = parse_switches(&["--color"], SPECS, Switch::STATS);
        assert_eq!(res.unwrap_err(), "unrecognized option '--color'");
        // After `--`, a switch is an operand.
        let (res, sw) = parse_switches(&["--", "--stats"], SPECS, Switch::STATS);
        assert_eq!(res.unwrap().1, ["--stats"]);
        assert!(!sw.stats());
    }

    #[test]
    fn a_tools_own_option_shadows_a_switch() {
        const OWN: &[OptSpec<K>] = &[OptSpec::long("color", HasArg::Optional, K::Width)];
        let (res, sw) = parse_switches(&["--color=never"], OWN, Switch::COLOR);
        assert_eq!(res.unwrap().0, vec![(K::Width, some("never"))]);
        assert_eq!(sw.color(), ColorMode::Auto);
    }

    #[test]
    fn exact_specs_are_never_abbreviated_or_ambiguous() {
        const EXT: &[OptSpec<K>] = &[
            OptSpec::long("show-all", HasArg::No, K::ShowAll),
            OptSpec::long("show-ends", HasArg::No, K::ShowEnds),
            OptSpec::long("shout", HasArg::No, K::Tty).exact(),
        ];
        let first = |args: &[&str]| {
            Getopt::new(os(args), EXT)
                .next()
                .map(|o| o.map(|(k, _)| k).map_err(|e| e.to_string()))
        };
        assert_eq!(first(&["--shout"]), Some(Ok(K::Tty)));
        assert_eq!(
            first(&["--shou"]),
            Some(Err("unrecognized option '--shou'".into()))
        );
        assert_eq!(
            first(&["--sh"]),
            Some(Err(
                "option '--sh' is ambiguous; possibilities: '--show-all' '--show-ends'".into()
            ))
        );
        assert_eq!(first(&["--show-a"]), Some(Ok(K::ShowAll)));
    }
}
