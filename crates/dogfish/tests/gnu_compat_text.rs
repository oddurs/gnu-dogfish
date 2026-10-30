//! Differential GNU-compatibility suite for the text tools' less common
//! behaviour: `-z` records, `--files0-from`, obsolete option syntax, offsets
//! of a shared standard input, and GNU's diagnostics.
//!
//! Plain cases use [`gnu_cases!`]. Cases that need a shell (a subshell
//! sharing one standard input, a redirection onto an input file) use
//! [`shell_cases!`]: the script runs twice under `sh -c`, once with the named
//! tools resolving to Dogfish and once to GNU, each in a fresh directory.

#![allow(non_snake_case)]

mod common;

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use common::{dogfish_bin, exec, gnu_tool};

fn fixtures() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = common::fresh_fixture_dir();
        let files: &[(&str, &[u8])] = &[
            ("lines.txt", b"b\na\n\nc\n"),
            ("notrail.txt", b"one\ntwo\nthree"),
            (
                "many.txt",
                b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n",
            ),
            ("cr1.txt", b"1\r"),
            ("cr2.txt", b"\n2\r\n"),
            ("cr3.txt", b"2\r\n"),
            ("nul.txt", b"b\0a\0b\0b\0c"),
            ("nulsort.txt", b"b 2\0a 1\0c\n3\0"),
            ("names0.txt", b"lines.txt\0many.txt\0"),
            ("names0-bad.txt", b"lines.txt\0\0nonexist\0"),
            ("names0-dash.txt", b"-\0"),
            ("dup.txt", b"a\na\nb\nc\nc\nc\nd\n"),
            ("fields.txt", b"a:b:c\nd:e\nfgh\n"),
            (
                "u8.txt",
                b"h\xc3\xa9llo w\xffrld \xc2\xa0x\n\xe6\x97\xa5\xe6\x9c\xac\n",
            ),
        ];
        common::write_files(&dir, files);
        // Names that are not UTF-8 (`@xff@` in a case's arguments). Some
        // file systems (APFS) refuse them with EILSEQ; the cases then
        // compare the diagnostics of a missing file instead.
        let odd: &[(&[u8], &[u8])] = &[
            (b"odd\xffname.txt", b"b\na\na\nc:d\n"),
            (b"one\xff.txt", b"only\n"),
            (b"nl\n\xff", b"x\n"),
            (b"dates\xff.txt", b"@1\nbogus\n"),
        ];
        for (name, data) in odd {
            match std::fs::write(dir.join(OsStr::from_bytes(name)), data) {
                Err(e) if e.raw_os_error() == Some(libc::EILSEQ) => {}
                res => res.unwrap(),
            }
        }
        std::fs::write(
            dir.join("names0-odd.txt"),
            b"odd\xffname.txt\0lines.txt\0nope\xff\0",
        )
        .unwrap();
        dir
    })
}

