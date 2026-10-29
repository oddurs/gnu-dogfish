//! sort — GNU key/ordering semantics + parallel merge sort + NDJSON `--by <field>`

use std::borrow::Cow;
use std::cmp::Ordering;
use std::hash::{BuildHasher, RandomState};
use std::io::{self, Read, Write};

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;

use anyhow::Result;
use digest::Digest;
use dogfish_core::{
    DogfishContext, FastReader, InputSource, emit_objects_or_table, eval_json_where, gnu_io_error,
    io::ignore_broken_pipe, read_ndjson_stdin, stdout, table::compare_json_field,
};
use md5::Md5;
use rayon::prelude::*;

use crate::common::getopt::{
    Arg, Getopt, HasArg, OptSpec, Order, argmatch, getopt_error, lossy, lossy_all, print_help,
    print_version, unsupported,
};
use crate::common::lines::{is_blank, looks_like_json, lossy_lines, split_lines};

const SORT_HELP: &str = "\
Sort lines of text files or NDJSON object streams

Usage: sort [OPTIONS] [FILES]...

Arguments:
  [FILES]...  

Options:
      --help                         Print help
  -b, --ignore-leading-blanks        Ignore leading blanks
  -d, --dictionary-order             Consider only blanks and alphanumeric characters
  -f, --ignore-case                  Fold lower case to upper case characters
  -g, --general-numeric-sort         Compare according to general numerical value
  -h, --human-numeric-sort           Compare human readable numbers (e.g., 2K, 1G)
  -i, --ignore-nonprinting           Consider only printable characters
  -M, --month-sort                   Compare (unknown) < 'JAN' < ... < 'DEC'
  -n, --numeric-sort                 Compare according to string numerical value
  -R, --random-sort                  Shuffle, but group identical keys
      --random-source <FILE>         Get random bytes from FILE
      --sort <WORD>                  Sort according to WORD: general-numeric -g, human-numeric -h,
                                     month -M, numeric -n, random -R, version -V
  -V, --version-sort                 Natural sort of (version) numbers within text
  -r, --reverse                      Reverse the result of comparisons [alias: --desc]
  -c, --check                        Check for sorted input; do not sort
  -k, --key <KEYS>                   Sort via a key; KEYDEF gives location and type
                                     (F[.C][OPTS][,F[.C][OPTS]])
  -o, --output <OUTPUT>              Write result to FILE instead of standard output
  -s, --stable                       Stabilize sort by disabling last-resort comparison
  -t, --field-separator <SEPARATOR>  Use SEP instead of non-blank to blank transition
  -u, --unique                       Output only the first of an equal run
      --by <BY_FIELD>                Modern Dogfish extension: Sort NDJSON objects by named field
                                     (e.g., `--by size`)
  -w, --where <WHERE_EXPR>           Filter NDJSON objects before sorting (e.g., `--where
                                     \"size>10K\"`)
  -J, --json                         Emit JSON / NDJSON output
";

