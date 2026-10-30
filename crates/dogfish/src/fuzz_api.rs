//! What the fuzz targets in `fuzz/` call: one function per parser, taking raw
//! fuzzer bytes. Each must never panic on any input, and checks what it can
//! of the parser's contract (round trips, bounds) with assertions, which the
//! fuzzer reports as crashes.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

use crate::common;
use crate::tools;

/// Fixed, locale-independent behaviour for every target: the C locale, UTC.
fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: called once, before anything reads the environment, from the
        // fuzzer's single thread.
        unsafe {
            std::env::remove_var("LC_ALL");
            std::env::remove_var("LC_CTYPE");
            std::env::remove_var("LANG");
            std::env::remove_var("POSIXLY_CORRECT");
            std::env::set_var("TZ", "UTC0");
        }
    });
}

/// Whether `data` holds a decimal number of six or more digits. Field widths
/// that large make `printf` and `date` (like GNU's) produce that much output,
/// which only exhausts the fuzzer's memory; their parsing, overflow included,
/// is covered by unit tests.
fn has_huge_number(data: &[u8]) -> bool {
    let decimal = data
        .split(|b| !b.is_ascii_digit())
        .any(|run| run.iter().skip_while(|&&d| d == b'0').count() >= 6);
    // `printf '%*d' 0x5fffffff` too.
    let hex = data.windows(2).enumerate().any(|(i, w)| {
        (w == b"0x" || w == b"0X")
            && data[i + 2..]
                .iter()
                .take_while(|b| b.is_ascii_hexdigit())
                .skip_while(|&&d| d == b'0')
                .count()
                >= 5
    });
    decimal || hex
}

/// `data` as UTF-8 text (lossily), the way the tools see their arguments.
fn text(data: &[u8]) -> String {
    String::from_utf8_lossy(data).into_owned()
}

/// `data` split at NUL bytes into a command line, program name first.
fn argv(tool: &str, data: &[u8]) -> Vec<OsString> {
    std::iter::once(OsString::from(tool))
        .chain(
            data.split(|&b| b == 0)
                .map(|a| OsString::from_vec(a.to_vec())),
        )
        .collect()
}

/// Splits off the first byte (a selector) from the rest.
fn split_first(data: &[u8]) -> (u8, &[u8]) {
    match data.split_first() {
        Some((&b, rest)) => (b, rest),
        None => (0, &[]),
    }
}

// ---------------------------------------------------------------- getopt

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Key {
    A,
    B,
    C,
    Long,
    LongArg,
    Optional,
}

const SPECS: &[common::getopt::OptSpec<Key>] = {
    use common::getopt::{HasArg, OptSpec};
    &[
        OptSpec::both(b'a', "all", HasArg::No, Key::A),
        OptSpec::both(b'b', "block-size", HasArg::Required, Key::B),
        OptSpec::short(b'c', HasArg::Optional, Key::C),
        OptSpec::long("almost", HasArg::No, Key::Long),
        OptSpec::long("color", HasArg::Optional, Key::Optional),
        OptSpec::long("colour", HasArg::Optional, Key::Optional),
        OptSpec::long("exact-only", HasArg::Required, Key::LongArg).exact(),
    ]
};

/// The option parser, on its own (every order) and inside the command-line
/// parsers of several tools. Every operand returned is an argument given, in
/// order, and parsing ends.
pub fn getopt(data: &[u8]) {
    use common::getopt::{Getopt, Order};
    init();
    let (sel, rest) = split_first(data);
    let args = argv("tool", rest);
    match sel % 12 {
        0..=2 => {
            let order = match sel % 3 {
                0 => Order::Permute,
                1 => Order::StopAtOperand,
                _ => Order::InPlace,
            };
            let given: Vec<OsString> = args[1..].to_vec();
            let mut g = Getopt::with_order(given.clone(), SPECS, order);
            // Each step consumes an argument or a character of a cluster.
            let bound: usize = given.iter().map(|a| a.len() + 1).sum::<usize>() + 1;
            let mut steps = 0;
            while let Some(r) = if order == Order::InPlace {
                g.next_in_order().map(|r| r.map(|_| ()))
            } else {
                g.next().map(|r| r.map(|_| ()))
            } {
                steps += 1;
                assert!(steps <= bound, "getopt does not end");
                if r.is_err() {
                    break;
                }
            }
            let mut rest = given.iter();
            for op in g.operands() {
                assert!(rest.any(|g| *g == op), "operand {op:?} not given in order");
            }
        }
        3 => tools::sort::fuzz::args(argv("sort", rest)),
        4 => tools::cut::fuzz::args(argv("cut", rest)),
        5 => tools::tr::fuzz::args(argv("tr", rest)),
        6 => tools::head::fuzz::args(argv("head", rest)),
        7 => tools::tail::fuzz::args(argv("tail", rest)),
        8 => tools::nl::fuzz::args(argv("nl", rest)),
        9 => tools::uniq::fuzz::args(argv("uniq", rest)),
        10 => tools::shuf::fuzz::args(argv("shuf", rest)),
        _ => {
            let _ = common::head_tail::parse_count(&text(rest), true);
        }
    }
}