gnu_cases! {
    // ---------------------------------------------------------------- cat
    // A CR at the end of one file and LF at the start of the next is a CRLF.
    cat_E_cr_across_files: "cat" ["-E", "cr1.txt", "cr2.txt"];
    cat_E_cr_not_followed_by_lf: "cat" ["-E", "cr1.txt", "cr3.txt"];
    cat_E_cr_at_end: "cat" ["-E", "lines.txt", "cr1.txt"];

    // ---------------------------------------------------------- head/tail
    head_obsolete_bytes: "head" ["-2c", "many.txt"];
    head_obsolete_blocks: "head" ["-1b", "many.txt"];
    head_obsolete_kib: "head" ["-1k", "many.txt"];
    head_obsolete_lines_verbose: "head" ["-3lv", "many.txt"];
    head_obsolete_bad_letter: "head" ["-1x", "many.txt"];
    head_digit_option_later: "head" ["-n", "1", "-3", "many.txt"];
    head_zero: "head" ["-z", "-n", "2", "nul.txt"];
    head_zero_elide: "head" ["-z", "-n", "-2", "nul.txt"];
    head_zero_elide_stdin: "head" ["-z", "-n", "-1"] stdin b"x\0y\0z";
    head_elide_lines_stdin: "head" ["-n", "-3"] stdin b"1\n2\n3\n4\n5";
    head_elide_bytes_stdin: "head" ["-c", "-3"] stdin b"abcdefgh";
    head_count_too_large: "head" ["-n", "99999999999999999999", "many.txt"];
    head_bad_suffix: "head" ["-c", "1g", "many.txt"];
    head_elide_bytes_too_large: "head" ["-c", "-9999999999999999999", "many.txt"];
    head_kib_suffix: "head" ["-c", "1KiB", "many.txt"];
    head_last_count_wins: "head" ["-c", "2", "-n", "1", "many.txt"];
    head_directory: "head" ["-n", "-1", "."];
    tail_obsolete_plus_bytes: "tail" ["+2c", "notrail.txt"];
    tail_obsolete_bytes: "tail" ["-3c", "notrail.txt"];
    tail_obsolete_lines: "tail" ["-1l", "notrail.txt"];
    tail_obsolete_l_default: "tail" ["-l", "many.txt"];
    tail_obsolete_b_default: "tail" ["-b", "many.txt"];
    tail_obsolete_plus: "tail" ["+14", "many.txt"];
    tail_obsolete_plus_c: "tail" ["+c", "many.txt"];
    tail_obsolete_too_large: "tail" ["-99999999999999999999", "many.txt"];
    tail_obsolete_posix2001: "tail" ["+2", "many.txt"] env [("_POSIX2_VERSION", "200112")];
    tail_digit_option: "tail" ["-2cX"];
    tail_count_too_large: "tail" ["-c99999999999999999999", "many.txt"];
    tail_zero: "tail" ["-z", "-n", "2", "nul.txt"];
    tail_zero_from_start: "tail" ["-z", "-n", "+3", "nul.txt"];
    tail_zero_count_skips_open: "tail" ["-n", "0", "nonexist"];
    tail_retry_without_follow: "tail" ["--retry", "-n", "2", "many.txt"];
    tail_retry_missing_without_follow: "tail" ["--retry", "nonexist"];
    tail_pid_without_follow: "tail" ["--pid=1", "-n", "1", "many.txt"];
    tail_follow_dash_by_name: "tail" ["-F", "-"];
    tail_follow_stdin_by_name: "tail" ["--follow=name"];
    tail_follow_bad_mode: "tail" ["--follow=sideways", "many.txt"];
    tail_follow_abbreviated_mode: "tail" ["--follow=n", "-"];
    tail_bad_sleep: "tail" ["-f", "-s", "x", "many.txt"];
    tail_negative_sleep: "tail" ["-f", "-s", "-1", "many.txt"];
    tail_overflowing_sleep: "tail" ["-f", "-s", "1e999", "many.txt"];
    tail_bad_pid: "tail" ["-f", "--pid=x", "many.txt"];
    tail_pid_too_large: "tail" ["-f", "--pid=2147483648", "many.txt"];
    tail_bad_max_unchanged_stats: "tail" ["-f", "--max-unchanged-stats=-1", "many.txt"];
    tail_follow_missing_by_descriptor: "tail" ["-f", "nonexist"];
    tail_follow_missing_by_name: "tail" ["--follow=name", "nonexist"];
    tail_follow_missing_retry_dir: "tail" ["--follow=descriptor", "--retry", "."];
    tail_follow_directory: "tail" ["-f", "."];
    tail_follow_piped_stdin: "tail" ["-f", "-c3"] stdin b"foo\n";
    tail_follow_piped_stdin_and_missing: "tail" ["-f", "-c3", "-", "nonexist"] stdin b"bar\n";
    tail_follow_dead_pid: "tail" ["-f", "-s.1", "--pid=2147483647", "-n", "2", "many.txt"];
    tail_follow_dead_pid_polling: "tail" ["---disable-inotify", "-f", "-s.1", "--pid=2147483647", "lines.txt", "many.txt"];
    tail_follow_dead_pid_n0: "tail" ["-n0", "-F", "-s.1", "--pid=2147483647", "many.txt"];

    // ----------------------------------------------------------------- wc
    wc_files0_from: "wc" ["--files0-from=names0.txt"];
    wc_files0_from_stdin: "wc" ["-l", "--files0-from=-"] stdin b"lines.txt\0many.txt";
    wc_files0_from_bad_names: "wc" ["--files0-from=names0-bad.txt"];
    wc_files0_from_dash_from_stdin: "wc" ["--files0-from=-"] stdin b"-\0lines.txt\0";
    wc_files0_from_dash_in_file: "wc" ["-c", "--files0-from=names0-dash.txt"] stdin b"abc";
    wc_files0_from_extra_operand: "wc" ["--files0-from=names0.txt", "lines.txt"];
    wc_files0_from_missing: "wc" ["--files0-from=nonexist"];
    wc_files0_from_empty: "wc" ["--total=always", "--files0-from=-"] stdin b"";
    wc_total_only: "wc" ["--total=only", "lines.txt", "many.txt"];
    wc_total_always_one: "wc" ["--total=always", "lines.txt"];
    wc_total_never: "wc" ["--total=never", "lines.txt", "many.txt"];
    wc_total_abbreviated: "wc" ["--total=n", "lines.txt", "many.txt"];
    wc_total_invalid: "wc" ["--total", "lines.txt", "many.txt"];
    wc_directory: "wc" [".", "lines.txt"];
    wc_bytes_regular: "wc" ["-c", "many.txt", "lines.txt"];
    wc_chars_c_locale: "wc" ["-m", "u8.txt"];
    wc_chars_bytes_c_locale: "wc" ["-mc", "u8.txt"];
    wc_chars_stdin_c_locale: "wc" ["-m"] stdin b"h\xc3\xa9llo\n";
    wc_all_c_locale: "wc" ["-lwmcL", "u8.txt"];
    wc_width_c_locale: "wc" ["-L", "u8.txt"];
    wc_words_c_locale: "wc" ["-w", "u8.txt"];
    wc_chars_utf8: "wc" ["-m", "u8.txt"] @ "C.UTF-8";
    wc_chars_bytes_utf8: "wc" ["-mc", "u8.txt"] @ "C.UTF-8";
    wc_all_utf8: "wc" ["-lwmcL", "u8.txt"] @ "C.UTF-8";
    wc_chars_stdin_utf8: "wc" ["-m"] @ "C.UTF-8" stdin b"h\xc3\xa9llo\n";
    wc_files0_from_one_name: "wc" ["--files0-from=-"] stdin b"lines.txt\0";

    // --------------------------------------------------------------- sort
    sort_zero: "sort" ["-z", "nul.txt"];
    sort_zero_key_newline_is_blank: "sort" ["-z", "-k2,2"] stdin b"1\n2\x002\n1\0";
    sort_zero_blanks: "sort" ["-zb", "-k2,2"] stdin b"1\n\n2\x002\n1\0";
    sort_zero_unique: "sort" ["-zu", "nul.txt"];
    sort_files0_from: "sort" ["--files0-from=names0.txt"];
    sort_files0_from_stdin: "sort" ["--files0-from=-"] stdin b"lines.txt\0many.txt";
    sort_files0_from_bad: "sort" ["--files0-from=names0-bad.txt"];
    sort_files0_from_dash: "sort" ["--files0-from=names0-dash.txt"];
    sort_files0_from_empty: "sort" ["--files0-from=-"] stdin b"";
    sort_files0_from_missing: "sort" ["--files0-from=nonexist"];
    sort_files0_from_extra: "sort" ["--files0-from=names0.txt", "lines.txt"];

    // --------------------------------------------------------------- uniq
    uniq_all_repeated: "uniq" ["-D", "dup.txt"];
    uniq_all_repeated_separate: "uniq" ["--all-repeated=separate", "dup.txt"];
    uniq_all_repeated_prepend: "uniq" ["--all-repeated=prepend", "dup.txt"];
    uniq_all_repeated_bad_method: "uniq" ["--all-repeated=x", "dup.txt"];
    uniq_all_repeated_with_count: "uniq" ["-Dc", "dup.txt"];
    uniq_group: "uniq" ["--group", "dup.txt"];
    uniq_group_both: "uniq" ["--group=both", "dup.txt"];
    uniq_group_append: "uniq" ["--group=append", "dup.txt"];
    uniq_group_with_unique: "uniq" ["--group", "-u", "dup.txt"];
    uniq_zero: "uniq" ["-z", "-c", "nul.txt"];
    uniq_obsolete_fields: "uniq" ["-1"] stdin b"a x\nb x\nc y\n";
    uniq_obsolete_digits_accumulate: "uniq" ["-1", "-0"] stdin b"a b c d e f g h i j k\n";
    uniq_obsolete_chars: "uniq" ["+1"] stdin b"ax\nbx\ncy\n";
    uniq_obsolete_chars_posix2001: "uniq" ["+1"] env [("_POSIX2_VERSION", "200112")] stdin b"ax\n";
    uniq_huge_skip_fields: "uniq" ["-f", "10000000000"] stdin b"a\nb\n";
    uniq_huge_skip_chars: "uniq" ["-s", "99999999999999999999999"] stdin b"a\nb\n";
    uniq_bad_skip: "uniq" ["-f", "x"] stdin b"a\n";
    uniq_extra_operand: "uniq" ["dup.txt", "out", "extra"];
    uniq_no_trailing_newline: "uniq" [] stdin b"a\na";

    // ---------------------------------------------------------------- cut
    cut_nul_delimiter: "cut" ["-d", "", "--output-delimiter=_", "-f2,3"] stdin b"a\0b\0c\n";
    cut_newline_delimiter: "cut" ["-d", "\n", "-f1"] stdin b"a:1\nb:";
    cut_newline_delimiter_s: "cut" ["-s", "-d", "\n", "-f2"] stdin b"\nb";
    cut_newline_delimiter_join: "cut" ["-d", "\n", "-f1-", "--output-delimiter=:"] stdin b"a\nb\n";
    cut_zero: "cut" ["-z", "-d:", "-f1"] stdin b"a:1\0b:2";
    cut_zero_bytes: "cut" ["-z", "-c1"] stdin b"ab\0cd";
    cut_nul_output_delimiter: "cut" ["-d:", "--output-delimiter=", "-f2,3"] stdin b"a:b:c\n";
    cut_abbreviated_option: "cut" ["-d:", "--out=_", "-f2,3"] stdin b"a:b:c\n";
    cut_only_delimited: "cut" ["-s", "-d:", "-f2", "fields.txt"];
    cut_complement: "cut" ["--complement", "-d:", "-f2", "fields.txt"];
    cut_byte_ranges_delimited: "cut" ["-b1-2,4-", "--output-delimiter=|", "fields.txt"];
    cut_huge_offset: "cut" ["-b18446744073709551615", "fields.txt"];
    cut_huge_offset_open_range: "cut" ["-b18446744073709551614-", "fields.txt"];
    cut_field_too_large: "cut" ["-f99999999999999999999", "fields.txt"];
    cut_decreasing_range: "cut" ["-f", "2-0", "fields.txt"];
    cut_double_dash_range: "cut" ["-b", "1-2-3", "fields.txt"];
    cut_field_range_double_dash: "cut" ["-f", "1--2", "fields.txt"];
    cut_no_endpoint: "cut" ["-f", "1,-", "fields.txt"];
    cut_invalid_value: "cut" ["-f", "1x", "fields.txt"];
    cut_missing_list: "cut" ["-f", "--", "fields.txt"];
    cut_empty_list: "cut" ["-b", "", "fields.txt"];
    cut_two_lists: "cut" ["-f1", "-b2", "fields.txt"];
    cut_delimiter_with_bytes: "cut" ["-d", "", "-b1", "fields.txt"];
    cut_multichar_delimiter: "cut" ["-d", "ab", "-f1", "fields.txt"];
    cut_no_list: "cut" ["fields.txt"];
    cut_missing_file: "cut" ["-f1", "nonexist", "fields.txt"];

    // ----------------------------------------------------------------- tr
    tr_upper_class_lines_up: "tr" ["[:lower:][:lower:]", "[:upper:]0-9"] stdin b"abcxyz\n";
    tr_misaligned_case_class: "tr" ["A-Z[:lower:]", "a-y[:upper:]"] stdin b"";
    tr_set2_ends_in_class: "tr" ["[:upper:] ", "[:lower:]"] stdin b"";
    tr_repeat_of_colon: "tr" ["[:*3][:digit:]", "a-m"] stdin b":1239";
    tr_bad_octal_repeat: "tr" ["[b*08]", "[x*]"] stdin b"";
    tr_backslash_at_end: "tr" ["\\", "x"] stdin b"\\";
    tr_complement_class_not_homogeneous: "tr" ["-cs", "[:upper:]", "X[Y*]"] stdin b"";
    tr_complement_class_homogeneous: "tr" ["-cs", "[:upper:]", "[X*]"] stdin b"AMZamz123.-+AMZ";
    tr_empty_equiv: "tr" ["[==]", "x"] stdin b"";
    tr_empty_class: "tr" ["[::]", "x"] stdin b"";
    tr_bad_class: "tr" ["[:foo:]", "x"] stdin b"";
    tr_equiv_too_long: "tr" ["[=ab=]", "x"] stdin b"";
    tr_equiv_in_set2: "tr" ["a", "[=b=]"] stdin b"a";
    tr_class_in_set2: "tr" ["a", "[:digit:]"] stdin b"a";
    tr_repeat_in_set1: "tr" ["[a*]", "x"] stdin b"a";
    tr_two_repeats_in_set2: "tr" ["ab", "[x*][y*]"] stdin b"a";
    tr_repeat_not_translating: "tr" ["-ds", "a", "[x*]"] stdin b"a";
    tr_reverse_range: "tr" ["z-a", "x"] stdin b"a";
    tr_ambiguous_octal: "tr" ["\\400", "x"] stdin b" 0";
    tr_repeat_escaped_count: "tr" ["abc", "[b*\\9]"] stdin b"abcd";
    tr_repeat_zeros: "tr" ["abc", "[b*00000000000000000000]"] stdin b"abcd";
    tr_complement_repeat: "tr" ["-c", "[a*65536]\\n", "[b*]"] stdin b"abcd";
    tr_missing_operand_after: "tr" ["-ds", "a"] stdin b"";
    tr_extra_operand: "tr" ["-d", "a", "b"] stdin b"";
    tr_empty_set2: "tr" ["a", ""] stdin b"";

    // ---------------------------------------------------------------- tac
    tac_before: "tac" ["-b"] stdin b"\na\nb\nc";
    tac_separator: "tac" ["-s:"] stdin b"a:b:c:";
    tac_separator_before: "tac" ["-s", ":", "-b"] stdin b":a:b:c";
    tac_multichar_separator: "tac" ["-s", "::"] stdin b"1::2:::3::";
    tac_nul_separator: "tac" ["-s", ""] stdin b"a\0b\0c";
    tac_regex: "tac" ["-r", "-s", "\\._+"] stdin b"a.___b.__1._2.__3.___4._";
    tac_regex_before: "tac" ["-b", "-r", "-s", "\\._+"] stdin b".__x.___y.____z._1._2.__3.___4";
    tac_regex_alternation: "tac" ["-r", "-s", "x\\|yy"] stdin b"1x2yy3x";
    tac_regex_group_star: "tac" ["-r", "-s", "\\(ab\\)*c"] stdin b"1c2ababc3abc";
    tac_regex_bracket: "tac" ["-r", "-s", "[0-9]+"] stdin b"a1b22c333";
    tac_regex_literal_star: "tac" ["-r", "-s", "*a"] stdin b"1*a2*a3";
    tac_regex_empty: "tac" ["-r", "-s", ""] stdin b"a";
    tac_regex_unmatched_open: "tac" ["-r", "-s", "\\("] stdin b"a";
    tac_regex_unmatched_close: "tac" ["-r", "-s", "a\\)"] stdin b"a";
    tac_regex_unmatched_bracket: "tac" ["-r", "-s", "[a"] stdin b"a";
    tac_regex_trailing_backslash: "tac" ["-r", "-s", "a\\"] stdin b"a";
    tac_regex_bad_backref: "tac" ["-r", "-s", "\\1"] stdin b"a";
    tac_regex_default_separator: "tac" ["-r"] stdin b"a\nb\n";
    tac_two_files: "tac" ["lines.txt", "notrail.txt"];
    tac_missing_file: "tac" ["nonexist", "lines.txt"];

    // ------------------------------------------------------------- printf
    printf_width_too_large: "printf" ["%*d", "2147483648", "15"];
    printf_width_too_small: "printf" ["%*d", "-2147483649", "15"];
    printf_precision_too_large: "printf" ["%.*d", "2147483648", "15"];
    printf_precision_negative: "printf" ["%.*d", "-3", "15"];
    printf_precision_huge_negative: "printf" ["10 %.*dx", "-2147483649", "0"];
    printf_width_not_numeric: "printf" ["%*d", "x", "1"];
    printf_hash_d: "printf" ["%#d", "0"];
    printf_zero_s: "printf" ["%0s", "0"];
    printf_precision_c: "printf" ["%.9c", "0"];
    printf_quote_s: "printf" ["%'s", "0"];
    printf_quote_d: "printf" ["%'d", "3333"];
    printf_width_b: "printf" ["%5b", "x"];
    printf_char_constant_trailing: "printf" ["%d\n", "\"a\""];
    printf_char_constant_alone: "printf" ["%d\n", "\""];
    printf_char_constant_utf8: "printf" ["%d\n", "'\u{e9}"] @ "C.UTF-8";
    printf_char_constant_c_locale: "printf" ["%d\n", "'\u{e9}"];
    printf_leading_space_quote: "printf" ["%d\n", " 'a"];
    printf_u_short: "printf" ["\\u00"];
    printf_big_u_short: "printf" ["\\U0000"];
    printf_u_surrogate: "printf" ["\\ud800"];
    printf_u_ok: "printf" ["\\u0030"];
    // --------------------------------------------------------------- echo
    echo_octal_without_zero: "echo" ["-n", "-e", "\\x1b\\n\\e\\n\\33\\n\\033\\n\\0033\\n"];
    echo_no_hex_digits: "echo" ["-e", "\\x"];
    echo_capital_e_not_escape: "echo" ["-e", "\\E"];
    echo_stop_output: "echo" ["-e", "foo\\n\\cbar"];
    echo_dash: "echo" ["-"];

    // -------------------------------------------------------------- paste
    paste_zero: "paste" ["-z", "nul.txt", "nul.txt"];
    paste_zero_serial: "paste" ["-z", "-s", "-d", " ", "nul.txt"];
    paste_trailing_backslash: "paste" ["-d", "\\", "lines.txt"];
    paste_trailing_backslash_after_text: "paste" ["-d", "ab\\", "lines.txt"];
    paste_empty_delimiter: "paste" ["-d", "", "lines.txt", "many.txt"];
    paste_delimiter_escapes: "paste" ["-d", "\\0\\t,", "lines.txt", "many.txt", "notrail.txt"];
    paste_uneven: "paste" ["notrail.txt", "many.txt", "lines.txt"];
    paste_serial_many: "paste" ["-s", "-d", ",:", "many.txt", "notrail.txt", "empty.txt"];
    paste_stdin_twice: "paste" ["-", "-"] stdin b"1\n2\n3\n4\n5\n";
    paste_serial_stdin_twice: "paste" ["-s", "-", "-"] stdin b"1\n2\n";
    paste_missing: "paste" ["lines.txt", "nonexist"];
    paste_serial_missing: "paste" ["-s", "nonexist", "lines.txt"];
    paste_binary: "paste" ["-s"] stdin b"\xff\xfe\n\x80";

    // ------------------------------------------------------------- mktemp
    mktemp_quiet_too_few_x: "mktemp" ["-q", "foo.XX"];
    mktemp_suffix_template_quoted: "mktemp" ["-d", "--suffix=aXXXXb", ""];
    mktemp_suffix_not_x: "mktemp" ["--suffix=b", "aXXa"];
    mktemp_quiet_create_failure: "mktemp" ["-q", "nonexist/fooXXXX"];
    // --------------------------------------------------------------- shuf
    shuf_single_range: "shuf" ["-i", "5-5"];
    shuf_zero_terminated: "shuf" ["-z", "-i", "1-1"];
    shuf_echo_one: "shuf" ["-e", "a"];
    shuf_head_zero_unreadable: "shuf" ["-n0", "nonexist"];
    shuf_repeat_head_zero: "shuf" ["-r", "-n", "0", "lines.txt"];
    shuf_echo_and_range: "shuf" ["-i0-9", "-e", "A"];
    shuf_bad_count: "shuf" ["-nA"];
    shuf_two_ranges: "shuf" ["-i0-9", "-i8-90"];
    shuf_range_no_dash: "shuf" ["-i1"];
    shuf_range_no_hi: "shuf" ["-i1-"];
    shuf_range_bad_hi: "shuf" ["-i1-A"];
    shuf_range_reversed: "shuf" ["-i2-1"];
    shuf_range_too_large: "shuf" ["-i1-99999999999999999999"];
    shuf_range_operand: "shuf" ["-i0-0", "1"];
    shuf_two_outputs: "shuf" ["-i0-9", "-o", "A", "-o", "B"];
    shuf_two_random_sources: "shuf" ["-i0-9", "--random-source", "A", "--random-source", "B"];
    shuf_echo_repeat_nothing: "shuf" ["-er"];
    shuf_extra_operand: "shuf" ["lines.txt", "many.txt"];
    shuf_missing_file: "shuf" ["nonexist"];
    shuf_one_line_no_newline: "shuf" [] stdin b"x";
    // ----------------------------------------------------------------- nl
    nl_sections: "nl" ["-ha", "-fa", "sections.txt"];
    nl_sections_no_renumber: "nl" ["-p", "-ha", "-fa", "sections.txt"];
    nl_section_delimiter_empty: "nl" ["-d", "", "sections.txt"];
    nl_section_delimiter_one_char: "nl" ["-d", "x"] stdin b"a\nx:x:\nc\n";
    nl_section_delimiter_long: "nl" ["-d", "foo"] stdin b"a\nfoofoo\nc\n";
    nl_regex_style: "nl" ["-bp[ac]", "many.txt"];
    nl_regex_style_interval: "nl" ["-bp^1\\{2\\}"] stdin b"1\n11\n111\n";
    nl_regex_error: "nl" ["-bp\\(", "many.txt"];
    nl_join_blank_lines: "nl" ["-ba", "-l2"] stdin b"a\n\n\n\n\nb\n";
    nl_start_too_large: "nl" ["-v9223372036854775808", "many.txt"];
    nl_start_max: "nl" ["-v9223372036854775807"] stdin b"a\n\\:\\:\nb\n";
    nl_overflow: "nl" ["-p", "-v9223372036854775807"] stdin b"a\n\\:\\:\nb\n";
    nl_increment_too_small: "nl" ["-i-9223372036854775809", "many.txt"];
    nl_increment_min: "nl" ["-v9223372036854775807", "-i-9223372036854775808"] stdin b"a\nb\n";
    nl_bad_style: "nl" ["-bx", "-hy", "many.txt"];
    nl_bad_format: "nl" ["-nxx", "many.txt"];
    nl_bad_width: "nl" ["-w0", "many.txt"];
    nl_bad_join: "nl" ["-l0", "many.txt"];
    nl_rz_negative: "nl" ["-n", "rz", "-v", "-5"] stdin b"a\nb\n";
    nl_left: "nl" ["-n", "ln", "-s", "%n"] stdin b"a\nb";
    // ------------------------------------- JSON lines are text when piped
    json_lines_sort: "sort" [] stdin b"{\"name\": \"b\"}\n{\"name\":\"a\"}\n";
    json_lines_head: "head" ["-n", "1"] stdin b"{\"a\":  1}\n{\"b\": 2}\n";
    json_lines_tail: "tail" ["-n", "1"] stdin b"{\"a\":  1}\n{\"b\": 2}\n";
    json_lines_uniq: "uniq" [] stdin b"{\"a\":  1}\n{\"a\":  1}\n";
    json_lines_cut: "cut" ["-c1-5"] stdin b"{\"a\":  1}\n";
    json_lines_cut_fields: "cut" ["-f1"] stdin b"{\"a\":  1}\n";

    // ------------------------------- file names that are not UTF-8
    // `@xff@` is the byte 0xff: the file is opened by its exact bytes, named
    // unquoted in output (headers, counts) and shell-escaped in messages.
    cat_non_utf8: "cat" ["odd@xff@name.txt", "nope@xff@", "one@xff@.txt"];
    cat_number_non_utf8: "cat" ["-n", "odd@xff@name.txt", "nope@xff@"];
    head_non_utf8: "head" ["-n", "1", "odd@xff@name.txt", "lines.txt", "nope@xff@"];
    head_non_utf8_elide_bytes: "head" ["-v", "-c", "-2", "odd@xff@name.txt"];
    tail_non_utf8: "tail" ["-n", "1", "odd@xff@name.txt", "lines.txt", "nope@xff@"];
    wc_non_utf8: "wc" ["odd@xff@name.txt", "nope@xff@", "one@xff@.txt"];
    wc_non_utf8_newline: "wc" ["-l", "nl\n@xff@", "one@xff@.txt"];
    wc_files0_from_non_utf8_names: "wc" ["--files0-from=names0-odd.txt"];
    wc_files0_from_non_utf8_stdin: "wc" ["-c", "--files0-from=-"] stdin b"odd\xffname.txt\0one\xff.txt";
    wc_files0_from_non_utf8_list: "wc" ["--files0-from=nope@xff@"];
    wc_files0_from_non_utf8_extra: "wc" ["--files0-from=-", "x@xff@"];
    sort_non_utf8: "sort" ["odd@xff@name.txt", "one@xff@.txt"];
    sort_non_utf8_missing: "sort" ["odd@xff@name.txt", "nope@xff@"];
    sort_files0_from_non_utf8_names:"sort" ["--files0-from=names0-odd.txt"];
    sort_files0_from_non_utf8_stdin: "sort" ["--files0-from=-"] stdin b"odd\xffname.txt\0one\xff.txt";
    sort_files0_from_non_utf8_list: "sort" ["--files0-from=nope@xff@"];
    sort_files0_from_non_utf8_extra: "sort" ["--files0-from=-", "x@xff@"];
    uniq_non_utf8: "uniq" ["-c", "odd@xff@name.txt"];
    uniq_non_utf8_missing: "uniq" ["nope@xff@"];
    uniq_non_utf8_output_unwritable: "uniq" ["odd@xff@name.txt", "nodir@xff@/out"];
    uniq_non_utf8_extra_operand: "uniq" ["a", "b", "c@xff@"];
    cut_non_utf8: "cut" ["-d:", "-f1", "odd@xff@name.txt", "nope@xff@"];
    tac_non_utf8: "tac" ["odd@xff@name.txt", "nope@xff@", "one@xff@.txt"];
    paste_non_utf8: "paste" ["odd@xff@name.txt", "one@xff@.txt"];
    paste_non_utf8_missing: "paste" ["-s", "one@xff@.txt", "nope@xff@"];
    nl_non_utf8: "nl" ["odd@xff@name.txt", "nope@xff@", "one@xff@.txt"];
    shuf_non_utf8: "shuf" ["one@xff@.txt"];
    shuf_non_utf8_missing: "shuf" ["nope@xff@"];
    shuf_non_utf8_output_unwritable: "shuf" ["-o", "nodir@xff@/out", "one@xff@.txt"];
    shuf_non_utf8_random_source: "shuf" ["--random-source=nope@xff@", "lines.txt"];
    shuf_non_utf8_extra_operand: "shuf" ["a", "b@xff@"];
    shuf_directory: "shuf" ["."];
    base64_non_utf8: "base64" ["one@xff@.txt"];
    base64_non_utf8_missing: "base64" ["nope@xff@"];
    base64_non_utf8_extra_operand: "base64" ["a", "b@xff@"];
    basenc_non_utf8: "basenc" ["--base32", "one@xff@.txt"];
    date_file_non_utf8: "date" ["-f", "dates@xff@.txt", "+%s"] env [("TZ", "UTC0")];
    date_file_non_utf8_missing: "date" ["-f", "nope@xff@"];
    date_reference_non_utf8: "date" ["-r", "one@xff@.txt", "+%Y"] env [("TZ", "UTC0")];
    date_reference_non_utf8_missing: "date" ["-r", "nope@xff@"];
}