#[derive(Default)]
struct SortArgs {
    ignore_blanks: bool,
    dictionary: bool,
    ignore_case: bool,
    general_numeric: bool,
    human_numeric: bool,
    ignore_nonprinting: bool,
    month: bool,
    numeric: bool,
    random: bool,
    random_source: Option<String>,
    sort_words: Vec<String>,
    version: bool,
    reverse: bool,
    check: bool,
    /// `-C`: check without reporting the disorder.
    check_quiet: bool,
    keys: Vec<String>,
    output: Option<String>,
    stable: bool,
    separator: Option<String>,
    unique: bool,
    by_field: Option<String>,
    where_expr: Option<String>,
    json: bool,
    files: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortOpt {
    IgnoreBlanks,
    Check,
    CheckQuiet,
    CheckWhen,
    Dictionary,
    IgnoreCase,
    General,
    Nonprinting,
    Key,
    Month,
    Numeric,
    Human,
    Version,
    Random,
    RandomSource,
    Sort,
    Output,
    Reverse,
    Stable,
    Separator,
    Unique,
    By,
    Where,
    Json,
    /// A GNU option Dogfish does not implement yet.
    Unsupported,
    Help,
    PrintVersion,
}

/// GNU's `long_options` (in its order: it decides ambiguity diagnostics) and
/// `short_options` for the options Dogfish implements, plus the Dogfish
/// `--desc`, `--by`, `-w, --where` and `-J, --json`.
const SPECS: &[OptSpec<SortOpt>] = &[
    OptSpec::both(
        b'b',
        "ignore-leading-blanks",
        HasArg::No,
        SortOpt::IgnoreBlanks,
    ),
    OptSpec::long("check", HasArg::Optional, SortOpt::CheckWhen),
    OptSpec::long("compress-program", HasArg::Required, SortOpt::Unsupported),
    OptSpec::long("debug", HasArg::No, SortOpt::Unsupported),
    OptSpec::both(b'd', "dictionary-order", HasArg::No, SortOpt::Dictionary),
    OptSpec::both(b'f', "ignore-case", HasArg::No, SortOpt::IgnoreCase),
    OptSpec::long("files0-from", HasArg::Required, SortOpt::Unsupported),
    OptSpec::both(b'g', "general-numeric-sort", HasArg::No, SortOpt::General),
    OptSpec::both(b'i', "ignore-nonprinting", HasArg::No, SortOpt::Nonprinting),
    OptSpec::both(b'k', "key", HasArg::Required, SortOpt::Key),
    OptSpec::both(b'm', "merge", HasArg::No, SortOpt::Unsupported),
    OptSpec::both(b'M', "month-sort", HasArg::No, SortOpt::Month),
    OptSpec::both(b'n', "numeric-sort", HasArg::No, SortOpt::Numeric),
    OptSpec::both(b'h', "human-numeric-sort", HasArg::No, SortOpt::Human),
    OptSpec::both(b'V', "version-sort", HasArg::No, SortOpt::Version),
    OptSpec::both(b'R', "random-sort", HasArg::No, SortOpt::Random),
    OptSpec::long("random-source", HasArg::Required, SortOpt::RandomSource),
    OptSpec::long("sort", HasArg::Required, SortOpt::Sort),
    OptSpec::both(b'o', "output", HasArg::Required, SortOpt::Output),
    OptSpec::both(b'r', "reverse", HasArg::No, SortOpt::Reverse),
    OptSpec::both(b's', "stable", HasArg::No, SortOpt::Stable),
    OptSpec::long("batch-size", HasArg::Required, SortOpt::Unsupported),
    OptSpec::both(b'S', "buffer-size", HasArg::Required, SortOpt::Unsupported),
    OptSpec::both(
        b't',
        "field-separator",
        HasArg::Required,
        SortOpt::Separator,
    ),
    OptSpec::both(
        b'T',
        "temporary-directory",
        HasArg::Required,
        SortOpt::Unsupported,
    ),
    OptSpec::both(b'u', "unique", HasArg::No, SortOpt::Unique),
    OptSpec::both(b'z', "zero-terminated", HasArg::No, SortOpt::Unsupported),
    OptSpec::long("parallel", HasArg::Required, SortOpt::Unsupported),
    OptSpec::long("help", HasArg::No, SortOpt::Help),
    OptSpec::long("version", HasArg::No, SortOpt::PrintVersion),
    OptSpec::long("desc", HasArg::No, SortOpt::Reverse).exact(),
    OptSpec::long("by", HasArg::Required, SortOpt::By).exact(),
    OptSpec::both(b'w', "where", HasArg::Required, SortOpt::Where).exact(),
    OptSpec::both(b'J', "json", HasArg::No, SortOpt::Json).exact(),
    OptSpec::short(b'c', HasArg::No, SortOpt::Check),
    OptSpec::short(b'C', HasArg::No, SortOpt::CheckQuiet),
];

/// `N[.M]` of an obsolete key position and the ordering letters after it.
fn obsolete_pos(pos: &[u8]) -> Option<(u64, u64, &[u8])> {
    let digits = |s: &[u8]| s.iter().take_while(|c| c.is_ascii_digit()).count();
    let number = |s: &[u8]| std::str::from_utf8(s).ok()?.parse::<u64>().ok();
    let n = digits(pos);
    let field = number(&pos[..n])?;
    let mut rest = &pos[n..];
    let mut chr = 0;
    if let Some(r) = rest.strip_prefix(b".") {
        let m = digits(r);
        chr = number(&r[..m])?;
        rest = &r[m..];
    }
    rest.iter()
        .all(|c| b"bdfgiMhnRrV".contains(c))
        .then_some((field, chr, rest))
}

/// The `-k` spelling of GNU's obsolete `+POS1 [-POS2]` key (`+1 -3` is
/// `-k 2.1,3`), or `None` when `pos1` (after the `+`) is no valid position:
/// GNU then takes the operand for a file. `pos2` is after its `-`.
fn obsolete_key(pos1: &[u8], pos2: Option<&[u8]>) -> Option<String> {
    let (sword, schar, sopts) = obsolete_pos(pos1)?;
    let mut key = format!(
        "{}.{}{}",
        sword.saturating_add(1),
        schar.saturating_add(1),
        String::from_utf8_lossy(sopts)
    );
    if let Some(pos2) = pos2 {
        let (eword, echar, eopts) = obsolete_pos(pos2)?;
        let eopts = String::from_utf8_lossy(eopts);
        // `-B` ends with field B; `-B.y` at character y of field B+1.
        if echar == 0 {
            key.push_str(&format!(",{}{eopts}", eword.max(1)));
        } else {
            key.push_str(&format!(",{}.{echar}{eopts}", eword.saturating_add(1)));
        }
    }
    Some(key)
}

/// Parses the command line; `Err` is the exit status after `--help`,
/// `--version` or a diagnostic.
fn parse_sort_args(args: Vec<OsString>) -> std::result::Result<SortArgs, i32> {
    use SortOpt::*;
    let mut p = SortArgs::default();
    let mut opts = Getopt::with_order(args.into_iter().skip(1).collect(), SPECS, Order::InPlace);
    let posixly_correct = std::env::var_os("POSIXLY_CORRECT").is_some();
    // GNU's `checkonly`: `c` reports the first disorder, `C` does not.
    let mut checkonly: Option<char> = None;
    let incompatible = |opts: &str| {
        eprintln!("sort: options '-{opts}' are incompatible");
        2
    };
    while let Some(item) = opts.next_in_order() {
        let (o, v) = match item.map_err(|e| getopt_error("sort", &e))? {
            Arg::Operand(file) => {
                // GNU's obsolete `+POS1 [-POS2]` keys, as in its getopt loop.
                if let Some(pos1) = file.as_bytes().strip_prefix(b"+") {
                    let pos2 = opts
                        .peek()
                        .map(|n| n.as_bytes())
                        .filter(|n| !posixly_correct && n.len() > 1 && n[1].is_ascii_digit())
                        .and_then(|n| n.strip_prefix(b"-"))
                        .map(<[u8]>::to_vec);
                    if let Some(key) = obsolete_key(pos1, pos2.as_deref()) {
                        if pos2.is_some() {
                            opts.take_next();
                        }
                        p.keys.push(key);
                        continue;
                    }
                }
                p.files.push(lossy(&file));
                continue;
            }
            Arg::Opt(o, v) => (o, v.as_deref().map(lossy)),
        };
        match o {
            IgnoreBlanks => p.ignore_blanks = true,
            Check | CheckQuiet | CheckWhen => {
                let c = match (o, v) {
                    (CheckQuiet, _) => 'C',
                    (CheckWhen, Some(when)) => argmatch(
                        "sort",
                        "--check",
                        when,
                        &[("quiet", 'C'), ("silent", 'C'), ("diagnose-first", 'c')],
                    )?,
                    _ => 'c',
                };
                if checkonly.is_some_and(|k| k != c) {
                    return Err(incompatible("cC"));
                }
                checkonly = Some(c);
                (p.check, p.check_quiet) = (true, c == 'C');
            }
            Dictionary => p.dictionary = true,
            IgnoreCase => p.ignore_case = true,
            General => p.general_numeric = true,
            Nonprinting => p.ignore_nonprinting = true,
            Key => p.keys.extend(v),
            Month => p.month = true,
            Numeric => p.numeric = true,
            Human => p.human_numeric = true,
            Version => p.version = true,
            Random => p.random = true,
            RandomSource => p.random_source = v,
            Sort => p.sort_words.extend(v),
            Output => {
                if p.output.is_some() && p.output != v {
                    eprintln!("sort: multiple output files specified");
                    return Err(2);
                }
                p.output = v;
            }
            Reverse => p.reverse = true,
            Stable => p.stable = true,
            Separator => {
                if p.separator.is_some() && p.separator != v {
                    eprintln!("sort: incompatible tabs");
                    return Err(2);
                }
                p.separator = v;
            }
            Unique => p.unique = true,
            By => p.by_field = v,
            Where => p.where_expr = v,
            Json => p.json = true,
            Unsupported => return Err(unsupported("sort", &opts.last_spelling())),
            Help => return Err(print_help(SORT_HELP)),
            PrintVersion => return Err(print_version("sort")),
        }
    }
    p.files.extend(lossy_all(opts.operands()));
    if let Some(c) = checkonly {
        if let Some(extra) = p.files.get(1) {
            eprintln!(
                "sort: extra operand {} not allowed with -{c}",
                crate::common::quote::qa(extra)
            );
            return Err(2);
        }
        if p.output.is_some() {
            return Err(incompatible(&format!("{c}o")));
        }
    }
    Ok(p)
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn obsolete_keys_translate_to_k() {
        assert_eq!(obsolete_key(b"1", Some(b"3")).as_deref(), Some("2.1,3"));
        assert_eq!(obsolete_key(b"0", Some(b"1")).as_deref(), Some("1.1,1"));
        assert_eq!(
            obsolete_key(b"1.1", Some(b"1.2")).as_deref(),
            Some("2.2,2.2")
        );
        assert_eq!(obsolete_key(b"1n", None).as_deref(), Some("2.1n"));
        assert_eq!(obsolete_key(b"x", None), None);
        assert_eq!(obsolete_key(b"", None), None);
        assert_eq!(obsolete_key(b"1.", None), None);
    }
}

#[derive(Clone, Copy, Default)]
struct SortOpts {
    skip_sblanks: bool,
    skip_eblanks: bool,
    dictionary: bool,
    fold: bool,
    general: bool,
    human: bool,
    nonprinting: bool,
    month: bool,
    numeric: bool,
    random: bool,
    reverse: bool,
    version: bool,
}

impl SortOpts {
    /// GNU `default_key_compare`: no ordering option at all (reverse excluded).
    fn is_default(&self) -> bool {
        !(self.skip_sblanks
            || self.skip_eblanks
            || self.dictionary
            || self.fold
            || self.general
            || self.human
            || self.nonprinting
            || self.month
            || self.numeric
            || self.random
            || self.version)
    }

