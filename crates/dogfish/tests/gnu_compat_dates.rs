//! Differential GNU-compatibility suite for date strings: the
//! `parse_datetime` grammar shared by `date -d`/`-f` and `touch -d`, and the
//! `date --debug` trace that shows how a string was parsed.
//!
//! Each table runs every string through `date --debug -f -` (one process per
//! implementation) under a pinned `TZ`, and compares stdout, stderr and the
//! exit status byte-for-byte. The strings name an absolute date (or are
//! invalid), so the results do not depend on the current time. A
//! deterministic generator adds thousands of mixed strings on top.

mod common;

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::OnceLock;

use common::{dogfish_bin, exec, gnu_tool};

fn fixtures() -> &'static Path {
    static DIR: OnceLock<std::path::PathBuf> = OnceLock::new();
    DIR.get_or_init(common::fresh_fixture_dir)
}

const FORMAT: &str = "+%Y-%m-%d %H:%M:%S.%N %z %Z";

/// Runs `date ARGS` under `TZ=tz` with `stdin` through both implementations.
fn run_both(tz: &str, args: &[&str], stdin: Option<&[u8]>) -> Option<(Output, Output)> {
    let gnu = gnu_tool("date")?;
    let mut ours = Command::new(dogfish_bin());
    ours.arg("date").args(args).env("TZ", tz);
    let mut theirs = Command::new(gnu);
    theirs.arg0("date").args(args).env("TZ", tz);
    Some((
        exec(ours, fixtures(), stdin, "C"),
        exec(theirs, fixtures(), stdin, "C"),
    ))
}

/// The first differing line of `a` and `b`, with a little context.
fn first_difference(a: &[u8], b: &[u8]) -> String {
    let (a, b) = (String::from_utf8_lossy(a), String::from_utf8_lossy(b));
    let (al, bl): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
    let i = (0..al.len().max(bl.len()))
        .find(|&i| al.get(i) != bl.get(i))
        .unwrap_or(0);
    let from = i.saturating_sub(8);
    format!(
        "--- dogfish (from line {from}) ---\n{}\n--- GNU ---\n{}",
        al[from.min(al.len())..(i + 3).min(al.len())].join("\n"),
        bl[from.min(bl.len())..(i + 3).min(bl.len())].join("\n")
    )
}

/// Parses each of `lines` with `date --debug -f -` under `TZ=tz` and asserts
/// that Dogfish and GNU agree on everything they print.
fn check_lines(tz: &str, lines: &[String]) {
    let mut input = lines.join("\n").into_bytes();
    input.push(b'\n');
    let Some((ours, gnu)) = run_both(tz, &["--debug", "-f", "-", FORMAT], Some(&input)) else {
        eprintln!("note: skipping — system `date` is missing or not GNU coreutils");
        return;
    };
    if ours.stdout == gnu.stdout
        && ours.stderr == gnu.stderr
        && ours.status.code() == gnu.status.code()
    {
        return;
    }
    // Name the first string that differs on its own, then show the diff.
    let culprit = lines.iter().find(|line| {
        run_both(tz, &["--debug", "-d", line, FORMAT], None)
            .is_some_and(|(a, b)| (a.stdout, a.stderr) != (b.stdout, b.stderr))
    });
    let (stream, a, b) = if ours.stdout != gnu.stdout {
        ("stdout", &ours.stdout, &gnu.stdout)
    } else {
        ("stderr", &ours.stderr, &gnu.stderr)
    };
    panic!(
        "TZ={tz}: {stream} differs (first differing string: {culprit:?})\n{}",
        first_difference(a, b)
    );
}

fn check_table(tz: &str, lines: &[&str]) {
    let lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    check_lines(tz, &lines);
}