// ------------------------------------------------------- shell pipelines

/// Runs `script` under `sh -c` with `tools` resolving to Dogfish, then to
/// GNU, each in a fresh directory, and compares stdout, stderr and status.
fn check_shell(name: &str, tools: &[&str], script: &str) {
    let mut gnu_paths = Vec::new();
    for tool in tools {
        let Some(path) = gnu_tool(tool) else {
            eprintln!("note: skipping — system `{tool}` is missing or not GNU coreutils");
            return;
        };
        gnu_paths.push(path);
    }
    let base = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("gnu_compat_text-shell")
        .join(name);
    let _ = std::fs::remove_dir_all(&base);
    let run = |which: &str, target: &dyn Fn(usize) -> PathBuf| {
        let bin = base.join(which).join("bin");
        let work = base.join(which).join("work");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        for (i, tool) in tools.iter().enumerate() {
            std::os::unix::fs::symlink(target(i), bin.join(tool)).unwrap();
        }
        let path = format!("{}:/usr/bin:/bin", bin.display());
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script).env("PATH", path);
        exec(cmd, &work, None, "C")
    };
    let ours = run("dogfish", &|_| PathBuf::from(dogfish_bin()));
    let theirs = run("gnu", &|i| gnu_paths[i].clone());
    let show = |o: &std::process::Output| {
        format!(
            "status={:?}\n--- stdout ---\n{:?}\n--- stderr ---\n{:?}",
            o.status.code(),
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        )
    };
    let ctx = format!(
        "\nscript: {script}\n=== dogfish ===\n{}\n=== GNU ===\n{}\n",
        show(&ours),
        show(&theirs)
    );
    assert_eq!(ours.stdout, theirs.stdout, "stdout differs{ctx}");
    assert_eq!(ours.stderr, theirs.stderr, "stderr differs{ctx}");
    assert_eq!(
        ours.status.code(),
        theirs.status.code(),
        "status differs{ctx}"
    );
}