    /// True when `-d`, `-i` or `-f` rewrite the key before it is compared.
    fn filters(&self) -> bool {
        self.dictionary || self.nonprinting || self.fold
    }

    /// GNU `key_to_opts`: the key's option letters, in GNU's canonical order.
    fn letters(&self) -> String {
        [
            (self.skip_sblanks || self.skip_eblanks, 'b'),
            (self.dictionary, 'd'),
            (self.fold, 'f'),
            (self.general, 'g'),
            (self.human, 'h'),
            (self.nonprinting, 'i'),
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

    /// GNU `check_ordering_compatibility`: at most one of `-n`, `-g`, `-h`, `-M`
    /// and the string orderings (`-V`, `-R`, `-d`, `-i`, which combine freely).
    fn is_compatible(&self) -> bool {
        let string_like = self.version || self.random || self.dictionary || self.nonprinting;
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

#[derive(Clone, Copy)]
struct SortKey {
    /// Zero-based start field and character offset.
    sword: usize,
    schar: usize,
    /// Zero-based end field (`None` = end of line) and 1-based char (0 = end of field).
    eword: Option<usize>,
    echar: usize,
    opts: SortOpts,
}

fn parse_sort_key(spec: &str) -> std::result::Result<SortKey, String> {
    let invalid = |what: &str| format!("{}: invalid field specification '{}'", what, spec);
    let b = spec.as_bytes();
    let mut i = 0;
    let number = |i: &mut usize| -> Option<usize> {
        let st = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        (st != *i).then(|| spec[st..*i].parse().unwrap_or(usize::MAX))
    };
    let mut opts = SortOpts::default();
    let parse_opts = |i: &mut usize, start: bool, opts: &mut SortOpts| -> Result<(), String> {
        while *i < b.len() && b[*i] != b',' {
            match b[*i] {
                b'b' if start => opts.skip_sblanks = true,
                b'b' => opts.skip_eblanks = true,
                b'd' => opts.dictionary = true,
                b'f' => opts.fold = true,
                b'g' => opts.general = true,
                b'h' => opts.human = true,
                b'i' => opts.nonprinting = true,
                b'M' => opts.month = true,
                b'n' => opts.numeric = true,
                b'R' => opts.random = true,
                b'r' => opts.reverse = true,
                b'V' => opts.version = true,
                _ => return Err(invalid("stray character in field spec")),
            }
            *i += 1;
        }
        Ok(())
    };

    let sfield = number(&mut i).ok_or_else(|| invalid("invalid number at field start"))?;
    if sfield == 0 {
        return Err(invalid("field number is zero"));
    }
    let mut schar = 1;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        schar = number(&mut i).ok_or_else(|| invalid("invalid number after '.'"))?;
        if schar == 0 {
            return Err(invalid("character offset is zero"));
        }
    }
    parse_opts(&mut i, true, &mut opts)?;
    let mut eword = None;
    let mut echar = 0;
    if i < b.len() && b[i] == b',' {
        i += 1;
        let efield = number(&mut i).ok_or_else(|| invalid("invalid number after ','"))?;
        if efield == 0 {
            return Err(invalid("field number is zero"));
        }
        eword = Some(efield - 1);
        if i < b.len() && b[i] == b'.' {
            i += 1;
            echar = number(&mut i).ok_or_else(|| invalid("invalid number after '.'"))?;
        }
        parse_opts(&mut i, false, &mut opts)?;
    }
    Ok(SortKey {
        sword: sfield - 1,
        schar: schar - 1,
        eword,
        echar,
        opts,
    })
}

struct Sorter {
    keys: Vec<SortKey>,
    tab: Option<u8>,
    /// Disable the last-resort whole-line comparison (`-s` / `-u`).
    no_last_resort: bool,
    reverse: bool,
    /// MD5 state primed with the `-R` seed; each key is hashed with a clone.
    random_seed: Md5,
}

impl Sorter {
    fn begfield(&self, line: &[u8], key: &SortKey) -> usize {
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
                while p < lim && is_blank(line[p]) {
                    p += 1;
                }
                while p < lim && !is_blank(line[p]) {
                    p += 1;
                }
            }
        }
        if key.opts.skip_sblanks {
            while p < lim && is_blank(line[p]) {
                p += 1;
            }
        }
        (p + key.schar).min(lim)
    }

    fn limfield(&self, line: &[u8], key: &SortKey) -> usize {
        let lim = line.len();
        let Some(mut eword) = key.eword else {
            return lim;
        };
        let echar = key.echar;
        if echar == 0 {
            eword += 1;
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
                while p < lim && is_blank(line[p]) {
                    p += 1;
                }
                while p < lim && !is_blank(line[p]) {
                    p += 1;
                }
            }
        }
        if echar != 0 {
            if key.opts.skip_eblanks {
                while p < lim && is_blank(line[p]) {
                    p += 1;
                }
            }
            p = (p + echar).min(lim);
        }
        p
    }