/// Strings with a full date, in UTC: every production of the grammar.
const UTC_STRINGS: &[&str] = &[
    // ISO 8601 dates and times.
    "1997-01-19 08:17:48 +0",
    "1997-01-19T08:17:48+0",
    "2020-01-01T10:20:30Z",
    "2020-01-01T10:20:30.123456789Z",
    "2020-01-01T10:20:30,5+05:30",
    "2020-01-01T10+05",
    "2020-01-01T1:2",
    "2020-01-01 1:2:3",
    "2020-1-2",
    "0005-01-01",
    "99-12-31",
    "69-01-01 00:00",
    "68-12-31 23:59",
    "2020-01-01T",
    "2020-01-01 T",
    "2020-01-01 T -5 hours",
    "2020-01-01 T10:00",
    "2020-01-01T10:00 T",
    "2021-13-01",
    "2021-02-30",
    "2020-02-29 12:00",
    "2021-02-29",
    "2020-01-01 00:00:60",
    "2020-01-01 24:00",
    "2020-01-01 23:59:59.999999999",
    "2020-01-01 12:00:00.0000000001",
    // Other date forms.
    "02/29/1996 1 year",
    "1/2/03",
    "12/31/1999 23:59",
    "2000/01/02",
    "999/1/2",
    "20210105",
    "1230 20000101",
    "200001011230",
    "Jan 5 2021",
    "5 Jan 2021",
    "January 5, 2021 3pm",
    "17-jun-1992",
    "jun-17-1992",
    "17-JUN+1992",
    "Sep. 5 2000",
    "Sept. 5 2000",
    "sept 5 2000",
    "Wed Jan  1 12:00:00 UTC 2020",
    "Wed, 01 Jan 2020 12:00:00 +0000",
    "Sun, 90-12-11 + 3 days - 90 minutes",
    "Thu Jan 1 1970",
    "Jan 1 12:00 2001",
    "Apr 11 22:59:00 2011",
    "1 jan 2000 10:00",
    "jan 2000",
    "2020-06",
    "2020-01-01 2021-01-01",
    "2020-01-01 10:00 11:00",
    // Times, meridians.
    "2021-01-05 3:04 pm",
    "12am 2021-01-05",
    "12pm 2021-01-05",
    "2000-01-01 12:00 A.M.",
    "2000-01-01 12:00:30.5 p.m.",
    "2000-01-01 0am",
    "2000-01-01 13pm",
    "2000-01-01 1:60pm",
    "2000-01-01 10:00 am +01",
    // Zones: names, numeric, military, DST.
    "2020-01-01 12:00:00 UTC",
    "2020-01-01 14:00 EST",
    "2020-01-01 14:00 -0500",
    "2020-01-01 14:00 +05",
    "2020-01-01 14:00 +5:30",
    "2020-01-01 14:00 +2400",
    "2020-01-01 14:00 +2401",
    "2020-01-01 14:00 -24:00",
    "2000-01-01 12:00:00 utc+1",
    "2000-01-01 12:00:00 gmt-0130",
    "2000-01-01 12:00:00 UTC +1:30",
    "2000-01-01 12:00:00 PDT",
    "2000-01-01 12:00:00 BST",
    "2000-01-01 12:00 E.S.T.",
    "2000-01-01 12:00 EST DST",
    "2000-01-01 12:00 EST -1 day",
    "2000-01-01 12:00 A",
    "2000-01-01 12:00 J",
    "2000-01-01 12:00 N",
    "2000-01-01 12:00 Z",
    "2000-01-01 12:00 T",
    "2000-01-01 12:00 T +1 hour",
    "2000-01-01 12:00 T -5",
    "2000-01-01 09:00T",
    "2000-01-01 12:00 IST",
    "2000-01-01 12:00 NZDT",
    "2000-01-01 12:00 NST",
    "2000-01-01 12:00 UTC EST",
    "2000-01-01 12:00 DST",
    "2000-01-01 12:00 +01 EST",
    "1969-12-31 23:00:00 -0100",
    // Relative items.
    "1997-01-19 08:17:48 +0 now",
    "1997-01-19 08:17:48 +0 yesterday",
    "1997-01-19 08:17:48 +0 tomorrow",
    "1997-01-19 08:17:48 +0 6 years ago",
    "1997-01-19 08:17:48 +0 7 months ago",
    "1997-01-19 08:17:48 +0 8 weeks ago",
    "1997-01-19 08:17:48 +0 next second",
    "1997-01-19 08:17:48 +0 next minute",
    "1997-01-19 08:17:48 +0 next hour",
    "1997-01-19 08:17:48 +0 next day",
    "1997-01-19 08:17:48 +0 next week",
    "1997-01-19 08:17:48 +0 next month",
    "1997-01-19 08:17:48 +0 next year",
    "1997-01-19 08:17:48 +0 last fortnight",
    "1997-01-19 08:17:48 +0 this hour",
    "1997-01-19 08:17:48 +0 third day",
    "1997-01-19 08:17:48 +0 twelfth month",
    "1997-01-19 08:17:48 +0 second",
    "1997-01-19 08:17:48 +0 2 seconds",
    "1997-01-19 08:17:48 +0 -2 secs",
    "1997-01-19 08:17:48 +0 1.5 sec ago",
    "1997-01-19 08:17:48 +0 -1.25 seconds",
    "1997-01-19 08:17:48 +0 +1.999999999999 sec",
    "1997-01-19 08:17:48 +0 -0.0000000001 sec",
    "1997-01-19 08:17:48 +0 1 day hence",
    "1997-01-19 08:17:48 +0 ago",
    "1997-01-19 08:17:48 +0 1 min 2 mins 3 minutes",
    "20050101  1 day",
    "20050101 +1 day",
    "20050101 -1 week ago",
    "2020-01-31 +1 month",
    "2016-10-31 - 1 month",
    "2020-02-29 +1 year",
    "2020-01-01 1 day 2 hours",
    "2 days hence 2000-01-01",
    "2000-01-01 3 mondays",
    "2000-01-01 + 9223372036854775807 seconds",
    "2000-01-01 9223372036854775807 years",
    "2000-01-01 2147483647 days",
    "2000-01-01 -2147483648 months",
    "2000-01-01 100000000 years",
    // Days of the week (ignored with a date, but parsed and traced).
    "2020-01-01 next monday",
    "last monday 2000-01-05",
    "Sun 2000-01-05",
    "sunday, 2000-01-05",
    "tues 2000-01-05",
    "thurs 2000-01-05",
    "wednes 2000-01-05",
    "3 fri 2000-01-05",
    "eleventh sat 2000-01-05",
    "mon tue 2000-01-05",
    // Comments, signs, odd spacing and punctuation.
    "2000-01-01 (comment) 12:00",
    "2000-01-01 (a (nested) comment) 12:00",
    "2000-01-01 (unterminated",
    "  2000-01-01\t12:00\x0b",
    "2000-01-01 - - 12:00",
    "2000-01-01 + 1 day",
    "2000-01-01 +",
    "2000-01-01 ,",
    "2000-01-01 12:",
    "2000-01-01 12:00:",
    "2000-01-01 1/",
    "2000-01-01 @5",
    "2000-01-01 noon",
    "2000-01-01 it's",
    "2000-01-01 foo",
    "2000-01-01 abcdefghijklmnopqrstuvwxyz",
    "2000-01-01 f.o.o",
    "2000-01-01 99999999999999999999",
    "2000-01-01 -99999999999999999999 days",
    "99999999999-01-01",
    "2147485547-01-01",
    "2147485548-01-01",
    "-5-01-01",
    // The @ form.
    "@0",
    "@-1",
    "@1.5",
    "@-1.5",
    "@ 5",
    "@1 2",
    "@",
    "@x",
    "@1.000000000999",
    "@-0.0000000001",
    "@9223372036854775807",
    "@-9223372036854775808",
    "@67768036191676799",
    "@67768036191676800",
    // TZ="..." inside the string.
    "TZ=\"Asia/Tokyo\" 2000-01-01 12:00",
    "TZ=\"America/Edmonton\" 2006-04-02 02:30:00",
    "TZ=\"EST5\" 2000-01-01 12:00",
    "TZ=\"EST5EDT\" 2000-07-01 12:00 EDT",
    "TZ=\"XXX-3:30\" 2000-01-01 12:00",
    "TZ=\"\" 2000-01-01",
    "TZ=\"a\\\"b\" 2000-01-01",
    "TZ=\"a\\\\b\" 2000-01-01",
    "TZ=\"a\\b\" 2000-01-01",
    "TZ=\"unterminated 2000-01-01",
    "  TZ=\"UTC0\"   2000-01-01",
];