// ------------------------------------------------------------------ dates

/// gnulib's `parse_datetime` (`date -d`, `touch -d`), in UTC and in a zone
/// the input names, with the result rendered by `date +FORMAT` (the bytes
/// after the first NUL, else a format using every field); and `touch -t`.
pub fn date(data: &[u8]) {
    use common::date_format::Stamp;
    use common::parse_datetime::{TimeZone, parse_datetime2};
    init();
    if has_huge_number(data) {
        return;
    }
    let (input, fmt) = match data.iter().position(|&b| b == 0) {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (data, &b"%Y-%m-%d %H:%M:%S.%N %z %A %j %U %G-W%V-%u %s"[..]),
    };
    let now = Stamp {
        sec: 1_700_000_000,
        ns: 123_456_789,
    };
    let tz = TimeZone::from_env();
    if let Some(t) = parse_datetime2(input, now, false, &tz) {
        assert!(t.ns < 1_000_000_000, "{input:?}: ns {}", t.ns);
        tools::date::fuzz::render(t, fmt);
    }
    tools::touch::fuzz::stamp(input);
}

/// `date +FORMAT` and `stat`'s strftime: any format, any time.
pub fn strftime(data: &[u8]) {
    init();
    if has_huge_number(data) {
        return;
    }
    let (sel, fmt) = split_first(data);
    let sec = i64::from(sel) * 86_400 * 365 - 30 * 86_400 * 365;
    if let Some(lt) = common::date_format::local_time(sec) {
        let _ = common::date_format::format_time(fmt, &lt, 999_999_999, sec, sel & 1 == 1);
    }
    let _ = common::strftime::format_local_time(
        &text(fmt),
        common::strftime::Timespec {
            sec,
            nsec: 123_456_789,
        },
    );
}

// ------------------------------------------------------------------ modes

/// `chmod`/`mkdir -m`/`install -m` modes: a compiled mode only ever yields
/// mode bits.
pub fn mode(data: &[u8]) {
    init();
    let spec = text(data);
    if let Some(changes) = common::mode::mode_compile(&spec) {
        for old in [0, 0o644, 0o755, 0o7777, 0o40755 & 0o7777] {
            for dir in [false, true] {
                let (new, _) = common::mode::mode_adjust(old, dir, 0o022, &changes);
                assert_eq!(new & !0o7777, 0, "{spec:?} on {old:o}: {new:o}");
            }
        }
    }
    let _ = common::mode::strmode9(u32::from_le_bytes(
        data.iter()
            .copied()
            .chain([0; 4])
            .take(4)
            .collect::<Vec<_>>()
            .try_into()
            .unwrap_or([0; 4]),
    ));
}

// ---------------------------------------------------------------- numbers

/// Sizes, block sizes, counts, durations, intervals and C numbers: every
/// parser accepts plain decimal numbers as themselves, and the formatters
/// take whatever the parsers return.
pub fn numbers(data: &[u8]) {
    use common::human;
    init();
    let s = text(data);
    if let Ok(n) = human::parse_size_suffixed(&s) {
        for opts in [0, human::HUMAN_AUTOSCALE | human::HUMAN_SI, 0x1f] {
            let _ = human::human_readable(n, opts, 1, 1);
            let _ = human::human_readable(n, opts, 512, 1024);
        }
    }
    if let Ok((opts, block)) = human::parse_block_size(&s) {
        assert!(block > 0, "{s:?}");
        let _ = human::human_readable(u64::MAX, opts, 1, block);
    }
    let _ = common::head_tail::parse_count(&s, false);
    if let Some((_, used)) = common::strtod::strtod_prefix(&s) {
        assert!(used <= s.len() && s.is_char_boundary(used), "{s:?}: {used}");
    }
    if let Some(secs) = common::interval::parse_interval(&s) {
        let _ = common::interval::duration_from_secs(secs);
    }
    let _ = dogfish_core::parse_size_str(&s);
    let _ = dogfish_core::parse_duration_str(&s);
    tools::seq::fuzz::operand(&s);

    // Plain decimal numbers parse as themselves.
    let n = u64::from_le_bytes(
        data.iter()
            .copied()
            .chain([0; 8])
            .take(8)
            .collect::<Vec<_>>()
            .try_into()
            .unwrap_or([0; 8]),
    );
    let d = n.to_string();
    assert_eq!(human::parse_size_suffixed(&d), Ok(n));
    if n > 0 {
        assert_eq!(
            human::parse_block_size(&d).map(|(_, b)| b),
            Ok(n),
            "block size {d}"
        );
    }
    assert_eq!(dogfish_core::parse_size_str(&d), Ok(n));
}

