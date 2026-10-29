//! sort — GNU key/ordering semantics, a parallel in-memory sort, GNU's
//! external sort and k-way merge (`-m`, `-S`, `--batch-size`,
//! `--compress-program`), and NDJSON `--by <field>`.

mod compare;
mod debug;
mod ldbl;
mod merge;

use std::cmp::Ordering;
use std::ffi::{CString, OsStr, OsString};
use std::hash::{BuildHasher, RandomState};
use std::io::{self, Read};
use std::os::fd::IntoRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;

use anyhow::Result;
use digest::Digest;
use dogfish_core::{
    DogfishContext, FastReader, emit_objects_or_table, eval_json_where, gnu_io_error,
    read_ndjson_stdin, table::compare_json_field,
};
use md5::Md5;
use rayon::prelude::*;

use crate::common::files0::{Files0From, extra_operand_error};
use crate::common::getopt::{
    Arg, Getopt, HasArg, OptSpec, Order, argmatch, getopt_error, lossy, print_help, print_version,
};
use crate::common::input::{close_stdin, stdin_file, stdin_is_closed};
use crate::common::lines::{line_end, looks_like_json, lossy_lines, split_records};
use crate::common::posixver::posix2_version;
use crate::common::quote::{qa, quote_locale};

use compare::{Blanks, NONE, SortKey, SortNum, SortOpts, Sorter, parse_sort_num};
use merge::{Die, Env, Res, Source, die, sort_die};

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
      --batch-size <NMERGE>          Merge at most NMERGE inputs at once; for more use temp files
  -c, --check[=diagnose-first]       Check for sorted input; do not sort
  -C, --check=quiet, --check=silent  Like -c, but do not report first bad line
      --compress-program <PROG>      Compress temporaries with PROG; decompress them with PROG -d
      --debug                        Annotate the part of the line used to sort, and warn about
                                     questionable usage to stderr
      --files0-from <F>              Read input from the files specified by NUL-terminated
                                     names in file F; if F is - then read names from
                                     standard input
  -k, --key <KEYS>                   Sort via a key; KEYDEF gives location and type
                                     (F[.C][OPTS][,F[.C][OPTS]])
  -m, --merge                        Merge already sorted files; do not sort
  -o, --output <OUTPUT>              Write result to FILE instead of standard output
  -s, --stable                       Stabilize sort by disabling last-resort comparison
  -S, --buffer-size <SIZE>           Use SIZE for main memory buffer
  -t, --field-separator <SEPARATOR>  Use SEP instead of non-blank to blank transition
  -T, --temporary-directory <DIR>    Use DIR for temporaries, not $TMPDIR or /tmp;
                                     multiple options specify multiple directories
      --parallel <N>                 Change the number of sorts run concurrently to N
  -u, --unique                       Output only the first of an equal run
  -z, --zero-terminated              Line delimiter is NUL, not newline
      --by <BY_FIELD>                Modern Dogfish extension: Sort NDJSON objects by named field
                                     (e.g., `--by size`)
  -w, --where <WHERE_EXPR>           Filter NDJSON objects before sorting (e.g., `--where
                                     \"size>10K\"`)
  -J, --json                         Emit JSON / NDJSON output
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortOpt {
    /// An ordering letter (`-b`, `-n`, ...).
    Ordering(u8),
    Check,
    CheckQuiet,
    CheckWhen,
    CompressProgram,
    Debug,
    Files0From,
    Key,
    Merge,
    BatchSize,
    Output,
    RandomSource,
    Sort,
    Stable,
    BufferSize,
    Separator,
    TempDir,
    Parallel,
    Unique,
    /// `-y`: accepted and ignored (Solaris).
    Y,
    ZeroTerminated,
    By,
    Where,
    Json,
    Help,
    PrintVersion,
}

/// GNU's `long_options` (in its order: it decides ambiguity diagnostics) and
/// `short_options`, plus the Dogfish `--desc`, `--by`, `-w, --where` and
/// `-J, --json`.
const SPECS: &[OptSpec<SortOpt>] = &[
    OptSpec::both(
        b'b',
        "ignore-leading-blanks",
        HasArg::No,
        SortOpt::Ordering(b'b'),
    ),
    OptSpec::long("check", HasArg::Optional, SortOpt::CheckWhen),
    OptSpec::long(
        "compress-program",
        HasArg::Required,
        SortOpt::CompressProgram,
    ),
    OptSpec::long("debug", HasArg::No, SortOpt::Debug),
    OptSpec::both(
        b'd',
        "dictionary-order",
        HasArg::No,
        SortOpt::Ordering(b'd'),
    ),
    OptSpec::both(b'f', "ignore-case", HasArg::No, SortOpt::Ordering(b'f')),
    OptSpec::long("files0-from", HasArg::Required, SortOpt::Files0From),
    OptSpec::both(
        b'g',
        "general-numeric-sort",
        HasArg::No,
        SortOpt::Ordering(b'g'),
    ),
    OptSpec::both(
        b'i',
        "ignore-nonprinting",
        HasArg::No,
        SortOpt::Ordering(b'i'),
    ),
    OptSpec::both(b'k', "key", HasArg::Required, SortOpt::Key),
    OptSpec::both(b'm', "merge", HasArg::No, SortOpt::Merge),
    OptSpec::both(b'M', "month-sort", HasArg::No, SortOpt::Ordering(b'M')),
    OptSpec::both(b'n', "numeric-sort", HasArg::No, SortOpt::Ordering(b'n')),
    OptSpec::both(
        b'h',
        "human-numeric-sort",
        HasArg::No,
        SortOpt::Ordering(b'h'),
    ),
    OptSpec::both(b'V', "version-sort", HasArg::No, SortOpt::Ordering(b'V')),
    OptSpec::both(b'R', "random-sort", HasArg::No, SortOpt::Ordering(b'R')),
    OptSpec::long("random-source", HasArg::Required, SortOpt::RandomSource),
    OptSpec::long("sort", HasArg::Required, SortOpt::Sort),
    OptSpec::both(b'o', "output", HasArg::Required, SortOpt::Output),
    OptSpec::both(b'r', "reverse", HasArg::No, SortOpt::Ordering(b'r')),
    OptSpec::both(b's', "stable", HasArg::No, SortOpt::Stable),
    OptSpec::long("batch-size", HasArg::Required, SortOpt::BatchSize),
    OptSpec::both(b'S', "buffer-size", HasArg::Required, SortOpt::BufferSize),
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
        SortOpt::TempDir,
    ),
    OptSpec::both(b'u', "unique", HasArg::No, SortOpt::Unique),
    OptSpec::both(b'z', "zero-terminated", HasArg::No, SortOpt::ZeroTerminated),
    OptSpec::long("parallel", HasArg::Required, SortOpt::Parallel),
    OptSpec::long("help", HasArg::No, SortOpt::Help),
    OptSpec::long("version", HasArg::No, SortOpt::PrintVersion),
    OptSpec::long("desc", HasArg::No, SortOpt::Ordering(b'r')).exact(),
    OptSpec::long("by", HasArg::Required, SortOpt::By).exact(),
    OptSpec::both(b'w', "where", HasArg::Required, SortOpt::Where).exact(),
    OptSpec::both(b'J', "json", HasArg::No, SortOpt::Json).exact(),
    OptSpec::short(b'c', HasArg::No, SortOpt::Check),
    OptSpec::short(b'C', HasArg::No, SortOpt::CheckQuiet),
    OptSpec::short(b'y', HasArg::Required, SortOpt::Y),
];