#[test]
fn date_strings_utc() {
    check_table("UTC0", UTC_STRINGS);
}

/// The same strings in a zone with an offset and DST.
#[test]
fn date_strings_new_york() {
    check_table("America/New_York", UTC_STRINGS);
}

#[test]
fn date_strings_dst() {
    check_table(
        "America/New_York",
        &[
            "2016-06-01 EDT + 6 months",
            "2020-03-08 02:30",
            "2020-03-08 02:30 -0500",
            "2020-03-08 02:30 EST",
            "2020-03-08 02:30 EDT",
            "2020-11-01 01:30",
            "2020-11-01 01:30 EST",
            "2020-11-01 01:30 EDT",
            "2020-11-01 01:30 EDT DST",
            "2020-03-07 12:00 +1 day",
            "2020-03-07 12:00 +24 hours",
            "2020-03-08 00:00 +1 day",
            "2020-07-01 12:00",
            "2020-01-01 EDT",
            "2020-07-01 EST",
            "TZ=\"America/Edmonton\" 2006-04-02 02:30:00",
        ],
    );
    check_table(
        "Europe/Helsinki",
        &[
            "2011-12-11 EET",
            "2011-06-11 EEST",
            "Mon Mar 28 00:36:07 2011 EEST 1 day ago",
            "Mon Mar 28 00:36:07 2011 EEST 24 hours ago",
            "2011-03-27 03:30",
            "2011-10-30 03:30",
        ],
    );
    check_table(
        "Europe/Dublin",
        &["2020-07-01 IST", "2020-01-01 GMT", "2020-07-01 12:00"],
    );
    check_table(
        "Australia/Lord_Howe",
        &["2020-10-04 02:15", "2020-04-05 01:45", "2020-01-01 +11"],
    );
    // Before 2005 Troll's offset was unknown: "-00", which %z shows as -0000.
    check_table(
        "Antarctica/Troll",
        &["1990-01-01", "@0", "2020-07-01 12:00"],
    );
}