// ----------------------------------------------------------------- printf

/// `printf FORMAT ARGUMENT...`: the format is the bytes up to the first NUL,
/// the arguments the NUL-separated rest.
pub fn printf(data: &[u8]) {
    init();
    if has_huge_number(data) {
        return;
    }
    let mut parts = data.split(|&b| b == 0);
    let fmt = parts.next().unwrap_or_default();
    let args: Vec<String> = parts.map(text).collect();
    let _ = tools::printf::fuzz::render(fmt, &args);
    let _ = common::escapes::unescape_echo_bytes(fmt, data.first() == Some(&b'0'));
}

// --------------------------------------------------------------------- tr

/// `tr` SETs.
/// The first byte picks `-c`, `-d`, `-s`, `-t`; then SET1, and SET2 after a NUL.
pub fn tr(data: &[u8]) {
    init();
    // Like GNU's, tr walks [c*n] repeats in SET1 one character at a time.
    if has_huge_number(data) {
        return;
    }
    let (flags, rest) = split_first(data);
    let mut parts = rest.splitn(2, |&b| b == 0);
    let set1 = parts.next().unwrap_or_default();
    tools::tr::fuzz::sets(set1, parts.next(), flags);
}

// ----------------------------------------------------------------- quoting

const STYLES: [common::quote::Quoting; 10] = {
    use common::quote::Quoting::*;
    [
        Literal,
        Shell,
        ShellAlways,
        ShellEscape,
        ShellEscapeAlways,
        C,
        CMaybe,
        Escape,
        Locale,
        CLocale,
    ]
};

/// Every quoting style (C locale): quoting then unquoting the way a shell
/// (shell styles) or a C compiler (the others) reads it gives the name back.
pub fn quote(data: &[u8]) {
    use common::quote::Quoting;
    init();
    for style in STYLES {
        let q = common::quote::quote_bytes_raw(data, style, b"");
        let back = match style {
            Quoting::Literal => q.clone(),
            Quoting::Shell
            | Quoting::ShellAlways
            | Quoting::ShellEscape
            | Quoting::ShellEscapeAlways => {
                shell_unquote(&q).unwrap_or_else(|| panic!("{style:?}: bad shell word {q:?}"))
            }
            Quoting::C | Quoting::CLocale => {
                c_unquote(&q, b'"').unwrap_or_else(|| panic!("{style:?}: bad C string {q:?}"))
            }
            Quoting::CMaybe if q.first() == Some(&b'"') => {
                c_unquote(&q, b'"').unwrap_or_else(|| panic!("{style:?}: bad C string {q:?}"))
            }
            Quoting::CMaybe => q.clone(),
            Quoting::Locale => {
                c_unquote(&q, b'\'').unwrap_or_else(|| panic!("{style:?}: bad quoted string {q:?}"))
            }
            Quoting::Escape => {
                c_escapes(&q, None).unwrap_or_else(|| panic!("{style:?}: bad escapes {q:?}"))
            }
        };
        assert_eq!(back, data, "{style:?} quoted {data:?} as {q:?}");
    }
    let _ = common::quote::display_width(data);
}