/// `name: [tools...] "script";`
macro_rules! shell_cases {
    ($( $name:ident : [$($tool:literal),* $(,)?] $script:expr; )*) => {
        $(
            #[test]
            fn $name() {
                check_shell(stringify!($name), &[$($tool),*], $script);
            }
        )*
    };
}

shell_cases! {
    // ---------------------------------------------------------------- cat
    cat_input_is_output: ["cat"] "echo x > out; cat out >> out; echo $?; cat out";
    cat_input_is_output_truncated: ["cat"] "echo x > doc; echo y > doc.end; cat doc doc.end > doc; echo $?; cat doc";
    cat_input_is_output_stdin: ["cat"] "echo x > f; cat - < f >> f; echo $?; cat f";
    cat_v_input_is_output: ["cat"] "printf 'a\\tb\\n' > f; cat -vT f >> f; echo $?; cat f";

    // --------------------------------------------------------------- head
    head_lines_leaves_offset: ["head"] "seq 5 > in; (head -n 2 > /dev/null; cat) < in";
    head_lines_leaves_offset_long: ["head"] "seq 20000 > in; (head -n 3 > /dev/null; wc -c) < in";
    head_bytes_leaves_offset: ["head"] "echo abc > in; (head -c1; head -c1) < in";
    head_elide_bytes_from_offset: ["head"] "printf 'abc\\ndef\\n' > in; (dd bs=1 skip=1 count=0 status=none; head -c -4) < in";
    head_elide_lines_leaves_offset: ["head"] "seq 70000 > in; (head -n -50000 > /dev/null; wc -l) < in";
    head_elide_lines_small_file: ["head"] "seq 10 > in; (head -n -3 > /dev/null; cat) < in";
    head_elide_bytes_huge_count: ["head"] "head --bytes=-9223372036854775807 < /dev/null; echo $?";
    head_closed_stdin: ["head"] "head <&-; echo $?";

    // --------------------------------------------------------------- tail
    tail_follow_closed_stdin: ["tail"] "tail -f - <&-; echo $?";
    tail_follow_closed_stdout: ["tail"] "touch f; tail -f f >&-; echo $?";
    tail_follow_regular_stdin: ["tail"] "seq 5 > f; timeout .5 tail -n2 -s.1 -f < f; echo $?";
    tail_follow_regular_stdin_polling: ["tail"] "seq 5 > f; timeout .5 tail ---dis -s.1 -f -n1 - f < f; echo $?";
    tail_follow_fifo: ["tail"] "mkfifo p; (echo 1; echo 2) > p & timeout 1 tail -s.1 -f p; echo $?";
    tail_follow_dev_null: ["tail"] "timeout .5 tail -s.1 -f /dev/null; echo $?";
    tail_follow_reader_gone: ["tail"] "seq 5 > f; (trap '' PIPE; timeout 5 tail -s.1 -n2 -f f; echo $? >&2) | head -n1";

    // ----------------------------------------------------------------- wc
    wc_bytes_twice_shares_offset: ["wc"] "printf ab > in; { wc -c; wc -c; } < in";
    wc_bytes_page_multiple: ["wc"] "head -c 1048576 /dev/zero > in; { wc -c; wc -c; } < in";
    wc_bytes_from_offset: ["head", "wc"] "seq 100 > in; (head -n 10 > /dev/null; wc -c) < in";
    wc_bytes_sparse_not_read: ["wc"] "truncate -s 1T big 2>/dev/null || exit 0; wc -c big";
    wc_newline_in_name: ["wc"] "touch 'a\nb'; printf 'a\\nb\\0' | wc --files0-from=-";
    wc_closed_stdin: ["wc"] "wc <&-; echo $?";

    // ---------------------------------------------------------------- tac
    tac_closed_stdin: ["tac"] "tac - - <&-; echo $?";
    tac_pipe_twice: ["tac"] "printf 'x\\ny\\n' | tac - -";

    // ---------------------------------------------- echo / mktemp, POSIX
    echo_posixly_correct: ["echo"] "export POSIXLY_CORRECT=1; env echo -n -E 'foo\\n'; env echo -nE foo; env echo -E -n foo; env echo --version";
    mktemp_posixly_correct: ["mktemp"] "POSIXLY_CORRECT=1 mktemp aXXXX --suffix=b; echo $?";

    // --------------------------------------------------------------- sort
    // -m: a streaming merge, stable across inputs, with -u and keys.
    sort_merge: ["sort"] "printf '1\\n3\\n5\\n' > a; printf '2\\n4\\n6' > b; sort -m a b";
    sort_merge_reverse_keys: ["sort"] "printf 'a 2\\nb 1\\n' > a; printf 'a 1\\nb 2\\n' > b; sort -m -s -k1,1 b a; sort -mr -k2n a";
    sort_merge_unique: ["sort"] "printf 'a\\na\\nb\\n' > a; printf 'a\\nb\\nb\\n' > b; sort -mu a b; sort -m -u -k1,1 a b";
    sort_merge_stdin: ["sort"] "printf '1\\n3\\n' > a; printf '2\\n4\\n' | sort -m a -";
    sort_merge_output_is_input: ["sort"] "printf 'a\\nc\\n' > f; printf 'b\\n' > g; sort -m f g -o f; cat f; sort -m g f > g; cat g";
    sort_merge_batches: ["sort"] "for i in $(seq 40); do echo $i > f$i; done; sort -n -m --batch-size=3 f* | paste -s -d ' '";
    sort_merge_batches_no_tmpdir: ["sort"] "for i in 1 2 3; do echo $i > f$i; done; sort -m --batch-size=2 -T nonexist f*; echo $?";
    sort_merge_fd_limit: ["sort"] "for i in $(seq 17); do echo $i > f$i; done; (ulimit -n 12; sort -n -m --batch-size=8 f* | paste -s -d ' ')";
    sort_merge_missing: ["sort"] "sort -m nonexist; echo $?; mkdir d; sort -m d; echo $?";
    sort_batch_size_errors: ["sort"] "for a in 0 1 -1 a 10x 99999999999999999999; do sort -m --batch-size=$a /dev/null 2>&1 | sed 's/is [0-9]*$/is N/'; done";
    // -S: sorted buffers spill to temporary files, then merge.
    sort_buffer_spill: ["sort"] "seq -w 3000 | sort -r -S 2k | md5sum; seq 3000 | sort -S 1k -u -k1.1,1.1 | paste -s -d ' '";
    sort_buffer_spill_stable: ["sort"] "seq 3000 | sort -S 2k -s -k1.1,1.1 --batch-size=2 | md5sum";
    sort_buffer_spill_no_tmpdir: ["sort"] "seq 3000 | sort -S 1k -T nonexist; echo $?; seq 10 | sort -S 1M -T nonexist | wc -l";
    sort_buffer_size_errors: ["sort"] "for s in 1x -1 1kb 99999999999999999999; do sort -S $s /dev/null; sort --buffer-size=$s /dev/null; done 2>&1; sort -S 10b -S 10% /dev/null; echo $?";
    sort_compress_program: ["sort"] "printf 'tr 41 14\\ntouch ok\\n' > z; chmod +x z; seq -w 2000 | tac > in; sort -S 1k --compress-program=./z in | md5sum; ls ok";
    sort_parallel: ["sort"] "printf '2\\n1\\n2\\n' | sort --parallel=1 -u; printf '2\\n1\\n' | sort --parallel=2 -S 10b; sort --parallel=0 /dev/null; sort --parallel=x /dev/null; echo $?";
    // -c / -C.
    sort_check_quiet: ["sort"] "printf 'b\\na\\n' | sort -C; echo $?; printf 'a\\na\\n' | sort -cu; echo $?; printf 'a\\0c\\0b\\0' | sort -cz | od -c";
    sort_check_incompatible: ["sort"] "sort -cC /dev/null; sort -c -o x /dev/null; sort -C -o x /dev/null; sort -c a b; sort --debug -c /dev/null; sort --debug -o x /dev/null; echo $?";
    // Keys and ordering options.
    sort_key_errors: ["sort"] "for k in 0 1.0 1,0 1x 1,2x 1. , 1.1,-k0 2.,3 2,; do sort -k$k /dev/null; done 2>&1; echo $?";
    sort_incompatible_options: ["sort"] "sort -dfgiMnR /dev/null; sort -in /dev/null; sort -k1,1nR /dev/null; sort --sort=random -n /dev/null; echo $?";
    sort_obsolete_keys: ["sort"] "printf 'a 2\\nb 1\\n' > f; sort +1 -2 f; sort +1x f; _POSIX2_VERSION=200112 sort +1 f; echo $?";
    sort_general_long_double: ["sort"] "printf '1.189731495357231765e+4932\\n-3.3621031431120935063e-4932\\n0\\n3.3621031431120935063e-4932\\n2.2250738585072014e-308\\n1.0000000000000000001\\n1\\n-nan\\nnan\\nx\\n1e5000\\n' | sort -gs";
    sort_general_nan_merge: ["sort"] "echo nan > F; sort -g -m F F";
    sort_output_dash_is_a_file: ["sort"] "printf 'b\\na\\n' > f; sort -o - f; echo; cat ./-";
    sort_output_file_full: ["sort"] "seq 3 > a; seq 30000 > b; for o in '' -m '-S 1k'; do sort $o -o /dev/full a; echo $?; sort $o -o /dev/full b b; echo $?; done";
    sort_merge_spill_closed_stdout: ["sort"] "seq 3 > a; seq 30000 > b; sort -m a >&-; echo $?; sort -S 1k b >&-; echo $?";
    sort_stdin_closed: ["sort"] "sort - <&-; echo $?; sort -m - <&-; echo $?";
    // --debug: key underlining and warnings.
    sort_debug_keys: ["sort"] "printf '1\\n\\n44\\n33\\n2\\n' | sort -s -k2n --debug; printf ' 1.2ignore\\n 1.1e4ignore\\n' | sort -s -g --debug; printf 'A\\tchr10\\nB\\tchr1\\n' | sort -s -k2.4b,2.3n --debug";
    sort_debug_month_human: ["sort"] "printf 'FEB\\nJAZZ\\n\\nJAN\\n' | sort -s -k1M --debug; printf -- '-0\\n1\\n-2\\n--Mi-1\\n-3\\n-0\\n' | sort -s --debug -k1,1h";
    sort_debug_last_resort: ["sort"] "printf ' 1\\n1\\n' | sort -b --debug; printf 'b a\\na b\\n' | sort -u -k2,2 --debug; printf 'a\\nb\\n' | sort -m --debug";
    sort_debug_warnings: ["sort"] "sort -s -rRVMhgb -k1,1n --debug /dev/null; sort -gbr -k1,1n -k1,1r --debug /dev/null; sort --debug -rb -k2n +2.2 -1b /dev/null; sort -nk1 -t- --debug /dev/null; sort -gk1 -t+ --debug /dev/null; sort -nk1 -t. --debug /dev/null";
    sort_debug_locale: ["sort"] "LC_ALL=missing sort --debug /dev/null";

    // ----------------------------------------------------------- cp --debug
    cp_debug: ["cp"] "echo hi > f; cp --debug f g; cp --debug --reflink=never f h; cp --debug --sparse=always f i; truncate -s 1M s; cp --debug s t";
    cp_debug_no_data_copy: ["cp"] "touch f; cp --debug --attributes-only f g; echo $?; cp --debug -l f h; cp --debug -s f j";
    cp_debug_skipped: ["cp"] "touch f g; cp --debug --update=none f g; echo $?; cp --debug -u f g; echo $?";

    // --------------------------------------------------------------- shuf
    shuf_is_permutation: ["shuf"] "seq 100 > in; shuf in | sort -n | cmp - in && echo ok";
    shuf_range_is_permutation: ["shuf"] "shuf -i 1-100 | sort -n | md5sum";
    shuf_min_head_count: ["shuf"] "shuf -n10 -i0-9 -n3 -n20 | wc -l";
    shuf_huge_range_subset: ["shuf"] "shuf -i1-18446744073709551615 -n2 | wc -l";
    shuf_repeat_count: ["shuf"] "shuf --rep -i0-9 -n1000 | sort -nu | paste -s -d ' '";
    shuf_repeat_forever: ["shuf"] "shuf --rep -e A B | head -n 1000 | wc -l";
    shuf_closed_stdin_file: ["shuf"] "shuf /dev/null <&-; echo $?";
}