/// GNU's own date.pl and date-debug.sh strings.
#[test]
fn date_strings_gnu_suite() {
    check_table(
        "UTC0",
        &[
            "1997-01-19 08:17:48 +0",
            "1995-1-1",
            "1992-1-5",
            "2016-10-1",
            "1998-1-1 3 years",
            "2005-01-01 00:00:00 +0 4 seconds ago",
            "20130101",
            "2013-10-30 00:00:00 UTC -8 days",
            "Apr 11 22:59:00 2011",
            "TZ=\"EST5\" 1970-01-01 00:00",
            "TZ=\"EST5\" 1970-01-01 00:00 +0",
            "1970-01-01 00:00 +0 TZ=\"EST5\"",
            "2000-01-01 09:00T",
        ],
    );
    check_table(
        "PST8",
        &[
            "TZ=\"EST5\" 1970-01-01 00:00",
            "TZ=\"EST5\" 1970-01-01 00:00 J",
        ],
    );
    check_table(
        "America/Belize",
        &["TZ=\"Asia/Tokyo\" Sun, 90-12-11 + 3 days - 90 minutes"],
    );
    let long = format!("TZ=\"{}0\" 2017-01-01", "a".repeat(2000));
    check_lines("UTC0", &[long]);
}

/// A small deterministic generator (xorshift64*), so failures reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