    fn key_slice<'a>(&self, line: &'a [u8], key: &SortKey) -> &'a [u8] {
        let beg = self.begfield(line, key);
        let end = self.limfield(line, key).max(beg);
        &line[beg..end]
    }

    fn keycompare(&self, a: &[u8], b: &[u8]) -> Ordering {
        for key in &self.keys {
            let ka = self.key_slice(a, key);
            let kb = self.key_slice(b, key);
            let o = &key.opts;
            let ord = if o.numeric || o.general || o.human || o.month || o.random || o.version {
                // As in GNU, these orderings see the key after `-d`/`-i`/`-f` rewriting.
                let (ka, kb): (Cow<[u8]>, Cow<[u8]>) = if o.filters() {
                    (
                        key_filter(ka, o).collect::<Vec<u8>>().into(),
                        key_filter(kb, o).collect::<Vec<u8>>().into(),
                    )
                } else {
                    (ka.into(), kb.into())
                };
                if o.numeric {
                    numeric_compare(&ka, &kb)
                } else if o.general {
                    general_numeric_compare(&ka, &kb)
                } else if o.human {
                    human_unit_order(&ka)
                        .cmp(&human_unit_order(&kb))
                        .then_with(|| numeric_compare(&ka, &kb))
                } else if o.month {
                    month_of(&ka).cmp(&month_of(&kb))
                } else if o.random {
                    self.random_compare(&ka, &kb)
                } else {
                    filevercmp(&ka, &kb)
                }
            } else if o.filters() {
                key_filter(ka, o).cmp(key_filter(kb, o))
            } else {
                ka.cmp(kb)
            };
            if ord != Ordering::Equal {
                return if o.reverse { ord.reverse() } else { ord };
            }
        }
        Ordering::Equal
    }

    /// GNU `compare_random`: order by the seeded MD5 of each key, falling back
    /// to the bytes themselves so that only identical keys compare equal.
    fn random_compare(&self, a: &[u8], b: &[u8]) -> Ordering {
        let digest = |key: &[u8]| self.random_seed.clone().chain_update(key).finalize();
        digest(a).cmp(&digest(b)).then_with(|| a.cmp(b))
    }

    fn compare(&self, a: &[u8], b: &[u8]) -> Ordering {
        if !self.keys.is_empty() {
            let ord = self.keycompare(a, b);
            if ord != Ordering::Equal || self.no_last_resort {
                return ord;
            }
        }
        let ord = a.cmp(b);
        if self.reverse { ord.reverse() } else { ord }
    }
}

