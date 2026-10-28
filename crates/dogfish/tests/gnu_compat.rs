//! Differential GNU-compatibility suite.
//!
//! Every case runs the same arguments (and stdin) through `dogfish <tool>` and
//! through the system GNU coreutils binary, then compares stdout, exit status
//! and (unless a case opts out) stderr byte-for-byte.
//!
//! When the system tool is missing or is not GNU coreutils (e.g. BSD tools on
//! macOS), the case prints a note and passes.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, OnceLock};

fn dogfish_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dogfish")
}

/// Fixture directory shared by every case; both implementations run with it
/// as their working directory so file names in output are identical.
fn fixtures() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        // Under target/tmp (cleaned by `cargo clean`), recreated fresh on each run.
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("gnu-compat-fixtures");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let files: &[(&str, &[u8])] = &[
            ("lines.txt", b"b\na\n\nc\n"),
            ("notrail.txt", b"one\ntwo\nthree"),
            ("empty.txt", b""),
            ("crlf.txt", b"alpha\r\nbeta\r\n\r\ngamma\r\n"),
            (
                "utf8.txt",
                "h\u{e9}llo w\u{f6}rld\n\u{65e5}\u{672c}\u{8a9e} \u{30c6}\u{30ad}\n\u{e9}t\u{e9}\n"
                    .as_bytes(),
            ),
            (
                "ctrl.txt",
                b"tab\there\x01ctl\x1bESC\x7fDEL\n\x80high\xffbyte\xa0nbsp\n\n\n\nend\tx",
            ),
            (
                "many.txt",
                b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n",
            ),
            ("blanks.txt", b"a\n\n\n\nb\n\n\nc\n"),
            (
                "nums.txt",
                b"10\n2\n1K\n2M\n-3\n1.5\n  7\n\nabc\n-0.5\n1K\n+4\n.25\n-\n007\n3e2\n",
            ),
            ("human.txt", b"1K\n2M\n512\n1G\n3k\n10\n1.5K\n-2K\n0\n"),
            (
                "fields.csv",
                b"name,age,city\nbob,30,NYC\nalice,25,LA\ncarol,35\nnodelim\n,,\n",
            ),
            (
                "words.txt",
                b"the quick brown fox\n  jumps   over\t the\nlazy dog\n",
            ),
            ("dup.txt", b"a\na\nb\nA\na\nc\nc\nc\nB\nb\n"),
            ("keyed.txt", b"b 2 x\na 10 y\nc 1 z\na 2 w\nB 3 v\nb 2 a\n"),
            (
                "colon.txt",
                b"root:x:0:0\ndaemon:x:1:1\nuser:x:1000:100\nnobody:x:65534:65534\n",
            ),
            ("tabs.txt", b"a\tb\tc\n1\t2\t3\n\tx\t\n"),
            (
                "mixcase.txt",
                b"banana\nApple\napple\nCherry\nbanana\nBANANA\n",
            ),
        ];
        for (name, data) in files {
            std::fs::write(dir.join(name), data).unwrap();
        }
        dir
    })
}

fn gnu_path(tool: &str) -> Option<PathBuf> {
    ["/usr/bin", "/bin", "/usr/local/bin"]
        .iter()
        .map(|d| Path::new(d).join(tool))
        .find(|p| p.is_file())
}

/// Returns the path of the system tool if it identifies as GNU coreutils.
fn gnu_tool(tool: &str) -> Option<PathBuf> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<PathBuf>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().unwrap();
    guard
        .entry(tool.to_string())
        .or_insert_with(|| {
            let path = gnu_path(tool)?;
            let out = Command::new(&path).arg("--version").output().ok()?;
            String::from_utf8_lossy(&out.stdout)
                .contains("GNU coreutils")
                .then_some(path)
        })
        .clone()
}

fn exec(mut cmd: Command, stdin: Option<&[u8]>, locale: &str) -> Output {
    cmd.current_dir(fixtures())
        .env("LC_ALL", locale)
        .env("LANG", "C")
        .env("NO_COLOR", "1")
        .env_remove("POSIXLY_CORRECT")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn");
    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().unwrap();
        let input = input.to_vec();
        // Write from a thread so large outputs can't deadlock us.
        std::thread::spawn(move || {
            let _ = pipe.write_all(&input);
        });
    }
    child.wait_with_output().expect("wait")
}