const WORDS: &[&str] = &[
    "jan",
    "Feb",
    "march",
    "Sep.",
    "sept",
    "SEPT.",
    "sun",
    "Mon",
    "tues",
    "wednes",
    "thur",
    "friday",
    "sat",
    "year",
    "years",
    "month",
    "fortnight",
    "weeks",
    "day",
    "hour",
    "minute",
    "mins",
    "second",
    "secs",
    "tomorrow",
    "yesterday",
    "today",
    "now",
    "last",
    "this",
    "next",
    "first",
    "third",
    "twelfth",
    "ago",
    "hence",
    "gmt",
    "UT",
    "utc",
    "wet",
    "bst",
    "nst",
    "edt",
    "CST",
    "pst",
    "akdt",
    "hst",
    "cest",
    "eet",
    "msk",
    "ist",
    "jst",
    "nzdt",
    "DST",
    "e.s.t.",
    "a",
    "j",
    "m",
    "n",
    "t",
    "y",
    "z",
    "am",
    "PM",
    "a.m.",
    "p.m.",
    "foo",
    "ago.",
];
const PUNCT: &[&str] = &[":", "/", ",", ".", "-", "+", "@", "T", " ", "\t"];
const DATES: &[&str] = &[
    "2020-02-29",
    "1999-12-31",
    "2000-01-01T00:00:00",
    "1/2/2003",
    "Jan 31 2021",
    "31 dec 1969",
    "jun-17-1992",
    "20380119 00:00",
    "1970-01-01 00:00:00 +0",
    "@1234567890",
];

fn gen_number(r: &mut Rng) -> String {
    match r.below(8) {
        0..=2 => format!("{:01$}", r.below(100), 1 + r.below(2) as usize),
        3 => format!("{:01$}", r.below(10_000), 3 + r.below(2) as usize),
        4 => format!("{:01$}", r.below(100_000_000), 5 + r.below(4) as usize),
        5 => {
            let (int, places) = (r.below(100), 1 + r.below(11) as u32);
            format!("{int}.{}", r.below(10u64.pow(places)))
        }
        6 => r.next().to_string(),
        _ => r.below(61).to_string(),
    }
}

/// A random string that always names an absolute date somewhere.
fn gen_string(r: &mut Rng) -> String {
    let mut items: Vec<String> = Vec::new();
    for _ in 0..r.below(6) {
        let item = match r.below(10) {
            0..=3 => gen_number(r),
            4..=7 => r.pick(WORDS).to_string(),
            _ => r.pick(PUNCT).to_string(),
        };
        items.push(item);
    }
    let at = r.below(items.len() as u64 + 1) as usize;
    items.insert(at, r.pick(DATES).to_string());
    let sep = [" ", "", "  "];
    let mut s = String::new();
    for item in items {
        s.push_str(&item);
        s.push_str(sep[r.below(3) as usize]);
    }
    if r.below(12) == 0 {
        let zones = ["UTC0", "EST5EDT", "Asia/Tokyo", "XXX-3:30"];
        s = format!("TZ=\"{}\" {s}", zones[r.below(4) as usize]);
    }
    s
}

#[test]
fn date_strings_generated() {
    for (seed, tz) in [
        (1, "UTC0"),
        (2, "America/New_York"),
        (3, "Europe/Helsinki"),
        (4, "Asia/Kolkata"),
    ] {
        let mut r = Rng(0x9e37_79b9_7f4a_7c15 ^ seed);
        let lines: Vec<String> = (0..2500).map(|_| gen_string(&mut r)).collect();
        check_lines(tz, &lines);
    }
}