/// GNU `NMERGE_DEFAULT`.
const NMERGE_DEFAULT: usize = 16;
/// GNU `MIN_MERGE_BUFFER_SIZE`: two bytes and a `struct line`.
const MIN_MERGE_BUFFER_SIZE: usize = 2 + 32;

/// What the command line asks for.
struct SortArgs {
    gkey: SortOpts,
    keys: Vec<SortKey>,
    tab: Option<u8>,
    checkonly: Option<u8>,
    merge: bool,
    debug: bool,
    stable: bool,
    unique: bool,
    zero_terminated: bool,
    output: Option<OsString>,
    random_source: Option<OsString>,
    compress: Option<OsString>,
    temp_dirs: Vec<OsString>,
    nmerge: usize,
    sort_size: usize,
    nthreads: usize,
    files0_from: Option<String>,
    files: Vec<OsString>,
    by_field: Option<String>,
    where_expr: Option<String>,
    json: bool,
}

/// GNU `badfieldspec`.
fn badfieldspec(spec: &OsStr, msgid: &str) -> Die {
    die(format_args!(
        "{msgid}: invalid field specification {}",
        quote_locale(spec)
    ))
}

/// GNU `parse_field_count`: the leading count of `s` (saturated) and the
/// rest. On failure `None`, after reporting `msgid` if there is one.
fn parse_field_count<'s>(s: &'s [u8], msgid: Option<&str>) -> Res<Option<(usize, &'s [u8])>> {
    let start = s
        .iter()
        .take_while(|c| matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
        .count();
    let mut i = start;
    if s.get(i) == Some(&b'+') {
        i += 1;
    }
    let digits = s[i..].iter().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 || s.get(start) == Some(&b'-') {
        if let Some(msgid) = msgid {
            return Err(die(format_args!(
                "{msgid}: invalid count at start of {}",
                quote_locale(OsStr::from_bytes(s))
            )));
        }
        return Ok(None);
    }
    let n = s[i..i + digits].iter().fold(0usize, |n, &d| {
        n.checked_mul(10)
            .and_then(|n| n.checked_add(usize::from(d - b'0')))
            .unwrap_or(usize::MAX)
    });
    Ok(Some((n, &s[i + digits..])))
}

/// A `-k POS1[,POS2]` key, as GNU's option loop parses it.
fn parse_key(spec: &OsStr) -> Res<SortKey> {
    let mut key = SortKey::whole_line(SortOpts::default());
    let field = |s, msgid| -> Res<(usize, &[u8])> {
        Ok(parse_field_count(s, Some(msgid))?.unwrap_or((0, &[])))
    };
    let (sword, mut s) = field(spec.as_bytes(), "invalid number at field start")?;
    if sword == 0 {
        return Err(badfieldspec(spec, "field number is zero"));
    }
    key.sword = sword - 1;
    if let [b'.', rest @ ..] = s {
        let (schar, r) = field(rest, "invalid number after '.'")?;
        if schar == 0 {
            return Err(badfieldspec(spec, "character offset is zero"));
        }
        key.schar = schar - 1;
        s = r;
    }
    if key.sword == 0 && key.schar == 0 {
        key.sword = NONE;
    }
    s = &s[key.opts.set_ordering(s, Blanks::Start)..];
    if let [b',', rest @ ..] = s {
        let (eword, r) = field(rest, "invalid number after ','")?;
        if eword == 0 {
            return Err(badfieldspec(spec, "field number is zero"));
        }
        key.eword = eword - 1;
        s = r;
        if let [b'.', rest @ ..] = s {
            let (echar, r) = field(rest, "invalid number after '.'")?;
            key.echar = echar;
            s = r;
        }
        s = &s[key.opts.set_ordering(s, Blanks::End)..];
    }
    if !s.is_empty() {
        return Err(badfieldspec(spec, "stray character in field spec"));
    }
    Ok(key)
}

/// The obsolete `+POS1` key, or `None` when `pos1` (after the `+`) is no
/// valid position: GNU then takes the operand for a file.
fn obsolete_start(pos1: &[u8]) -> Res<Option<SortKey>> {
    let mut key = SortKey::whole_line(SortOpts::default());
    let Some((sword, mut s)) = parse_field_count(pos1, None)? else {
        return Ok(None);
    };
    key.sword = sword;
    if let [b'.', rest @ ..] = s {
        let Some((schar, r)) = parse_field_count(rest, None)? else {
            return Ok(None);
        };
        key.schar = schar;
        s = r;
    }
    if key.sword == 0 && key.schar == 0 {
        key.sword = NONE;
    }
    if key.opts.set_ordering(s, Blanks::Start) != s.len() {
        return Ok(None);
    }
    key.traditional = true;
    Ok(Some(key))
}

/// The obsolete `-POS2` that ends `key`: `arg` is the whole operand.
fn obsolete_end(key: &mut SortKey, arg: &OsStr) -> Res<()> {
    let bytes = &arg.as_bytes()[1..];
    let (eword, mut s) =
        parse_field_count(bytes, Some("invalid number after '-'"))?.unwrap_or((0, &[]));
    key.eword = eword;
    if let [b'.', rest @ ..] = s {
        let (echar, r) =
            parse_field_count(rest, Some("invalid number after '.'"))?.unwrap_or((0, &[]));
        key.echar = echar;
        s = r;
    }
    // +A.x -B.y is -k A+1.x+1,B.y when y is 0, else -k A+1.x+1,B+1.y.
    if key.echar == 0 && key.eword != 0 {
        key.eword -= 1;
    }
    if key.opts.set_ordering(s, Blanks::End) != s.len() {
        return Err(badfieldspec(arg, "stray character in field spec"));
    }
    Ok(())
}

/// gnulib `xstrtol_fatal` for sort's numeric options.
fn xstrtol_fatal(kind: StrtolFail, option: &str, arg: &OsStr) -> Die {
    let arg = arg.to_string_lossy();
    match kind {
        StrtolFail::Invalid => die(format_args!("invalid {option} argument '{arg}'")),
        StrtolFail::Suffix => die(format_args!("invalid suffix in {option} argument '{arg}'")),
        StrtolFail::Overflow => die(format_args!("{option} argument '{arg}' too large")),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StrtolFail {
    Invalid,
    Suffix,
    Overflow,
}

/// gnulib `xstrtoumax (s, &end, 10, &n, valid_suffixes)` without the `B`
/// and `iB` suffixes: the value (saturated), the error, and `end`: past a
/// valid suffix, or at the first character not understood.
fn xstrtoumax(s: &[u8], valid_suffixes: &[u8]) -> (u64, Option<StrtolFail>, usize) {
    let start = s
        .iter()
        .take_while(|c| matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
        .count();
    if s.get(start) == Some(&b'-') {
        return (0, Some(StrtolFail::Invalid), 0);
    }
    let mut i = start + usize::from(s.get(start) == Some(&b'+'));
    let digits = s[i..].iter().take_while(|c| c.is_ascii_digit()).count();
    let mut overflow = false;
    let mut n: u64 = 0;
    if digits == 0 {
        match s.first() {
            Some(c) if valid_suffixes.contains(c) => {
                n = 1;
                i = 0;
            }
            _ => return (0, Some(StrtolFail::Invalid), 0),
        }
    } else {
        for &d in &s[i..i + digits] {
            match n
                .checked_mul(10)
                .and_then(|n| n.checked_add(u64::from(d - b'0')))
            {
                Some(v) => n = v,
                None => {
                    overflow = true;
                    n = u64::MAX;
                }
            }
        }
        i += digits;
    }
    let fail = |overflow: bool, suffix: bool| match (overflow, suffix) {
        (_, true) => Some(StrtolFail::Suffix),
        (true, false) => Some(StrtolFail::Overflow),
        _ => None,
    };
    let Some(&c) = s.get(i) else {
        return (n, fail(overflow, false), i);
    };
    if !valid_suffixes.contains(&c) {
        return (n, fail(overflow, true), i);
    }
    let power = match c {
        b'k' | b'K' => 1,
        b'm' | b'M' => 2,
        b'g' | b'G' => 3,
        b't' | b'T' => 4,
        b'P' => 5,
        b'E' => 6,
        b'Z' => 7,
        b'Y' => 8,
        b'R' => 9,
        b'Q' => 10,
        _ => 0,
    };
    for _ in 0..power {
        match n.checked_mul(1024) {
            Some(v) => n = v,
            None => {
                overflow = true;
                n = u64::MAX;
            }
        }
    }
    (n, fail(overflow, i + 1 != s.len()), i + 1)
}

/// Total physical memory (gnulib `physmem_total`).
fn physmem_total() -> f64 {
    // SAFETY: sysconf only queries system values.
    let (pages, size) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    if pages > 0 && size > 0 {
        pages as f64 * size as f64
    } else {
        64.0 * 1024.0 * 1024.0
    }
}

/// GNU `specify_sort_size`: `-S SIZE`, in KiB unless suffixed (`b` for
/// bytes, `%` of physical memory); several give the largest.
fn specify_sort_size(p: &mut SortArgs, option: &str, arg: &OsStr) -> Res<()> {
    let s = arg.as_bytes();
    let (mut n, mut e, suffix) = xstrtoumax(s, b"EgGkKmMPQRtTYZ");
    let digit_before = suffix > 0 && s[suffix - 1].is_ascii_digit();
    // The default unit is KiB.
    if e.is_none() && digit_before {
        match n.checked_mul(1024) {
            Some(v) => n = v,
            None => e = Some(StrtolFail::Overflow),
        }
    }
    if e == Some(StrtolFail::Suffix) && digit_before && suffix + 1 == s.len() {
        match s[suffix] {
            b'b' => e = None,
            b'%' => {
                let mem = physmem_total() * n as f64 / 100.0;
                if mem < u64::MAX as f64 {
                    n = mem as u64;
                    e = None;
                } else {
                    e = Some(StrtolFail::Overflow);
                }
            }
            _ => {}
        }
    }
    match e {
        None => {
            let n =
                usize::try_from(n).map_err(|_| xstrtol_fatal(StrtolFail::Overflow, option, arg))?;
            if n >= p.sort_size {
                p.sort_size = n.max(p.nmerge * MIN_MERGE_BUFFER_SIZE);
            }
            Ok(())
        }
        Some(kind) => Err(xstrtol_fatal(kind, option, arg)),
    }
}

/// GNU `specify_nmerge`: `--batch-size`, at least 2 and at most what the
/// file-descriptor limit allows.
fn specify_nmerge(arg: &OsStr) -> Res<usize> {
    let (n, e, _) = xstrtoumax(arg.as_bytes(), b"");
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: out-pointer: getrlimit fills the live local struct.
    let max_nmerge = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
        limit.rlim_cur as u32
    } else {
        1024
    }
    .wrapping_sub(3);
    let too_large = || {
        eprintln!(
            "sort: --batch-size argument {} too large",
            quote_locale(arg)
        );
        die(format_args!(
            "maximum --batch-size argument with current rlimit is {max_nmerge}"
        ))
    };
    match e {
        None => {
            let Ok(n) = u32::try_from(n) else {
                return Err(too_large());
            };
            if n < 2 {
                eprintln!("sort: invalid --batch-size argument {}", quote_locale(arg));
                return Err(die("minimum --batch-size argument is '2'"));
            }
            if max_nmerge < n {
                return Err(too_large());
            }
            Ok(n as usize)
        }
        Some(StrtolFail::Overflow) => Err(too_large()),
        Some(kind) => Err(xstrtol_fatal(kind, "--batch-size", arg)),
    }
}

/// GNU `specify_nthreads`: `--parallel=N`, N > 0.
fn specify_nthreads(arg: &OsStr) -> Res<usize> {
    match xstrtoumax(arg.as_bytes(), b"") {
        (_, Some(StrtolFail::Overflow), _) => Ok(usize::MAX),
        (_, Some(kind), _) => Err(xstrtol_fatal(kind, "--parallel", arg)),
        (0, None, _) => Err(die("number in parallel must be nonzero")),
        (n, None, _) => Ok(usize::try_from(n).unwrap_or(usize::MAX)),
    }
}

/// Parses the command line as GNU's option loop does, reporting errors as
/// they come; `Err` is the exit status after `--help`, `--version` or a
/// diagnostic.
fn parse_sort_args(args: Vec<OsString>) -> std::result::Result<SortArgs, i32> {
    use SortOpt::*;
    let mut p = SortArgs {
        gkey: SortOpts::default(),
        keys: Vec::new(),
        tab: None,
        checkonly: None,
        merge: false,
        debug: false,
        stable: false,
        unique: false,
        zero_terminated: false,
        output: None,
        random_source: None,
        compress: None,
        temp_dirs: Vec::new(),
        nmerge: NMERGE_DEFAULT,
        sort_size: 0,
        nthreads: 0,
        files0_from: None,
        files: Vec::new(),
        by_field: None,
        where_expr: None,
        json: false,
    };
    let mut opts = Getopt::with_order(args.into_iter().skip(1).collect(), SPECS, Order::InPlace);
    let posixly_correct = std::env::var_os("POSIXLY_CORRECT").is_some();
    let posix_ver = posix2_version();
    let mut traditional_usage = !(200112..200809).contains(&posix_ver);
    let fail = |d: Die| d.0;
    loop {
        // POSIX: after a file operand, everything is a file, except (in
        // traditional usage) `-o FILE` before the last operand.
        if posixly_correct
            && !p.files.is_empty()
            && let Some(next) = opts.take_next()
        {
            let b = next.as_bytes();
            let keep_parsing = traditional_usage
                && p.checkonly.is_none()
                && b.starts_with(b"-o")
                && (b.len() > 2 || opts.peek().is_some());
            if !keep_parsing {
                p.files.push(next);
                continue;
            }
            opts.splice_front(vec![next]);
        }
        let Some(item) = opts.next_in_order() else {
            break;
        };
        let (o, v) = match item.map_err(|e| getopt_error("sort", &e))? {
            Arg::Operand(file) => {
                // GNU's obsolete `+POS1 [-POS2]` keys.
                if let Some(pos1) = file.as_bytes().strip_prefix(b"+") {
                    let minus_pos = opts.peek().is_some_and(|n| {
                        let n = n.as_bytes();
                        n.len() > 1 && n[0] == b'-' && n[1].is_ascii_digit()
                    });
                    traditional_usage |= minus_pos && !posixly_correct;
                    if traditional_usage
                        && let Some(mut key) = obsolete_start(pos1).map_err(fail)?
                    {
                        if minus_pos && let Some(pos2) = opts.take_next() {
                            obsolete_end(&mut key, &pos2).map_err(fail)?;
                        }
                        p.keys.push(key);
                        continue;
                    }
                }
                p.files.push(file);
                continue;
            }
            Arg::Opt(o, v) => (o, v.unwrap_or_default()),
        };
        match o {
            Ordering(c) => {
                p.gkey.set_ordering(&[c], Blanks::Both);
            }
            Sort => {
                let c = argmatch(
                    "sort",
                    "--sort",
                    &v,
                    &[
                        ("general-numeric", b'g'),
                        ("human-numeric", b'h'),
                        ("month", b'M'),
                        ("numeric", b'n'),
                        ("random", b'R'),
                        ("version", b'V'),
                    ],
                )?;
                p.gkey.set_ordering(&[c], Blanks::Both);
            }
            Check | CheckQuiet | CheckWhen => {
                let c = match o {
                    CheckQuiet => b'C',
                    CheckWhen
                        if !v.is_empty()
                            || opts
                                .last_word()
                                .is_some_and(|w| w.as_bytes().contains(&b'=')) =>
                    {
                        argmatch(
                            "sort",
                            "--check",
                            &v,
                            &[("quiet", b'C'), ("silent", b'C'), ("diagnose-first", b'c')],
                        )?
                    }
                    _ => b'c',
                };
                if p.checkonly.is_some_and(|k| k != c) {
                    return Err(fail(die("options '-cC' are incompatible")));
                }
                p.checkonly = Some(c);
            }
            CompressProgram => {
                if p.compress.as_ref().is_some_and(|c| *c != v) {
                    return Err(fail(die("multiple compress programs specified")));
                }
                p.compress = Some(v);
            }
            Debug => p.debug = true,
            Files0From => p.files0_from = Some(lossy(&v)),
            Key => p.keys.push(parse_key(&v).map_err(fail)?),
            Merge => p.merge = true,
            BatchSize => p.nmerge = specify_nmerge(&v).map_err(fail)?,
            Output => {
                if p.output.as_ref().is_some_and(|o| *o != v) {
                    return Err(fail(die("multiple output files specified")));
                }
                p.output = Some(v);
            }
            RandomSource => {
                if p.random_source.as_ref().is_some_and(|r| *r != v) {
                    return Err(fail(die("multiple random sources specified")));
                }
                p.random_source = Some(v);
            }
            Stable => p.stable = true,
            BufferSize => {
                let spelled = opts.last_spelling();
                specify_sort_size(&mut p, &spelled, &v).map_err(fail)?;
            }
            Separator => {
                let b = v.as_bytes();
                let newtab = match b {
                    [] => return Err(fail(die("empty tab"))),
                    [c] => *c,
                    b"\\0" => 0,
                    _ => {
                        return Err(fail(die(format_args!(
                            "multi-character tab {}",
                            quote_locale(&v)
                        ))));
                    }
                };
                if p.tab.is_some_and(|t| t != newtab) {
                    return Err(fail(die("incompatible tabs")));
                }
                p.tab = Some(newtab);
            }
            TempDir => p.temp_dirs.push(v),
            Parallel => p.nthreads = specify_nthreads(&v).map_err(fail)?,
            Unique => p.unique = true,
            Y => {
                // `-y ARG` ignores an ARG of digits; any other word after
                // `-y` is an operand.
                let separate = opts
                    .last_word()
                    .is_some_and(|w| w.as_bytes().ends_with(b"y"));
                if separate && !v.as_bytes().iter().all(u8::is_ascii_digit) {
                    p.files.push(v);
                }
            }
            ZeroTerminated => p.zero_terminated = true,
            By => p.by_field = Some(lossy(&v)),
            Where => p.where_expr = Some(lossy(&v)),
            Json => p.json = true,
            Help => return Err(print_help(SORT_HELP)),
            PrintVersion => return Err(print_version("sort")),
        }
    }
    p.files.extend(opts.operands());
    Ok(p)
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    fn key(spec: &str) -> (usize, usize, usize, usize) {
        let k = parse_key(OsStr::new(spec)).ok().unwrap();
        (k.sword, k.schar, k.eword, k.echar)
    }

    #[test]
    fn keys_parse_like_gnu() {
        assert_eq!(key("1"), (NONE, 0, NONE, 0));
        assert_eq!(key("1.2"), (0, 1, NONE, 0));
        assert_eq!(key("2,3"), (1, 0, 2, 0));
        assert_eq!(key("2.3b,4.5n"), (1, 2, 3, 5));
        assert_eq!(key("99999999999999999999").0, usize::MAX - 1);
    }

    #[test]
    fn obsolete_keys() {
        let k = obsolete_start(b"1").ok().flatten().unwrap();
        assert_eq!((k.sword, k.schar), (1, 0));
        let mut k = obsolete_start(b"1.1").ok().flatten().unwrap();
        obsolete_end(&mut k, OsStr::new("-1.2")).ok().unwrap();
        assert_eq!((k.sword, k.schar, k.eword, k.echar), (1, 1, 1, 2));
        let mut k = obsolete_start(b"0").ok().flatten().unwrap();
        obsolete_end(&mut k, OsStr::new("-1")).ok().unwrap();
        assert_eq!((k.sword, k.eword, k.echar), (NONE, 0, 0));
        assert!(obsolete_start(b"x").ok().flatten().is_none());
        assert!(obsolete_start(b"").ok().flatten().is_none());
        assert!(obsolete_start(b"1.").ok().flatten().is_none());
    }

    #[test]
    fn buffer_sizes() {
        assert_eq!(xstrtoumax(b"10", b"kK"), (10, None, 2));
        assert_eq!(xstrtoumax(b"2k", b"kK"), (2048, None, 2));
        assert_eq!(xstrtoumax(b"2b", b"kK").1, Some(StrtolFail::Suffix));
        assert_eq!(xstrtoumax(b"-1", b"").1, Some(StrtolFail::Invalid));
    }
}

/// Resolves the `-R` seed: 16 bytes from `--random-source=FILE`, or fresh
/// randomness (GNU `random_md5_state_init`).
fn sort_random_seed(source: Option<&OsStr>) -> Res<[u8; 16]> {
    let mut seed = [0u8; 16];
    match source {
        Some(path) => {
            let mut file =
                std::fs::File::open(path).map_err(|e| sort_die("open failed", path, &e))?;
            file.read_exact(&mut seed).map_err(|e| match e.kind() {
                io::ErrorKind::UnexpectedEof => {
                    die(format_args!("{}: end of file", quote_locale(path)))
                }
                _ => die(format_args!(
                    "{}: read error: {}",
                    quote_locale(path),
                    gnu_io_error(&e)
                )),
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

/// The inputs named in `--files0-from=FROM` (GNU reads them all first and
/// stops at the first bad name).
fn files0_sources(from: &str) -> Res<Vec<OsString>> {
    let mut list =
        Files0From::open(from).map_err(|e| sort_die("open failed", OsStr::new(from), &e))?;
    let names = list
        .read_all()
        .map_err(|_| die(format_args!("cannot read file names from {}", qa(from))))?;
    if names.is_empty() {
        return Err(die(format_args!("no input from {}", qa(from))));
    }
    for (i, name) in names.iter().enumerate() {
        if let Some(msg) = list.check_name(name, i as u64 + 1, false) {
            return Err(die(msg));
        }
    }
    Ok(names.into_iter().map(OsString::from).collect())
}

/// GNU `check_inputs`: every input must be readable (`euidaccess`).
fn check_inputs(files: &[OsString]) -> Res<()> {
    for f in files {
        if f == "-" {
            continue;
        }
        let ok = CString::new(f.as_bytes()).is_ok_and(|c| {
            // SAFETY: a NUL-terminated path; faccessat only checks access.
            unsafe {
                libc::faccessat(libc::AT_FDCWD, c.as_ptr(), libc::R_OK, libc::AT_EACCESS) == 0
            }
        });
        if !ok {
            return Err(sort_die("cannot read", f, &io::Error::last_os_error()));
        }
    }
    Ok(())
}

/// GNU `check_output`: opens (without truncating) the `-o` file and moves
/// it onto standard output.
fn check_output(output: &OsStr) -> Res<()> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o666)
        .open(output)
        .map_err(|e| sort_die("open failed", output, &e))?;
    let fd = file.into_raw_fd();
    if fd != 1 {
        // SAFETY: descriptors only: fd is ours and open; dup2 makes 1 a copy
        // of it (not close-on-exec), then the original is closed.
        unsafe {
            libc::dup2(fd, 1);
            libc::close(fd);
        }
    }
    Ok(())
}

/// A line with its first key extracted (and for `-n`/`-g`, converted) ahead
/// of the in-memory sort.
struct Keyed<'a> {
    line: &'a [u8],
    key: &'a [u8],
    pre: Pre<'a>,
    /// The first 8 bytes of the key and of the line, big-endian and
    /// zero-padded: equal-length prefixes order like the bytes, and are
    /// compared without touching the line.
    key_prefix: u64,
    line_prefix: u64,
}

/// The first key converted ahead of the sort.
enum Pre<'a> {
    None,
    Num(SortNum<'a>),
    /// A `-g` key ([`compare::general_key`]) as its high and low halves,
    /// which order like it but keep `Keyed` small and 8-byte aligned.
    General(i64, u64),
}

/// See [`Keyed::key_prefix`].
fn prefix8(s: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    let n = s.len().min(8);
    b[..n].copy_from_slice(&s[..n]);
    u64::from_be_bytes(b)
}

/// How the in-memory sort compares first keys, decided once.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FirstKey {
    /// Converted ahead: [`Pre::Num`].
    Num,
    /// Converted ahead: [`Pre::General`].
    General,
    /// No ordering option: bytes, by [`Keyed::key_prefix`] first.
    Bytes,
    /// Anything else, through [`Sorter::key_order`].
    Other,
}

impl Sorter {
    /// [`Sorter::compare_pre`] for lines whose first key was extracted (and,
    /// for a plain `-n` or `-g` key, converted) once, before sorting.
    fn compare_keyed(&self, how: FirstKey, a: &Keyed<'_>, b: &Keyed<'_>) -> Ordering {
        let first = &self.keys[0];
        let ord = match how {
            FirstKey::Num => match (&a.pre, &b.pre) {
                (Pre::Num(x), Pre::Num(y)) => compare::num_order(x, y),
                _ => Ordering::Equal,
            },
            FirstKey::General => match (&a.pre, &b.pre) {
                (Pre::General(xh, xl), Pre::General(yh, yl)) => (xh, xl).cmp(&(yh, yl)),
                _ => Ordering::Equal,
            },
            // Its first 8 bytes decide most comparisons.
            FirstKey::Bytes => a
                .key_prefix
                .cmp(&b.key_prefix)
                .then_with(|| a.key.cmp(b.key)),
            FirstKey::Other => self.key_order(first, a.key, b.key),
        };
        if ord != Ordering::Equal {
            // key_order reverses itself.
            return if first.opts.reverse && how != FirstKey::Other {
                ord.reverse()
            } else {
                ord
            };
        }
        if self.keys.len() > 1 {
            let ord = self.keycompare_from(1, a.line, b.line);
            if ord != Ordering::Equal {
                return ord;
            }
        }
        if self.no_last_resort {
            return Ordering::Equal;
        }
        let ord = a
            .line_prefix
            .cmp(&b.line_prefix)
            .then_with(|| a.line.cmp(b.line));
        if self.reverse { ord.reverse() } else { ord }
    }
}

/// Sorts `lines` in memory (GNU `sortlines`), keeping the first of each
/// run of equal lines with `-u`; on `nthreads` threads (`--parallel`) when
/// that is fewer than rayon's pool has.
fn sort_lines(sorter: &Sorter, lines: &mut Vec<&[u8]>, unique: bool, nthreads: usize) {
    if nthreads > 0
        && nthreads < rayon::current_num_threads()
        && let Ok(pool) = rayon::ThreadPoolBuilder::new()
            .num_threads(nthreads)
            .build()
    {
        return pool.install(|| sort_lines_now(sorter, lines, unique));
    }
    sort_lines_now(sorter, lines, unique);
}

fn sort_lines_now(sorter: &Sorter, lines: &mut Vec<&[u8]>, unique: bool) {
    if sorter.keys.is_empty() {
        if sorter.reverse {
            lines.par_sort_unstable_by(|a, b| b.cmp(a));
        } else {
            lines.par_sort_unstable();
        }
        if unique {
            lines.dedup();
        }
        return;
    }
    // Extract (and convert) the first key once per line rather than in
    // every comparison.
    let first = &sorter.keys[0];
    let plain = !first.opts.filters();
    let numeric = plain && first.opts.numeric;
    let general = plain && first.opts.general && !first.opts.numeric;
    let mut keyed: Vec<Keyed> = lines
        .par_iter()
        .map(|&line| {
            let key = sorter.key_slice(line, first);
            Keyed {
                line,
                key,
                pre: if numeric {
                    Pre::Num(parse_sort_num(key))
                } else if general {
                    let g = compare::general_key(key);
                    Pre::General((g >> 64) as i64, g as u64)
                } else {
                    Pre::None
                },
                key_prefix: prefix8(key),
                line_prefix: prefix8(line),
            }
        })
        .collect();
    let how = if numeric {
        FirstKey::Num
    } else if general {
        FirstKey::General
    } else if first.opts.is_default() {
        FirstKey::Bytes
    } else {
        FirstKey::Other
    };
    // With the last-resort comparison only identical lines tie, so an
    // unstable sort gives the same output, faster.
    if sorter.no_last_resort {
        keyed.par_sort_by(|a, b| sorter.compare_keyed(how, a, b));
    } else {
        keyed.par_sort_unstable_by(|a, b| sorter.compare_keyed(how, a, b));
    }
    if unique {
        keyed.dedup_by(|cur, prev| sorter.compare_keyed(how, prev, cur) == Ordering::Equal);
    }
    lines.clear();
    lines.extend(keyed.into_iter().map(|k| k.line));
}

/// One input's bytes: mapped, or read into memory.
enum SortInput {
    Mapped(memmap2::Mmap),
    Owned(Vec<u8>),
}

impl std::ops::Deref for SortInput {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            SortInput::Mapped(m) => m,
            SortInput::Owned(v) => v,
        }
    }
}

/// The number of threads GNU sorts with: `--parallel`, else the
/// processors, at most 8.
fn sort_threads(nthreads: usize) -> usize {
    if nthreads > 0 {
        return nthreads;
    }
    std::thread::available_parallelism().map_or(1, |n| n.get().min(8))
}

/// The bytes GNU reserves per line in a sort buffer beyond its text: a
/// `struct line` (32 bytes) per merge level of the threads.
fn line_bytes(nthreads: usize) -> usize {
    let nthreads = sort_threads(nthreads);
    if nthreads > 1 {
        let (mut tmp, mut mult) = (1usize, 1usize);
        while tmp < nthreads {
            tmp = tmp.saturating_mul(2);
            mult += 1;
        }
        mult * 32
    } else {
        32 * 3 / 2
    }
}

/// The metadata GNU's `sort_buffer_size` looks at for an input.
fn input_metadata(f: &OsStr) -> Res<std::fs::Metadata> {
    let meta = if f == "-" {
        if stdin_is_closed() {
            Err(io::Error::from_raw_os_error(libc::EBADF))
        } else {
            stdin_file().metadata()
        }
    } else {
        std::fs::metadata(f)
    };
    meta.map_err(|e| sort_die("stat failed", f, &e))
}

/// GNU `sort_buffer_size` with `-S`: room for the inputs in the worst case
/// (every byte a newline), but no more than `-S` (all of it for an input of
/// unknown size).
fn sort_buffer_size(files: &[OsString], sort_size: usize, line_bytes: usize) -> Res<usize> {
    let worst_case_per_input_byte = line_bytes + 1;
    let mut size = worst_case_per_input_byte + 1;
    for f in files {
        let meta = input_metadata(f)?;
        if !meta.is_file() {
            return Ok(sort_size);
        }
        let file_size = usize::try_from(meta.len()).unwrap_or(usize::MAX);
        let worst = file_size
            .checked_mul(worst_case_per_input_byte)
            .and_then(|w| w.checked_add(1));
        match worst {
            Some(w) if sort_size - size.min(sort_size) > w => size += w,
            _ => return Ok(sort_size),
        }
    }
    Ok(size)
}

/// Sorts one buffer of lines and writes it to a new temporary file.
fn spill(
    env: &mut Env,
    data: &mut Vec<u8>,
    bounds: &mut Vec<(usize, usize)>,
    temps: &mut Vec<Source>,
    nthreads: usize,
) -> Res<()> {
    let mut lines: Vec<&[u8]> = bounds.iter().map(|&(s, e)| &data[s..e]).collect();
    sort_lines(env.sorter, &mut lines, env.unique, nthreads);
    let Some((id, mut w)) = env.create_temp(false)? else {
        return Err(die("cannot create temporary file"));
    };
    w.write_lines(&lines)?;
    env.close_output(w)?;
    temps.push(Source::Temp(id));
    data.clear();
    bounds.clear();
    Ok(())
}

/// GNU `sort` with `-S`: sorts the inputs a buffer at a time, writing each
/// sorted buffer to a temporary file, then merges those. Input that fits in
/// one buffer is sorted and written directly.
fn sort_external(env: &mut Env, files: &[OsString], nthreads: usize) -> Res<()> {
    let sorter = env.sorter;
    let eol = env.eol;
    let line_bytes = line_bytes(nthreads);
    let budget = sort_buffer_size(files, env.sort_size, line_bytes)?;
    let mut data: Vec<u8> = Vec::new();
    let mut bounds: Vec<(usize, usize)> = Vec::new();
    let mut temps: Vec<Source> = Vec::new();
    for f in files {
        let src = Source::File(f.clone());
        let mut r = match env.open(&src)? {
            Ok(r) => r,
            Err(e) => return Err(sort_die("open failed", f, &e)),
        };
        while r.advance(sorter, eol)? {
            let line = r.line();
            let cost = data.len() + line.len() + 1 + (bounds.len() + 1) * line_bytes;
            if !bounds.is_empty() && cost > budget {
                spill(env, &mut data, &mut bounds, &mut temps, nthreads)?;
            }
            let start = data.len();
            data.extend_from_slice(line);
            bounds.push((start, data.len()));
        }
    }
    if temps.is_empty() {
        let mut lines: Vec<&[u8]> = bounds.iter().map(|&(s, e)| &data[s..e]).collect();
        sort_lines(sorter, &mut lines, env.unique, nthreads);
        let mut w = env.open_output()?;
        w.write_lines(&lines)?;
        return env.close_output(w);
    }
    if !bounds.is_empty() {
        spill(env, &mut data, &mut bounds, &mut temps, nthreads)?;
    }
    let ntemps = temps.len();
    merge::merge(env, temps, ntemps)
}

/// The default sort: every input in memory (mapped where possible), one
/// parallel sort, then the output.
fn sort_in_memory(env: &mut Env, p: &SortArgs, ctx: &DogfishContext, ndjson: bool) -> Res<()> {
    let files = &p.files;
    let eol = env.eol;
    // A mapped input must not be the output, which is truncated before the
    // lines are written.
    let out_id = {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: descriptor only: fd 1 stays open; ManuallyDrop keeps it so.
        let fd1 = std::mem::ManuallyDrop::new(unsafe {
            <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(1)
        });
        fd1.metadata().ok().map(|m| (m.dev(), m.ino()))
    };
    let mut bufs: Vec<SortInput> = Vec::with_capacity(files.len());
    for f in files {
        let data = if f == "-" {
            env.read_stdin = true;
            input_metadata(f)?;
            let mut v = Vec::new();
            (&*stdin_file())
                .read_to_end(&mut v)
                .map(|_| SortInput::Owned(v))
        } else {
            let file = std::fs::File::open(f).map_err(|e| sort_die("open failed", f, &e))?;
            let same = {
                use std::os::unix::fs::MetadataExt;
                file.metadata()
                    .ok()
                    .is_some_and(|m| Some((m.dev(), m.ino())) == out_id)
            };
            FastReader::from_file(file).and_then(|r| match r {
                FastReader::Mapped(m) if !same => Ok(SortInput::Mapped(m)),
                r => r.into_bytes().map(SortInput::Owned),
            })
        };
        match data {
            Ok(b) => bufs.push(b),
            Err(e) => return Err(sort_die("read failed", f, &e)),
        }
    }
    let parsed = p;
    if ndjson
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
            let reverse = parsed.gkey.reverse;
            objs.par_sort_by(|a, b| {
                let ord = compare_json_field(a, b, &field);
                if reverse { ord.reverse() } else { ord }
            });
            if parsed.unique {
                objs.dedup_by(|a, b| compare_json_field(a, b, &field) == Ordering::Equal);
            }
            ctx.telemetry.add_items(objs.len() as u64);
            if let Err(e) = emit_objects_or_table(&objs, ctx, parsed.json, !parsed.json) {
                return Err(die(e));
            }
            return Ok(());
        }
    }
    ctx.telemetry
        .add_read(bufs.iter().map(|b| b.len() as u64).sum());
    let mut lines: Vec<&[u8]> = bufs.iter().flat_map(|b| split_records(b, eol)).collect();
    sort_lines(env.sorter, &mut lines, env.unique, p.nthreads);
    let mut w = env.open_output()?;
    w.write_lines(&lines)?;
    ctx.telemetry.add_items(w.lines);
    ctx.telemetry.add_written(w.bytes);
    env.close_output(w)
}