struct SortNum<'a> {
    neg: bool,
    int: &'a [u8],
    frac: &'a [u8],
}

/// Parses the leading `[blanks][-]digits[.digits]` of a key, like GNU `strnumcmp`.
fn parse_sort_num(s: &[u8]) -> SortNum<'_> {
    let mut i = 0;
    while i < s.len() && is_blank(s[i]) {
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

fn numeric_compare(a: &[u8], b: &[u8]) -> Ordering {
    let (x, y) = (parse_sort_num(a), parse_sort_num(b));
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
        (false, false) => mag(&x, &y),
        (true, true) => mag(&y, &x),
    }
}

/// GNU `find_unit_order`: signed SI-suffix magnitude of a human-readable number.
fn human_unit_order(s: &[u8]) -> i32 {
    let mut i = 0;
    while i < s.len() && is_blank(s[i]) {
        i += 1;
    }
    let neg = i < s.len() && s[i] == b'-';
    if neg {
        i += 1;
    }
    let mut max_digit = b'0';
    let mut seen_dot = false;
    while i < s.len() && (s[i].is_ascii_digit() || (s[i] == b'.' && !seen_dot)) {
        if s[i] == b'.' {
            seen_dot = true;
        } else {
            max_digit = max_digit.max(s[i]);
        }
        i += 1;
    }
    if max_digit == b'0' {
        return 0;
    }
    let order = match s.get(i) {
        Some(b'K') | Some(b'k') => 1,
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
    };
    if neg { -order } else { order }
}