gnu_cases! {
    // --debug diagnostics outside the parser.
    date_debug_discarded_d: "date" ["--debug", "-d", "x", "-d", "@0"] env [("TZ", "UTC0")];
    date_debug_discarded_d_quiet: "date" ["-d", "x", "-d", "@0"] env [("TZ", "UTC0")];
    date_debug_utc_flag: "date" ["-u", "--debug", "-d", "2020-01-01"] env [("TZ", "Asia/Tokyo")];
    date_debug_tz_unset: "date" ["--debug", "-d", "@0", "+%F"];
    date_debug_invalid: "date" ["--debug", "-d", "2020-02-30"] env [("TZ", "UTC0")];
    date_debug_file: "date" ["--debug", "-f", "-", "+%s"] env [("TZ", "UTC0")] stdin b"@1\nbogus\n2020-01-01\n";
    date_debug_reference_format: "date" ["--debug", "-d", "@0", "-R"] env [("TZ", "UTC0")];
    date_out_of_range: "date" ["-d", "@67768036191676800"] env [("TZ", "UTC0")];
    date_out_of_range_negative: "date" ["-d", "@-67768040609740801"] env [("TZ", "UTC0")];
    date_file_nul_line: "date" ["-f", "-", "+%s"] env [("TZ", "UTC0")] stdin b"@1\0junk\nx\0y\n";
    date_file_no_final_newline: "date" ["-f", "-", "+%s"] env [("TZ", "UTC0")] stdin b"@1\n@2";
    date_file_crlf: "date" ["-f", "-", "+%s"] env [("TZ", "UTC0")] stdin b"@1\r\n";
    date_long_tz_in_string: "date" ["-d", "TZ=\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0\" 2017", "+%F"] env [("TZ", "UTC0")];
    date_tz_in_string_output_zone: "date" ["-d", "TZ=\"Asia/Tokyo\" 2000-01-01 09:00"] env [("TZ", "America/New_York")];
    // Found by util/date-fuzz.py. nstrftime's year conversions for years
    // before 0: %C and %y truncate toward zero, %C of -1..-99 is "-0".
    date_negative_year_century: "date" ["-d", "@-12120000000000", "+%Y %C %y %g %G %+3C %+4y %_5C %-C"] env [("TZ", "UTC0")];
    date_negative_year_small: "date" ["-f", "-", "+%Y %C %y %g %G|%+C|%+y"] env [("TZ", "UTC0")] stdin b"@-62198755200\n@-65000000000\n@-59000000000\n@-61000000000\n@-2208988800\n";
    // %c and %x come from glibc's own strftime: an unpadded, int-wrapping
    // %Y and a %y of (tm_year % 100 + 100) % 100.
    date_c_x_odd_years: "date" ["-f", "-", "+%c|%x|%^c|%30c|%_12x|%Ec|%D"] env [("TZ", "UTC0")] stdin b"0005-01-01\n@67768036191676799\n@-12120000000000\n@-62198755200\n9999-12-31\n";
    // %s is mktime of the broken-down time: in Moscow's repeated hour of
    // 2014-10-26, whose offsets share tm_isdst, it names the other instant.
    date_s_repeated_hour: "date" ["-f", "-", "+%s %T %z"] env [("TZ", "Europe/Moscow")] stdin b"@1414270800\n@1414272600\n@1414274400\n";
    // ...and it moves glibc's mktime guess, which picks the side of the next
    // line's ambiguous local time.
    date_s_steers_next_line: "date" ["-f", "-", "+%s"] env [("TZ", "America/New_York")] stdin b"@0\n2020-11-01 01:31\n2020-11-01 01:31 +1 hour\n";
    // A TZ="..." name that is not UTF-8 is echoed as bytes.
    date_debug_tz_not_utf8: "date" ["--debug", "-f", "-", "-R"] env [("TZ", "Asia/Kolkata")] stdin b"TZ=\"\x80\" 2000-01-01\nTZ=\"E\x95ST5\" 00:00\n";
}

// nstrftime as ls, du and stat use it too: the E and O modifiers (what
// gnulib leaves to glibc's strftime), unknown conversions, and the flags a
// subformat passes on.
gnu_cases! {
    date_modifiers_odd_years: "date" ["-f", "-", "+%EC|%Ey|%EY|%Oy|%OC|%Og|%OG|%_5Od|%-Od|%_Oe|%5EY|%^Ox|%#Eb|%Oq|%_6Oz"] env [("TZ", "UTC0")] stdin b"@-62200000000\n@-100000000000\n@-62162035200\n@0\n@67767976233316800\n";
    date_modifiers_offset: "date" ["-d", "@0", "+%Oz|%_9Oz|%-Oz|%5Oz"] env [("TZ", "Asia/Kolkata")];
    date_modifiers_negative_offset: "date" ["-d", "@0", "+%Oz|%_9Oz|%-Oz|%5Oz"] env [("TZ", "America/St_Johns")];
    date_unknown_conversions: "date" ["-d", "@0", "+[%5J][%_5J][%5OX][%5:J][%5::J][%5%d][%^5%d][%5E][%5::::z][%::%d][%:][%5]"] env [("TZ", "UTC0")];
    date_flagged_subformats: "date" ["-f", "-", "+%_D|%-D|%+D|%_F|%-F|%3F|%12F|%+12F|%_15F|%010F"] env [("TZ", "UTC0")] stdin b"@-62200000000\n@-62162035200\n@0\n";
}