pub fn run_sort(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_sort_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    Ok(match sort_main(parsed, ctx) {
        Ok(code) => code,
        Err(Die(code)) => code,
    })
}

fn sort_main(mut p: SortArgs, ctx: &DogfishContext) -> Res<i32> {
    if let Some(from) = p.files0_from.as_deref() {
        if let Some(extra) = p.files.first() {
            return Err(Die(extra_operand_error("sort", lossy(extra), 2)));
        }
        p.files = files0_sources(from)?;
    }

    // Keys without ordering options inherit the global ones.
    for key in &mut p.keys {
        if key.opts.is_default() && !key.opts.reverse {
            key.opts = p.gkey;
        }
    }
    let gkey_only = p.keys.is_empty() && !p.gkey.is_default();
    if gkey_only {
        p.keys.push(SortKey::whole_line(p.gkey));
    }
    if let Some(key) = p.keys.iter().find(|k| !k.opts.is_compatible()) {
        let mut o = key.opts;
        (o.skipsblanks, o.skipeblanks, o.reverse) = (false, false, false);
        return Err(die(format_args!(
            "options '-{}' are incompatible",
            o.letters()
        )));
    }

    if p.debug {
        if p.checkonly.is_some() || p.output.is_some() {
            let c = p.checkonly.unwrap_or(b'o') as char;
            return Err(die(format_args!("options '-{c} --debug' are incompatible")));
        }
        debug::locale_messages();
        debug::key_warnings(&p.keys, &p.gkey, gkey_only, p.tab, p.stable || p.unique);
    }

    let mut random_seed = Md5::new();
    if p.keys.iter().any(|k| k.opts.random) {
        random_seed.update(sort_random_seed(p.random_source.as_deref())?);
    }

    if p.temp_dirs.is_empty() {
        p.temp_dirs
            .push(std::env::var_os("TMPDIR").unwrap_or_else(|| "/tmp".into()));
    }
    if p.files.is_empty() {
        p.files.push("-".into());
    }
    if p.sort_size > 0 {
        p.sort_size = p.sort_size.max(p.nmerge * MIN_MERGE_BUFFER_SIZE);
    }

    let sorter = Sorter {
        keys: p.keys.clone(),
        tab: p.tab,
        no_last_resort: p.unique || p.stable,
        reverse: p.gkey.reverse,
        random_seed,
    };
    let mut env = Env::new(&sorter, line_end(p.zero_terminated));
    env.unique = p.unique;
    env.debug = p.debug;
    env.nmerge = p.nmerge;
    env.sort_size = p.sort_size;
    env.temp_dirs = std::mem::take(&mut p.temp_dirs);
    env.output = p.output.clone();
    if let Some(prog) = p.compress.clone() {
        // A child's exit must be waited for, whatever the caller set.
        // SAFETY: restores the default action for SIGCHLD.
        unsafe { libc::signal(libc::SIGCHLD, libc::SIG_DFL) };
        env.compress = Some(prog);
    }

    let result = run(&mut env, &p, ctx);
    env.cleanup();
    let code = result?;
    if env.read_stdin
        && let Err(e) = close_stdin()
    {
        return Err(sort_die("close failed", OsStr::new("-"), &e));
    }
    Ok(code)
}