/// strtod-style prefix parse for `-g`.
fn parse_general_num(s: &[u8]) -> Option<f64> {
    let mut i = 0;
    while i < s.len() && s[i].is_ascii_whitespace() {
        i += 1;
    }
    let st = i;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        i += 1;
    }
    let rest = &s[i..];
    let lower: Vec<u8> = rest
        .iter()
        .take(8)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if lower.starts_with(b"inf") || lower.starts_with(b"nan") {
        let neg = s[st] == b'-';
        return Some(match (&lower[..3], neg) {
            (b"inf", false) => f64::INFINITY,
            (b"inf", true) => f64::NEG_INFINITY,
            _ => f64::NAN,
        });
    }
    // Hexadecimal (strtod accepts `0x1F`, `0x1.8p3`).
    if lower.starts_with(b"0x")
        && lower
            .get(2)
            .is_some_and(|c| c.is_ascii_hexdigit() || *c == b'.')
    {
        let neg = s[st] == b'-';
        let mut j = i + 2;
        let mut v = 0f64;
        let mut any = false;
        while j < s.len() && s[j].is_ascii_hexdigit() {
            v = v * 16.0 + (s[j] as char).to_digit(16).unwrap() as f64;
            j += 1;
            any = true;
        }
        if j < s.len() && s[j] == b'.' {
            j += 1;
            let mut scale = 1.0 / 16.0;
            while j < s.len() && s[j].is_ascii_hexdigit() {
                v += (s[j] as char).to_digit(16).unwrap() as f64 * scale;
                scale /= 16.0;
                j += 1;
                any = true;
            }
        }
        if any {
            if j < s.len() && (s[j] == b'p' || s[j] == b'P') {
                let mut k = j + 1;
                let eneg = k < s.len() && s[k] == b'-';
                if k < s.len() && (s[k] == b'+' || s[k] == b'-') {
                    k += 1;
                }
                let es = k;
                while k < s.len() && s[k].is_ascii_digit() {
                    k += 1;
                }
                if k > es {
                    let e: i32 = std::str::from_utf8(&s[es..k])
                        .ok()?
                        .parse()
                        .unwrap_or(i32::MAX);
                    v *= 2f64.powi(if eneg { -e } else { e });
                }
            }
            return Some(if neg { -v } else { v });
        }
    }
    let digits_start = i;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    let mut has_digits = i > digits_start;
    if i < s.len() && s[i] == b'.' {
        i += 1;
        let fs = i;
        while i < s.len() && s[i].is_ascii_digit() {
            i += 1;
        }
        has_digits |= i > fs;
    }
    if !has_digits {
        return None;
    }
    if i < s.len() && (s[i] == b'e' || s[i] == b'E') {
        let mut j = i + 1;
        if j < s.len() && (s[j] == b'+' || s[j] == b'-') {
            j += 1;
        }
        let es = j;
        while j < s.len() && s[j].is_ascii_digit() {
            j += 1;
        }
        if j > es {
            i = j;
        }
    }
    std::str::from_utf8(&s[st..i]).ok()?.parse().ok()
}

fn general_numeric_compare(a: &[u8], b: &[u8]) -> Ordering {
    match (parse_general_num(a), parse_general_num(b)) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(x), Some(y)) => match x.partial_cmp(&y) {
            Some(o) => o,
            None => x.is_nan().cmp(&y.is_nan()).reverse(),
        },
    }
}

/// The bytes of a key after `-d`/`-i` deletion and `-f` folding.
fn key_filter<'a>(key: &'a [u8], o: &SortOpts) -> impl Iterator<Item = u8> + 'a {
    let SortOpts {
        dictionary,
        nonprinting,
        fold,
        ..
    } = *o;
    key.iter()
        .copied()
        .filter(move |&c| {
            if dictionary {
                c.is_ascii_alphanumeric() || is_blank(c)
            } else if nonprinting {
                (0x20..0x7f).contains(&c)
            } else {
                true
            }
        })
        .map(move |c| if fold { c.to_ascii_uppercase() } else { c })
}