/// ls, du and stat format file times with the formatter `date` uses
/// (`common/date_format.rs`): files dated in odd years, from before year 0
/// to the largest `int` year.
mod odd_years {
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// The fixture files, `y<seconds since the epoch>`.
    const FILES: [&str; 9] = [
        "y-67768040609740800", // the first second of year -2147481748
        "y-12219292800000",
        "y-100000000000", // year -1199
        "y-62200000000",  // year -2
        "y-62167219200",  // year 0
        "y-62135596800",  // year 1
        "y-59000000000",
        "y67767976233316800",
        "y67768036191676799", // the last second of year 2147485547
    ];

    fn stamp(name: &str) -> SystemTime {
        let secs: i64 = name[1..].parse().unwrap();
        let d = Duration::from_secs(secs.unsigned_abs());
        if secs < 0 {
            UNIX_EPOCH - d
        } else {
            UNIX_EPOCH + d
        }
    }

    /// Creates the files in `dir`; false if its file system cannot hold
    /// their times (ext4 with 128-byte inodes keeps 1901 to 2446 only).
    fn make_files(dir: &Path) -> bool {
        FILES.iter().all(|name| {
            let path = dir.join(name);
            let t = stamp(name);
            let times = std::fs::FileTimes::new().set_accessed(t).set_modified(t);
            std::fs::File::create(&path).is_ok_and(|f| f.set_times(times).is_ok())
                && std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .is_ok_and(|m| m == t)
        })
    }

    fn fixtures() -> &'static Path {
        static DIR: OnceLock<PathBuf> = OnceLock::new();
        DIR.get_or_init(|| {
            // Inside the binary's fixture directory, not a fresh one: making
            // it fresh again would delete it under the tests running in it.
            let dir = super::fixtures().join("odd-years");
            std::fs::create_dir_all(&dir).unwrap();
            if make_files(&dir) {
                return dir;
            }
            // tmpfs keeps 64-bit times. One directory per test binary and
            // user (the owner of `dir`), replaced on every run.
            let uid = std::fs::metadata(&dir).map_or(0, |m| m.uid());
            let shm =
                Path::new("/dev/shm").join(format!("{}-odd-years-{uid}", env!("CARGO_CRATE_NAME")));
            let _ = std::fs::remove_dir_all(&shm);
            if std::fs::create_dir_all(&shm).is_ok() && make_files(&shm) {
                return shm;
            }
            eprintln!("note: no file system here keeps times before 1901");
            dir
        })
    }

    const STYLE: &str =
        "--time-style=+%C.%y.%g.%G.%Y|%c|%x|%F|%D|%EC|%Ey|%EY|%Oy|%OC|%+6Y|%_3C|%-y|%s";

    crate::gnu_cases! {
        ls_odd_years_format: "ls" ["-l", STYLE, FILES[0], FILES[1], FILES[2], FILES[3], FILES[4], FILES[5], FILES[6], FILES[7], FILES[8]] env [("TZ", "UTC0")];
        ls_odd_years_default: "ls" ["-l", "."] env [("TZ", "UTC0")];
        ls_odd_years_full_iso: "ls" ["-lU", "--full-time", "."] env [("TZ", "America/New_York")];
        ls_odd_years_long_iso: "ls" ["-l", "--time-style=long-iso", "."] env [("TZ", "UTC0")];
        du_odd_years_format: "du" ["--time", STYLE, "-a", "."] env [("TZ", "UTC0")];
        du_odd_years_default: "du" ["--time", "-a", "."] env [("TZ", "UTC0")];
        du_odd_years_full_iso: "du" ["--time", "--time-style=full-iso", "-a", "."] env [("TZ", "America/New_York")];
        stat_odd_years: "stat" ["-c", "%n %y|%x|%Y|%X", FILES[0], FILES[1], FILES[2], FILES[3], FILES[4], FILES[5], FILES[6], FILES[7], FILES[8]] env [("TZ", "UTC0")];
        stat_odd_years_default: "stat" [FILES[3], FILES[8]] env [("TZ", "Asia/Kolkata")];
    }
}