/// Reads one POSIX shell word (with bash's `$'...'`), or `None` if it is not
/// exactly one complete word.
pub fn shell_unquote(q: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < q.len() {
        match q[i] {
            b'\'' => {
                let end = i + 1 + q[i + 1..].iter().position(|&c| c == b'\'')?;
                out.extend_from_slice(&q[i + 1..end]);
                i = end + 1;
            }
            b'$' if q.get(i + 1) == Some(&b'\'') => {
                let (bytes, used) = ansi_c(&q[i + 2..])?;
                out.extend(bytes);
                i += 2 + used;
            }
            b'"' => {
                i += 1;
                loop {
                    match *q.get(i)? {
                        b'"' => break,
                        b'\\' if matches!(q.get(i + 1), Some(b'$' | b'`' | b'"' | b'\\')) => {
                            out.push(q[i + 1]);
                            i += 2;
                        }
                        b'$' | b'`' => return None,
                        c => {
                            out.push(c);
                            i += 1;
                        }
                    }
                }
                i += 1;
            }
            b'\\' => {
                out.push(*q.get(i + 1)?);
                i += 2;
            }
            // Unquoted, these would split the word or expand.
            b' ' | b'\t' | b'\n' | b'|' | b'&' | b';' | b'<' | b'>' | b'(' | b')' | b'`' | b'$'
            | b'*' | b'?' | b'[' => return None,
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Some(out)
}

/// The body of `$'...'` up to its closing quote: the bytes and how much of
/// `q` they took (closing quote included).
fn ansi_c(q: &[u8]) -> Option<(Vec<u8>, usize)> {
    let end = {
        let mut j = 0;
        loop {
            match *q.get(j)? {
                b'\\' => j += 2,
                b'\'' => break j,
                _ => j += 1,
            }
        }
    };
    Some((c_escapes(&q[..end], None)?, end + 1))
}

/// A C string literal quoted with `quote` (`"` or `'`).
fn c_unquote(q: &[u8], quote: u8) -> Option<Vec<u8>> {
    let body = q.strip_prefix(&[quote])?.strip_suffix(&[quote])?;
    c_escapes(body, Some(quote))
}

/// Resolves C escapes; an unescaped `quote` inside is an error.
fn c_escapes(s: &[u8], quote: Option<u8>) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if Some(c) == quote {
            return None;
        }
        if c != b'\\' {
            out.push(c);
            i += 1;
            continue;
        }
        let e = *s.get(i + 1)?;
        i += 2;
        out.push(match e {
            b'a' => 0x07,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 0x0b,
            b'0'..=b'7' => {
                let mut v = u32::from(e - b'0');
                let mut n = 1;
                while n < 3 && s.get(i).is_some_and(|d| (b'0'..=b'7').contains(d)) {
                    v = v * 8 + u32::from(s[i] - b'0');
                    i += 1;
                    n += 1;
                }
                u8::try_from(v).ok()?
            }
            other => other,
        });
    }
    Some(out)
}

// ----------------------------------------------------------- sort and cut

/// `sort -k KEYDEF` (up to the first NUL) and key number parsing (the rest).
pub fn sort_key(data: &[u8]) {
    init();
    let (spec, line) = match data.iter().position(|&b| b == 0) {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (data, data),
    };
    tools::sort::fuzz::key(&text(spec), line);
}

/// `cut -b/-c/-f LIST`.
pub fn cut_list(data: &[u8]) {
    init();
    tools::cut::fuzz::list(&text(data));
}

// ---------------------------------------------------------------- basenc

/// Every `basenc` encoding round-trips.
pub fn basenc(data: &[u8]) {
    init();
    tools::basenc::fuzz::round_trip(data);
}

// -------------------------------------------------------------- ls bits

/// `LS_COLORS`, and the version sort `ls -v` and `sort -V` use.
pub fn ls_colors(data: &[u8]) {
    init();
    tools::ls::colors::fuzz::parse(data);
    let (a, b) = data.split_at(data.len() / 2);
    tools::ls::version_sort::fuzz::cmp(a, b);
    let _ = common::fnmatch::fnmatch(a, b, data.first() == Some(&b'.'));
}

// ---------------------------------------------------------------- tables

/// The NDJSON reader and the `--where` filter: `field!=v` is always the
/// negation of `field=v`; a record's line parses back to its value, and so
/// does a projection's.
pub fn table(data: &[u8]) {
    init();
    let (first, rest) = match data.iter().position(|&b| b == b'\n') {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (data, &data[..0]),
    };
    let expr = text(first);
    let Some(records) = dogfish_core::parse_ndjson(rest) else {
        return;
    };
    let objs: Vec<&serde_json::Value> = records.iter().map(|r| &r.value).collect();
    for r in &records {
        let back: serde_json::Value = serde_json::from_str(&r.line).expect("a record's line");
        assert_eq!(back, r.value);
        let fields: Vec<&str> = expr.split(',').collect();
        let p = r.project(&fields);
        let back: serde_json::Value = serde_json::from_str(&p.line).expect("a projection's line");
        assert_eq!(back, p.value);
    }
    for o in objs.iter().copied() {
        assert!(o.is_object());
        let _ = dogfish_core::eval_json_where(o, &expr);
        if let Some(map) = o.as_object()
            && let Some((field, value)) = map.iter().next()
        {
            let ops = ['>', '<', '=', '!', '~'];
            let rhs = expr.replace(ops, "");
            if !field.contains(ops) && !field.trim().is_empty() && field.trim() == field {
                let eq = dogfish_core::eval_json_where(o, &format!("{field}={rhs}"));
                let ne = dogfish_core::eval_json_where(o, &format!("{field}!={rhs}"));
                assert_ne!(eq, ne, "{field}={rhs} on {value}");
            }
        }
        let _ = dogfish_core::table::compare_json_field(o, objs[0], &expr);
    }
}