struct Case<'a> {
    tool: &'a str,
    args: &'a [&'a str],
    stdin: Option<&'a [u8]>,
    compare_stderr: bool,
    locale: &'a str,
}

/// True when the system C library actually provides `locale` with UTF-8
/// semantics (GNU tools silently fall back to "C" otherwise).
fn locale_is_utf8(locale: &str) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().unwrap();
    *guard.entry(locale.to_string()).or_insert_with(|| {
        let Some(wc) = gnu_tool("wc") else {
            return false;
        };
        let mut cmd = Command::new(wc);
        cmd.arg("-m");
        let out = exec(cmd, Some("\u{e9}".as_bytes()), locale);
        String::from_utf8_lossy(&out.stdout).trim() == "1"
    })
}

fn check(case: Case<'_>) {
    let Some(gnu) = gnu_tool(case.tool) else {
        eprintln!(
            "note: skipping — system `{}` is missing or not GNU coreutils",
            case.tool
        );
        return;
    };
    if case.locale != "C" && !locale_is_utf8(case.locale) {
        eprintln!("note: skipping — locale `{}` is unavailable", case.locale);
        return;
    }

    let mut ours = Command::new(dogfish_bin());
    ours.arg(case.tool).args(case.args);
    let mut theirs = Command::new(&gnu);
    // GNU prints argv[0] in diagnostics; make it the bare tool name.
    theirs.arg0(case.tool).args(case.args);

    let a = exec(ours, case.stdin, case.locale);
    let b = exec(theirs, case.stdin, case.locale);

    let show = |o: &Output| {
        format!(
            "status={:?}\n--- stdout ---\n{:?}\n--- stderr ---\n{:?}",
            o.status.code(),
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        )
    };
    let ctx = format!(
        "\ncase: {} {:?} (stdin: {:?})\n=== dogfish ===\n{}\n=== GNU ===\n{}\n",
        case.tool,
        case.args,
        case.stdin.map(String::from_utf8_lossy),
        show(&a),
        show(&b)
    );
    assert_eq!(a.stdout, b.stdout, "stdout differs{ctx}");
    assert_eq!(a.status.code(), b.status.code(), "exit status differs{ctx}");
    if case.compare_stderr {
        assert_eq!(a.stderr, b.stderr, "stderr differs{ctx}");
    }
}