fn run(env: &mut Env, p: &SortArgs, ctx: &DogfishContext) -> Res<i32> {
    if let Some(c) = p.checkonly {
        if let Some(extra) = p.files.get(1) {
            return Err(die(format_args!(
                "extra operand {} not allowed with -{}",
                qa(extra),
                c as char
            )));
        }
        if p.output.is_some() {
            return Err(die(format_args!(
                "options '-{}o' are incompatible",
                c as char
            )));
        }
        let ordered = merge::check(env, &p.files[0], c == b'C')?;
        return Ok(if ordered { 0 } else { 1 });
    }

    check_inputs(&p.files)?;
    if let Some(out) = &p.output {
        check_output(out)?;
    }

    if p.merge {
        ctx.telemetry.set_strategy("streaming k-way merge");
        let files = p.files.iter().cloned().map(Source::File).collect();
        merge::merge(env, files, 0)?;
        return Ok(0);
    }
    if env.sort_size > 0 {
        ctx.telemetry
            .set_strategy("external sort: sorted buffers merged from temporary files");
        sort_external(env, &p.files, p.nthreads)?;
        return Ok(0);
    }
    ctx.telemetry
        .set_strategy("zero-alloc &[u8] slices + rayon parallel sort (GNU keys)");
    // Rich output: NDJSON objects are sorted by field on a terminal, with
    // -J, or when --by/--where ask for it. Piped output is GNU's.
    let ndjson = !p.zero_terminated
        && !p.debug
        && ((ctx.stdout_is_tty() && p.output.is_none())
            || p.json
            || p.by_field.is_some()
            || p.where_expr.is_some());
    sort_in_memory(env, p, ctx, ndjson)?;
    Ok(0)
}

/// sort's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("sort", SORT_HELP, SPECS)
        .choices(
            "sort",
            [
                "general-numeric",
                "human-numeric",
                "month",
                "numeric",
                "random",
                "version",
            ],
        )
        .choices("check", ["quiet", "silent", "diagnose-first"])
}

/// Entry points for the fuzz targets in `fuzz/` (only with the `fuzzing` feature).
#[cfg(feature = "fuzzing")]
#[allow(dead_code)]
pub mod fuzz {
    use super::*;

    /// `sort -k KEYDEF`, the number parsers and the command line.
    pub fn key(spec: &str, line: &[u8]) {
        let _ = parse_key(OsStr::new(spec));
        let n = parse_sort_num(line);
        assert!(n.digits() <= line.len());
        let (_, used) = ldbl::strtold_key(line);
        assert!(used <= line.len());
        let (month, end) = compare::month_of(line);
        assert!(month <= 12 && end <= line.len());
    }

    pub fn args(args: Vec<OsString>) {
        let _ = parse_sort_args(args);
    }
}