/// GNU `getmonth`: 0 for unknown, else 1..=12 for a key that starts (after
/// blanks) with a case-insensitive `JAN`..`DEC`.
fn month_of(key: &[u8]) -> u8 {
    const MONTHS: [&[u8]; 12] = [
        b"JAN", b"FEB", b"MAR", b"APR", b"MAY", b"JUN", b"JUL", b"AUG", b"SEP", b"OCT", b"NOV",
        b"DEC",
    ];
    let start = key.iter().position(|&c| !is_blank(c)).unwrap_or(key.len());
    let Some(abbr) = key[start..].get(..3) else {
        return 0;
    };
    MONTHS
        .iter()
        .position(|m| abbr.eq_ignore_ascii_case(m))
        .map_or(0, |i| i as u8 + 1)
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
fn filevercmp(a: &[u8], b: &[u8]) -> Ordering {
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

/// Turns on the ordering a `--sort=WORD` names.
type SetOrdering = fn(&mut SortOpts);

/// Resolves a `--sort=WORD` argument the way GNU `argmatch` does: an exact
/// name or an unambiguous prefix.
fn parse_sort_word(word: &str) -> std::result::Result<SetOrdering, String> {
    const WORDS: [(&str, SetOrdering); 6] = [
        ("general-numeric", |o| o.general = true),
        ("human-numeric", |o| o.human = true),
        ("month", |o| o.month = true),
        ("numeric", |o| o.numeric = true),
        ("random", |o| o.random = true),
        ("version", |o| o.version = true),
    ];
    if let Some(&(_, set)) = WORDS.iter().find(|(name, _)| *name == word) {
        return Ok(set);
    }
    let mut matches = WORDS.iter().filter(|(name, _)| name.starts_with(word));
    let problem = match (matches.next(), matches.next()) {
        (Some(&(_, set)), None) => return Ok(set),
        (None, _) => "invalid",
        (Some(_), Some(_)) => "ambiguous",
    };
    let valid: String = WORDS
        .iter()
        .map(|(name, _)| format!("\n  - '{name}'"))
        .collect();
    Err(format!(
        "{problem} argument '{word}' for '--sort'\nValid arguments are:{valid}\nTry 'sort --help' for more information."
    ))
}

/// The 16 bytes GNU sort mixes into every `-R` hash: the start of
/// `--random-source=FILE`, or fresh randomness.
fn sort_random_seed(source: Option<&str>) -> std::result::Result<[u8; 16], String> {
    let mut seed = [0u8; 16];
    match source {
        Some(path) => {
            let mut file = std::fs::File::open(path)
                .map_err(|e| format!("open failed: {}: {}", path, gnu_io_error(&e)))?;
            file.read_exact(&mut seed).map_err(|e| match e.kind() {
                io::ErrorKind::UnexpectedEof => format!("'{path}': end of file"),
                _ => format!("'{}': read error: {}", path, gnu_io_error(&e)),
            })?;
        }
        None => {
            // std's `RandomState` keys are drawn from the OS entropy source.
            let state = RandomState::new();
            for (i, chunk) in seed.as_chunks_mut::<8>().0.iter_mut().enumerate() {
                *chunk = state.hash_one(i).to_ne_bytes();
            }
        }
    }
    Ok(seed)
}

pub fn run_sort(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_sort_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };

    let mut global = SortOpts {
        skip_sblanks: parsed.ignore_blanks,
        skip_eblanks: parsed.ignore_blanks,
        dictionary: parsed.dictionary,
        fold: parsed.ignore_case,
        general: parsed.general_numeric,
        human: parsed.human_numeric,
        nonprinting: parsed.ignore_nonprinting,
        month: parsed.month,
        numeric: parsed.numeric,
        random: parsed.random,
        reverse: parsed.reverse,
        version: parsed.version,
    };
    for word in &parsed.sort_words {
        match parse_sort_word(word) {
            Ok(set) => set(&mut global),
            Err(msg) => {
                eprintln!("sort: {msg}");
                return Ok(1);
            }
        }
    }
    let tab = match parsed.separator.as_deref() {
        None => None,
        Some("\\0") => Some(0),
        Some("") => {
            eprintln!("sort: empty tab");
            return Ok(2);
        }
        Some(s) if s.len() == 1 => Some(s.as_bytes()[0]),
        Some(s) => {
            eprintln!("sort: multi-character tab '{}'", s);
            return Ok(2);
        }
    };
    let mut keys = Vec::with_capacity(parsed.keys.len());
    for spec in &parsed.keys {
        match parse_sort_key(spec) {
            Ok(mut k) => {
                // Keys without ordering options inherit the global ones (GNU semantics).
                if k.opts.is_default() && !k.opts.reverse {
                    k.opts = global;
                }
                keys.push(k);
            }
            Err(msg) => {
                eprintln!("sort: {}", msg);
                return Ok(2);
            }
        }
    }
    if keys.is_empty() && !global.is_default() {
        keys.push(SortKey {
            sword: 0,
            schar: 0,
            eword: None,
            echar: 0,
            opts: global,
        });
    }
    if let Some(key) = keys.iter().find(|k| !k.opts.is_compatible()) {
        eprintln!("sort: options '-{}' are incompatible", key.opts.letters());
        return Ok(2);
    }
    let mut random_seed = Md5::new();
    if keys.iter().any(|k| k.opts.random) {
        match sort_random_seed(parsed.random_source.as_deref()) {
            Ok(seed) => random_seed.update(seed),
            Err(msg) => {
                eprintln!("sort: {msg}");
                return Ok(2);
            }
        }
    }
    let sorter = Sorter {
        keys,
        tab,
        no_last_resort: parsed.unique || parsed.stable,
        reverse: parsed.reverse,
        random_seed,
    };

    let sources = InputSource::from_args(&parsed.files);
    let mut bufs: Vec<Vec<u8>> = Vec::with_capacity(sources.len());
    for src in &sources {
        match FastReader::open(src).and_then(|r| r.into_bytes()) {
            Ok(b) => {
                ctx.telemetry.add_read(b.len() as u64);
                bufs.push(b);
            }
            Err(e) => {
                eprintln!(
                    "sort: cannot read: {}: {}",
                    src.display_name(),
                    gnu_io_error(&e)
                );
                return Ok(2);
            }
        }
    }

    // Check if input is an NDJSON object stream!
    if !parsed.check
        && bufs
            .iter()
            .find(|b| !b.is_empty())
            .is_some_and(|b| looks_like_json(b))
    {
        let all_lines: Vec<String> = bufs.iter().flat_map(|b| lossy_lines(b)).collect();
        if let Some(mut objs) = read_ndjson_stdin(&all_lines) {
            ctx.telemetry
                .set_strategy("NDJSON field-aware parallel sort");
            if let Some(ref expr) = parsed.where_expr {
                objs.retain(|o| eval_json_where(o, expr));
            }
            let field = parsed
                .by_field
                .clone()
                .unwrap_or_else(|| "name".to_string());
            objs.par_sort_by(|a, b| {
                let ord = compare_json_field(a, b, &field);
                if parsed.reverse { ord.reverse() } else { ord }
            });
            if parsed.unique {
                objs.dedup_by(|a, b| compare_json_field(a, b, &field) == Ordering::Equal);
            }
            ctx.telemetry.add_items(objs.len() as u64);
            emit_objects_or_table(&objs, ctx, parsed.json, !parsed.json)?;
            return Ok(0);
        }
    }

    let mut lines: Vec<&[u8]> = bufs.iter().flat_map(|b| split_lines(b)).collect();

    if parsed.check {
        let name = sources[0].display_name();
        for (idx, pair) in lines.windows(2).enumerate() {
            let ord = sorter.compare(pair[0], pair[1]);
            if ord == Ordering::Greater || (parsed.unique && ord == Ordering::Equal) {
                if !parsed.check_quiet {
                    eprintln!(
                        "sort: {}:{}: disorder: {}",
                        name,
                        idx + 2,
                        String::from_utf8_lossy(pair[1])
                    );
                }
                return Ok(1);
            }
        }
        return Ok(0);
    }

    if sorter.keys.is_empty() {
        ctx.telemetry
            .set_strategy("zero-alloc &[u8] slices + rayon par_sort_unstable");
        if parsed.reverse {
            lines.par_sort_unstable_by(|a, b| b.cmp(a));
        } else {
            lines.par_sort_unstable();
        }
    } else {
        ctx.telemetry
            .set_strategy("zero-alloc &[u8] slices + rayon stable par_sort_by (GNU keys)");
        lines.par_sort_by(|a, b| sorter.compare(a, b));
    }
    if parsed.unique {
        lines.dedup_by(|cur, prev| sorter.compare(prev, cur) == Ordering::Equal);
    }

    let to_stdout = parsed.output.as_deref().is_none_or(|o| o == "-");
    let sink: Box<dyn Write> = match parsed.output.as_deref() {
        Some(path) if path != "-" => match std::fs::File::create(path) {
            Ok(f) => Box::new(f),
            Err(e) => {
                eprintln!("sort: open failed: {}: {}", path, gnu_io_error(&e));
                return Ok(2);
            }
        },
        _ => Box::new(stdout::stdout()),
    };
    let mut out = io::BufWriter::with_capacity(256 * 1024, sink);
    let mut written = 0u64;
    // GNU names a failed write to standard output, then exits with its
    // `close_stdout` diagnostic (`sort: write error`).
    let stdout_failed = |what: &str, e: &io::Error| {
        let failed = to_stdout && stdout::is_write_error(e);
        if failed {
            eprintln!("sort: {what}: 'standard output': {}", gnu_io_error(e));
        }
        failed
    };
    for s in &lines {
        if let Err(e) = out.write_all(s).and_then(|()| out.write_all(b"\n")) {
            if stdout_failed("write failed", &e) {
                return Ok(2);
            }
            ignore_broken_pipe(Err(e))?;
        }
        written += s.len() as u64 + 1;
    }
    ctx.telemetry.add_items(lines.len() as u64);
    ctx.telemetry.add_written(written);
    if let Err(e) = out.flush() {
        if stdout_failed("fflush failed", &e) {
            return Ok(2);
        }
        ignore_broken_pipe(Err(e))?;
    }
    Ok(0)
}