/// `name: tool [args...] (@ "LOCALE")? (stdin b"...")? (=> nostderr)?;`
///
/// Cases run under `LC_ALL=C` unless a locale is given. Dogfish is always
/// UTF-8 aware, so multibyte cases (`wc -m`, `wc -L`) compare against GNU in
/// `C.UTF-8`.
macro_rules! gnu_cases {
    ($(
        $(#[$meta:meta])*
        $name:ident : $tool:literal [$($arg:expr),* $(,)?]
            $(@ $locale:literal)?
            $(stdin $input:expr)?
            $(=> $nostderr:ident)?
        ;
    )*) => {
        $(
            $(#[$meta])*
            #[test]
            fn $name() {
                #[allow(unused_mut, unused_assignments)]
                let mut stdin: Option<&[u8]> = None;
                $( stdin = Some($input); )?
                #[allow(unused_mut, unused_assignments)]
                let mut locale = "C";
                $( locale = $locale; )?
                #[allow(unused_mut, unused_assignments)]
                let mut compare_stderr = true;
                $( let _ = stringify!($nostderr); compare_stderr = false; )?
                check(Case {
                    tool: $tool,
                    args: &[$($arg),*],
                    stdin,
                    compare_stderr,
                    locale,
                });
            }
        )*
    };
}

gnu_cases! {
    // ---------------------------------------------------------------- cat
    cat_plain: "cat" ["lines.txt", "notrail.txt"];
    cat_stdin_dash: "cat" ["-", "lines.txt"] stdin b"from stdin\n";
    cat_show_all: "cat" ["-A", "ctrl.txt"];
    cat_show_nonprinting: "cat" ["-v", "ctrl.txt"];
    cat_show_ends: "cat" ["-E", "ctrl.txt", "crlf.txt"];
    cat_show_tabs: "cat" ["-T", "tabs.txt"];
    cat_e: "cat" ["-e", "ctrl.txt"];
    cat_t: "cat" ["-t", "ctrl.txt"];
    cat_number: "cat" ["-n", "lines.txt", "notrail.txt", "lines.txt"];
    cat_number_nonblank: "cat" ["-b", "blanks.txt"];
    cat_squeeze: "cat" ["-s", "blanks.txt"];
    cat_squeeze_number: "cat" ["-sn", "blanks.txt"];
    cat_b_overrides_n: "cat" ["-nb", "blanks.txt"];
    cat_show_all_crlf: "cat" ["-A", "crlf.txt"];
    cat_utf8_v: "cat" ["-v", "utf8.txt"];
    cat_empty: "cat" ["empty.txt"];
    cat_missing: "cat" ["nonexist"];
    cat_missing_then_file: "cat" ["nonexist", "lines.txt"];
    cat_number_stdin: "cat" ["-n"] stdin b"x\ny";

    // ---------------------------------------------------------- head/tail
    head_default: "head" ["many.txt"];
    head_n3: "head" ["-n", "3", "many.txt"];
    head_n_neg: "head" ["-n", "-2", "many.txt"];
    head_n_neg_attached: "head" ["-n-13", "many.txt"];
    head_n_neg_notrail: "head" ["-n", "-1", "notrail.txt"];
    head_n_neg_all: "head" ["-n", "-100", "many.txt"];
    head_c5: "head" ["-c", "5", "many.txt"];
    head_c_neg: "head" ["-c", "-4", "notrail.txt"];
    head_legacy: "head" ["-3", "many.txt"];
    head_stdin_neg: "head" ["-n", "-2"] stdin b"a\nb\nc\nd\n";
    head_stdin_c_neg: "head" ["-c", "-3"] stdin b"abcdefg";
    head_multi: "head" ["-n", "2", "lines.txt", "many.txt"];
    head_multi_quiet: "head" ["-q", "-n", "1", "lines.txt", "many.txt"];
    head_zero: "head" ["-n", "0", "many.txt"];
    head_notrail: "head" ["-n", "5", "notrail.txt"];
    head_missing: "head" ["nonexist"];
    tail_default: "tail" ["many.txt"];
    tail_n3: "tail" ["-n", "3", "many.txt"];
    tail_plus: "tail" ["-n", "+13", "many.txt"];
    tail_plus1: "tail" ["-n", "+1", "lines.txt"];
    tail_plus_stdin: "tail" ["-n", "+2"] stdin b"a\nb\nc\n";
    tail_c_plus: "tail" ["-c", "+3", "notrail.txt"];
    tail_c3: "tail" ["-c", "3", "notrail.txt"];
    tail_notrail: "tail" ["-n", "2", "notrail.txt"];
    tail_stdin: "tail" ["-n", "2"] stdin b"a\nb\nc";
    tail_legacy: "tail" ["-2", "many.txt"];
    tail_multi: "tail" ["-n", "1", "lines.txt", "notrail.txt"];
    tail_empty: "tail" ["empty.txt"];
    tail_missing: "tail" ["nonexist"];

    // ----------------------------------------------------------------- wc
    wc_default_file: "wc" ["words.txt"];
    wc_stdin: "wc" [] stdin b"hello world\nfoo\n";
    wc_stdin_l: "wc" ["-l"] stdin b"a\nb\n";
    wc_single_count: "wc" ["-w", "words.txt"];
    wc_multi: "wc" ["words.txt", "lines.txt", "empty.txt"];
    wc_multi_l: "wc" ["-l", "words.txt", "many.txt"];
    wc_lc: "wc" ["-lc", "utf8.txt"];
    wc_chars: "wc" ["-m", "utf8.txt"] @ "C.UTF-8";
    wc_bytes: "wc" ["-c", "utf8.txt"];
    wc_all_counts: "wc" ["-lwmc", "utf8.txt", "crlf.txt"] @ "C.UTF-8";
    wc_utf8_default: "wc" ["utf8.txt"] @ "C.UTF-8";
    wc_utf8_max_line: "wc" ["-L", "utf8.txt"] @ "C.UTF-8";
    wc_ctrl_utf8: "wc" ["-lwmcL", "ctrl.txt"] @ "C.UTF-8";
    wc_ctrl_c_locale: "wc" ["-lwL", "ctrl.txt"];
    wc_max_line: "wc" ["-L", "words.txt"];
    wc_max_line_tabs: "wc" ["-L", "tabs.txt", "crlf.txt"];
    wc_notrail: "wc" ["notrail.txt"];
    wc_empty: "wc" ["empty.txt"];
    wc_stdin_dash_and_file: "wc" ["-", "lines.txt"] stdin b"x y z\n";
    wc_missing: "wc" ["nonexist"];
    wc_missing_and_file: "wc" ["lines.txt", "nonexist"];

    // --------------------------------------------------------------- sort
    sort_plain: "sort" ["dup.txt"];
    sort_numeric: "sort" ["-n", "nums.txt"];
    sort_numeric_reverse: "sort" ["-rn", "nums.txt"];
    sort_human: "sort" ["-h", "human.txt"];
    sort_reverse: "sort" ["-r", "mixcase.txt"];
    sort_unique: "sort" ["-u", "dup.txt"];
    sort_fold: "sort" ["-f", "mixcase.txt"];
    sort_fold_unique: "sort" ["-fu", "mixcase.txt"];
    sort_key2n: "sort" ["-k2n", "keyed.txt"];
    sort_key2_2n: "sort" ["-k", "2,2n", "keyed.txt"];
    sort_key1_then_key2: "sort" ["-k1,1", "-k2,2n", "keyed.txt"];
    sort_key_stable: "sort" ["-s", "-k1,1", "keyed.txt"];
    sort_sep_key: "sort" ["-t", ":", "-k3,3n", "colon.txt"];
    sort_sep_key_rev: "sort" ["-t:", "-k3nr", "colon.txt"];
    sort_numeric_unique: "sort" ["-nu"] stdin b"1\n01\n2\n1.0\n";
    sort_stdin: "sort" [] stdin b"zeta\nalpha\nmid\n";
    sort_notrail: "sort" ["notrail.txt"];
    sort_multi: "sort" ["lines.txt", "notrail.txt"];
    sort_blanks: "sort" ["-b", "words.txt"];
    sort_missing: "sort" ["nonexist"] => nostderr;

    // --------------------------------------------------------------- uniq
    uniq_plain: "uniq" ["dup.txt"];
    uniq_count: "uniq" ["-c", "dup.txt"];
    uniq_repeated: "uniq" ["-d", "dup.txt"];
    uniq_unique: "uniq" ["-u", "dup.txt"];
    uniq_ignore_case: "uniq" ["-i", "dup.txt"];
    uniq_ignore_case_count: "uniq" ["-ic", "dup.txt"];
    uniq_stdin: "uniq" [] stdin b"x\nx\ny\n";
    uniq_notrail: "uniq" [] stdin b"x\nx";

    // ---------------------------------------------------------------- cut
    cut_fields: "cut" ["-d", ",", "-f", "1,3", "fields.csv"];
    cut_field_range: "cut" ["-d,", "-f2-", "fields.csv"];
    cut_only_delim: "cut" ["-s", "-d,", "-f1", "fields.csv"];
    cut_chars: "cut" ["-c", "1-3", "words.txt"];
    cut_chars_list: "cut" ["-c", "1,3,5-", "lines.txt"];
    cut_bytes: "cut" ["-b", "-2", "fields.csv"];
    cut_tab_default: "cut" ["-f", "2", "tabs.txt"];
    cut_colon: "cut" ["-d:", "-f", "1,3-4", "colon.txt"];
    cut_out_delim: "cut" ["-d:", "-f1,3", "--output-delimiter=|", "colon.txt"];
    cut_stdin: "cut" ["-d", " ", "-f", "2"] stdin b"a b c\nd e f\n";
    cut_notrail: "cut" ["-c1"] stdin b"ab\ncd";

    // ----------------------------------------------------------------- tr
    tr_upper: "tr" ["a-z", "A-Z"] stdin b"Hello, World!\n";
    tr_class: "tr" ["[:lower:]", "[:upper:]"] stdin b"abc xyz\n";
    tr_delete: "tr" ["-d", "aeiou"] stdin b"education\n";
    tr_squeeze: "tr" ["-s", " "] stdin b"a   b    c\n";
    tr_squeeze_translate: "tr" ["-s", "a-z", "A-Z"] stdin b"aabbccdd\n";
    tr_complement_delete: "tr" ["-cd", "0-9\n"] stdin b"a1b2c3\nx9\n";
    tr_complement_translate: "tr" ["-c", "a-z\n", "_"] stdin b"ab-cd e!\n";
    tr_newlines: "tr" ["\\n", " "] stdin b"a\nb\nc\n";
    tr_short_set2: "tr" ["abc", "x"] stdin b"aabbcc\n";
    tr_delete_squeeze: "tr" ["-ds", "a", "b"] stdin b"aabbbcbb\n";
    tr_digits: "tr" ["[:digit:]", "#"] stdin b"r2d2 c3po\n";
    tr_octal: "tr" ["\\101", "z"] stdin b"ABA\n";

    // ----------------------------------------------------------------- nl
    nl_default: "nl" ["lines.txt"];
    nl_stdin: "nl" [] stdin b"b\na\n\nc\n";
    nl_all: "nl" ["-ba", "lines.txt"];
    nl_width_sep: "nl" ["-w", "3", "-s", ": ", "lines.txt"];
    nl_notrail: "nl" ["notrail.txt"];
    nl_multi: "nl" ["lines.txt", "notrail.txt"];

    // ---------------------------------------------------------------- tac
    tac_file: "tac" ["many.txt"];
    tac_notrail: "tac" ["notrail.txt"];
    tac_stdin: "tac" [] stdin b"1\n2\n3\n";
    tac_blank: "tac" ["blanks.txt"];

    // ---------------------------------------------------------------- seq
    seq_simple: "seq" ["5"];
    seq_range: "seq" ["3", "7"];
    seq_step: "seq" ["1", "3", "10"];
    seq_negative_step: "seq" ["10", "-3", "1"];
    seq_float_step: "seq" ["0", "0.25", "1"];
    seq_neg_float: "seq" ["1", "-0.5", "-1"];
    seq_width: "seq" ["-w", "8", "11"];
    seq_width_neg: "seq" ["-w", "-3", "2"];
    seq_sep: "seq" ["-s", ", ", "1", "4"];
    seq_empty_range: "seq" ["5", "1"];
    seq_float_bounds: "seq" ["1.5", "4"];

    // --------------------------------------------------- basename/dirname
    basename_simple: "basename" ["/usr/lib/libfoo.so"];
    basename_suffix: "basename" ["/a/b/file.txt", ".txt"];
    basename_trailing_slash: "basename" ["/a/b/"];
    basename_multi: "basename" ["-a", "/x/y", "z/w"];
    basename_s: "basename" ["-s", ".c", "a/b.c", "c/d.c"];
    basename_root: "basename" ["/"];
    dirname_simple: "dirname" ["/usr/lib/libfoo.so"];
    dirname_noslash: "dirname" ["file"];
    dirname_trailing: "dirname" ["/a/b/"];
    dirname_multi: "dirname" ["/a/b", "c/d/e", "/"];

    // ---------------------------------------------------------- echo/printf
    echo_simple: "echo" ["hello", "world"];
    echo_n: "echo" ["-n", "no newline"];
    echo_e: "echo" ["-e", "a\\tb\\nc"];
    echo_no_escape: "echo" ["a\\tb"];
    echo_octal: "echo" ["-e", "\\0101\\0102"];
    printf_s: "printf" ["%s-%s\\n", "a", "b", "c"];
    printf_d: "printf" ["%5d|%-5d|%05d\\n", "42", "7", "3"];
    printf_float: "printf" ["%.3f %e\\n", "3.14159", "1234.5"];
    printf_hex: "printf" ["%x %X %o\\n", "255", "255", "8"];
    printf_escape: "printf" ["a\\tb\\\\c\\n"];
    printf_percent: "printf" ["100%%\\n"];
    printf_char: "printf" ["%c%c\\n", "hello", "world"];

    // --------------------------------------------------------- base64/hash
    base64_encode: "base64" ["lines.txt"];
    base64_stdin: "base64" [] stdin b"The quick brown fox jumps over the lazy dog, again and again and again.";
    base64_decode: "base64" ["-d"] stdin b"aGVsbG8gd29ybGQK\n";
    base64_empty: "base64" ["empty.txt"];
    sha256_files: "sha256sum" ["lines.txt", "empty.txt"];
    sha256_stdin: "sha256sum" [] stdin b"abc";
    md5_files: "md5sum" ["lines.txt", "utf8.txt"];
    md5_stdin: "md5sum" [] stdin b"";
    md5_missing: "md5sum" ["nonexist"];
    base64_missing: "base64" ["nonexist"];
    base64_wrap: "base64" ["-w", "8", "utf8.txt"];
    base64_nowrap: "base64" ["-w", "0", "many.txt"];
    base64_roundtrip_ctrl: "base64" ["ctrl.txt"];
    sha256_crlf: "sha256sum" ["crlf.txt", "ctrl.txt"];
    sha256_missing_and_file: "sha256sum" ["nonexist", "empty.txt"];

    // ------------------------------------------------------ extra coverage
    cat_A_stdin: "cat" ["-A"] stdin b"x\ty\r\n\x00\x7f\xc3\xa9\n";
    cat_vT_utf8: "cat" ["-vT", "utf8.txt", "tabs.txt"];
    cat_E_crlf_stdin: "cat" ["-E"] stdin b"a\r\nb\rc\r";
    cat_nb_notrail_chain: "cat" ["-b", "notrail.txt", "lines.txt"];
    cat_s_leading_blanks: "cat" ["-s"] stdin b"\n\n\nx\n\n\n";
    cat_n_chain: "cat" ["-n", "notrail.txt", "notrail.txt"];
    head_verbose_stdin: "head" ["-v", "-n", "1"] stdin b"a\nb\n";
    head_c_suffix: "head" ["-c", "1K", "many.txt"];
    head_bad_count: "head" ["-n", "abc", "many.txt"];
    head_missing_and_file: "head" ["-n", "1", "nonexist", "lines.txt"];
    head_crlf: "head" ["-n", "2", "crlf.txt"];
    tail_c_plus1: "tail" ["-c", "+1", "lines.txt"];
    tail_n0: "tail" ["-n", "0", "many.txt"];
    tail_plus_beyond: "tail" ["-n", "+100", "many.txt"];
    tail_c_stdin: "tail" ["-c", "4"] stdin b"abcdefg\n";
    tail_verbose: "tail" ["-v", "-n", "1", "lines.txt"];
    tail_bad_count: "tail" ["-n", "x"] stdin b"";
    sort_general: "sort" ["-g"] stdin b"1e3\n10\n-inf\nabc\n2.5\n0x10\n-1e-2\n";
    sort_dictionary: "sort" ["-d"] stdin b"b-c\na_b\n#a\nab\n";
    sort_ignore_nonprint: "sort" ["-i"] stdin b"b\x01a\nba\na\x02z\n";
    sort_check_ok: "sort" ["-c", "many.txt"] => nostderr;
    sort_check_bad: "sort" ["-c"] stdin b"b\na\n";
    sort_key_char_pos: "sort" ["-k1.2,1.3"] stdin b"xbz\nyaz\nzay\nwbb\n";
    sort_key_blanks: "sort" ["-k2b,2"] stdin b"a   c\nb  a\nc b\n";
    sort_key_reverse_only: "sort" ["-k2,2r", "-k1,1"] stdin b"a 1\nb 2\nc 1\n";
    sort_numeric_key_tab: "sort" ["-t", ",", "-k2,2n", "-k1,1r", "fields.csv"];
    sort_human_reverse: "sort" ["-hr"] stdin b"10K\n5G\n200M\n500\n1k\n";
    sort_reverse_stable: "sort" ["-rs", "-k1,1"] stdin b"a 1\nb 2\na 3\nb 4\n";
    sort_crlf: "sort" ["crlf.txt"];
    sort_utf8: "sort" ["utf8.txt", "lines.txt"];
    sort_bad_key: "sort" ["-k0"] stdin b"x\n";
    sort_multichar_tab: "sort" ["-t", "ab"] stdin b"x\n";
    uniq_skip_fields: "uniq" ["-f", "1"] stdin b"a x\nb x\nc y\n";
    uniq_skip_chars: "uniq" ["-s", "1"] stdin b"ax\nbx\ncy\n";
    uniq_check_chars: "uniq" ["-w", "1", "-c"] stdin b"ab\nac\nbd\n";
    uniq_crlf: "uniq" ["-c"] stdin b"a\r\na\r\na\n";
    uniq_missing: "uniq" ["nonexist"];
    cut_complement: "cut" ["-d,", "-f2", "--complement", "fields.csv"];
    cut_overlap_ranges: "cut" ["-c", "3,1-2,2-4"] stdin b"abcdefg\n";
    cut_fields_reorder: "cut" ["-d:", "-f3,1"] stdin b"a:b:c:d\n";
    cut_crlf: "cut" ["-c", "1-5"] stdin b"alpha\r\nbe\r\n";
    cut_missing: "cut" ["-f1", "nonexist", "tabs.txt"];
    cut_no_list: "cut" ["fields.csv"];
    cut_zero_field: "cut" ["-f0", "fields.csv"];
    tr_ranges_mixed: "tr" ["a-cx-z", "A-CX-Z"] stdin b"abcxyz mno\n";
    tr_squeeze_class: "tr" ["-s", "[:space:]"] stdin b"a  \t\t b\n\n\nc\n";
    tr_delete_class: "tr" ["-d", "[:punct:]"] stdin b"he,l.l!o?\n";
    tr_repeat_fill: "tr" ["a-f", "[x*]"] stdin b"abcdefgh\n";
    tr_repeat_n: "tr" ["abcd", "[x*2]yz"] stdin b"abcd\n";
    tr_truncate: "tr" ["-t", "abc", "xy"] stdin b"abcabc\n";
    tr_high_bytes: "tr" ["\\200-\\377", "?"] stdin b"a\xc3\xa9b\n";
    tr_complement_squeeze: "tr" ["-cs", "a-z", "\\n"] stdin b"hello, big world!\n";
    tr_missing_operand: "tr" ["a"] stdin b"x\n";
    nl_rz: "nl" ["-n", "rz", "-w", "3", "lines.txt"];
    nl_ln: "nl" ["-n", "ln", "lines.txt"];
    nl_bn: "nl" ["-b", "n", "lines.txt"];
    nl_start_incr: "nl" ["-v", "10", "-i", "5", "many.txt"];
    nl_crlf: "nl" ["crlf.txt"];
    nl_missing: "nl" ["nonexist"];
    tac_crlf: "tac" ["crlf.txt"];
    tac_multi: "tac" ["lines.txt", "notrail.txt", "empty.txt"];
    tac_missing: "tac" ["nonexist"];
    echo_hex_high: "echo" ["-e", "\\xc3\\xa9\\x41"];
    echo_c_stop: "echo" ["-e", "abc\\cdef", "ghi"];
    echo_E: "echo" ["-E", "a\\nb"];
    echo_bad_flag: "echo" ["-x", "y"];
    echo_backslash_seq: "echo" ["-e", "a\\qb\\\\"];
    printf_g: "printf" ["%g %g %g %G %g %g\\n", "100000", "1000000", "0.0001", "1e-5", "0", "123.456"];
    printf_flags: "printf" ["%5.2s|%-4d|%+d|% d|%#x|%#o|%05.1f\\n", "hello", "3", "5", "5", "255", "8", "3.14159"];
    printf_star: "printf" ["%*d|%-*d|%.*f\\n", "5", "42", "4", "7", "2", "3.14159"];
    printf_char_code: "printf" ["%d %d\\n", "'A", "\"z"];
    printf_unsigned_neg: "printf" ["%u %x\\n", "-1", "-1"];
    printf_bad_number: "printf" ["%d|%d\\n", "abc", "12abc"];
    printf_octal_hex_args: "printf" ["%d %d %i\\n", "0x1F", "010", "-077"];
    printf_escapes: "printf" ["\\101\\x41\\u00e9\\t\\q\\n"] @ "C.UTF-8";
    printf_b: "printf" ["%b|%b\\n", "a\\0101\\101\\tz", "x\\cy"];
    printf_reuse_missing: "printf" ["%s=%d;", "a", "1", "b"];
    printf_invalid_conv: "printf" ["%z"];
    printf_exp: "printf" ["%e|%.2E|%10.3e\\n", "0", "-12345.678", "0.000123"];
    printf_inf: "printf" ["%f %e %5.1f\\n", "inf", "-inf", "nan"];
    printf_prec_int: "printf" ["%.3d|%.0d|%8.3x\\n", "7", "0", "255"];

    // --------------------------------------------------------------- paste
    paste_parallel: "paste" ["lines.txt", "notrail.txt"];
    paste_delims: "paste" ["-d", ",:", "lines.txt", "many.txt", "notrail.txt"];
    paste_serial: "paste" ["-s", "lines.txt", "notrail.txt"];
    paste_stdin: "paste" ["-", "lines.txt"] stdin b"x\ny\n";
    paste_missing: "paste" ["nonexist"];
}
