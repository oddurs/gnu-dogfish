//! Differential GNU-compatibility suite for the shell & system tools
//! (`sys.rs`) and the hashing & encoding tools (`hash.rs`), plus GNU-style
//! diagnostics for unknown options across every tool GNU coreutils also has.
//!
//! Every case runs the same arguments (and stdin) through `dogfish <tool>` and
//! through the system GNU coreutils binary, then compares stdout, exit status
//! and (unless a case opts out) stderr byte-for-byte. Cases that depend on the
//! time zone pin `TZ`. When the system tool is missing or is not GNU coreutils
//! (e.g. BSD tools on macOS), the case prints a note and passes.

// Case names mirror the flags they test (`id_G` vs `id_g`), where case matters.
#![allow(non_snake_case)]

mod common;

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, UNIX_EPOCH};

use common::{dogfish_bin, exec, gnu_tool};

const MD5_HELLO: &str = "b1946ac92492d2347c6235b4d2611184";
const MD5_WORLD: &str = "591785b794601e212b260e25925636fd";
const MD5_X: &str = "9dd4e461268c8034f5c8564e155c67a6";
const MD5_Y: &str = "415290769594460e2e485922904f345d";
const MD5_EMPTY: &str = "d41d8cd98f00b204e9800998ecf8427e";
const SHA256_HELLO: &str = "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03";
const B2_HELLO: &str = "f60ce482e5cc1229f39d71313171a8d9f4ca3a87d066bf4b205effb528192a75\
                        f14f3271e2c1a90e1de53f275b4d4793eef2f5e31ea90d2ce29d2e481c36435f";
const B2_256_HELLO: &str = "93becc6e9882211c3ec3708c95bcd69baab7bb59c7f4bc84ce637b88a534b783";

const STD_B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL_B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Deterministic pseudo-random bytes (xorshift), larger than the codecs'
/// blocks so that streaming crosses block boundaries.
fn big_data() -> &'static [u8] {
    static DATA: OnceLock<Vec<u8>> = OnceLock::new();
    DATA.get_or_init(|| {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        (0..300_001)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    })
}

/// Base64 of `data` with `alphabet`, wrapped at 76 columns as GNU writes it.
fn base64_wrapped(data: &[u8], alphabet: &[u8; 64]) -> Vec<u8> {
    let mut flat = Vec::new();
    for g in data.chunks(3) {
        let byte = |i: usize| u32::from(g.get(i).copied().unwrap_or(0));
        let n = (byte(0) << 16) | (byte(1) << 8) | byte(2);
        for k in 0..4 {
            flat.push(if k <= g.len() {
                alphabet[((n >> (18 - 6 * k)) & 63) as usize]
            } else {
                b'='
            });
        }
    }
    let mut out = Vec::new();
    for line in flat.chunks(76) {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}

/// `big_data` in base64, as standard input.
fn big_b64() -> &'static [u8] {
    static DATA: OnceLock<Vec<u8>> = OnceLock::new();
    DATA.get_or_init(|| base64_wrapped(big_data(), STD_B64))
}

/// Fixture directory shared by every case; both implementations run with it
/// as their working directory so file names in output are identical.
fn fixtures() -> &'static Path {
    static DIR: OnceLock<std::path::PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = common::fresh_fixture_dir();
        let bin: Vec<u8> = (0..=255u8).collect();
        let mut big_bad = big_b64().to_vec();
        let at = big_bad.len() - 1000;
        big_bad[at] = b'*';
        let mut big_url = base64_wrapped(big_data(), URL_B64);
        big_url[9000] = b'+';
        let lists: Vec<(&str, String)> = vec![
            (
                "good.md5",
                format!("{MD5_HELLO}  a.txt\n{MD5_WORLD}  b.txt\n"),
            ),
            ("bad.md5", format!("{MD5_HELLO}  a.txt\n{MD5_X}  b.txt\n")),
            ("mixed.md5", format!("{MD5_HELLO}  a.txt\ngarbage line\n")),
            (
                "multi.md5",
                format!("{MD5_X}  a.txt\n{MD5_X}  b.txt\nbad\nbad2\n"),
            ),
            (
                "miss.md5",
                format!("{MD5_HELLO}  a.txt\n{MD5_HELLO}  missing.txt\n"),
            ),
            (
                "miss2.md5",
                format!("{MD5_HELLO}  nope1\n{MD5_HELLO}  nope2\n"),
            ),
            ("crlf.md5", format!("{MD5_HELLO}  a.txt\r\n")),
            (
                "upper.md5",
                format!("{}  a.txt\n", MD5_HELLO.to_uppercase()),
            ),
            ("star.md5", format!("{MD5_HELLO} *a.txt\n")),
            (
                "reversed.md5",
                format!("{MD5_HELLO} a.txt\n{MD5_WORLD}\tb.txt\n"),
            ),
            (
                "mixstyle.md5",
                format!("{MD5_HELLO}  a.txt\n{MD5_WORLD} b.txt\n"),
            ),
            ("lead.md5", format!("  \t{MD5_HELLO}  a.txt\n")),
            (
                "comment.md5",
                format!("# a comment\n\n{MD5_HELLO}  a.txt\n"),
            ),
            (
                "tag.md5",
                format!("MD5 (a.txt) = {MD5_HELLO}\nMD5 (b.txt) = {MD5_WORLD}\n"),
            ),
            (
                "mixtag.md5",
                format!("MD5 (a.txt) = {MD5_HELLO}\nSHA256 (a.txt) = {SHA256_HELLO}\n"),
            ),
            ("tight.md5", format!("MD5(a.txt)= {MD5_HELLO}\n")),
            ("paren.md5", format!("MD5 (paren (1).txt) = {MD5_X}\n")),
            ("nonl.md5", format!("{MD5_HELLO}  a.txt")),
            (
                "escaped.md5",
                format!("\\{MD5_X}  back\\\\slash\n\\{MD5_Y}  new\\nline\n"),
            ),
            ("badescape.md5", format!("\\{MD5_HELLO}  a\\qb\n")),
            ("dash.md5", format!("{MD5_EMPTY}  -\n")),
            ("dir.md5", format!("{MD5_HELLO}  d\n")),
            ("short.md5", format!("{}  a.txt\n", &MD5_HELLO[..30])),
            ("a.sha256", format!("{SHA256_HELLO}  a.txt\n")),
            ("tag.sha256", format!("SHA256 (a.txt) = {SHA256_HELLO}\n")),
            ("a.b2", format!("{B2_HELLO}  a.txt\n")),
            ("a256.b2", format!("{B2_256_HELLO}  a.txt\n")),
            ("tag.b2", format!("BLAKE2b (a.txt) = {B2_HELLO}\n")),
            (
                "tag256.b2",
                format!("BLAKE2b-256 (a.txt) = {B2_256_HELLO}\n"),
            ),
            (
                "badlen.b2",
                format!("BLAKE2b-7 (a.txt) = {}\n", &B2_256_HELLO[..2]),
            ),
            ("odd.b2", format!("{}  a.txt\n", &B2_256_HELLO[..63])),
            ("lines.txt", "b\na\nc\n".to_string()),
            (
                "dates.txt",
                "@0\n2020-02-29 12:00\nbogus\n\n1 Jan 2000\n".to_string(),
            ),
        ];
        let mut files: Vec<(&str, &[u8])> = vec![
            ("a.txt", b"hello\n"),
            ("b.txt", b"world\n"),
            ("empty.txt", b""),
            ("bin.dat", &bin),
            ("with space.txt", b"x"),
            ("back\\slash", b"x"),
            ("new\nline", b"y"),
            ("cr\rname", b"z"),
            ("paren (1).txt", b"x"),
            ("d/inner.txt", b"inner\n"),
            ("hello.b64", b"aGVsbG8gd29ybGQK\n"),
            ("wrapped.b64", b"aGVs\nbG8g\nd29y\nbGQK\n"),
            ("big.dat", big_data()),
            ("big.b64", big_b64()),
            ("bigbad.b64", &big_bad),
            ("bigurl.b64", &big_url),
        ];
        files.extend(lists.iter().map(|(n, c)| (*n, c.as_bytes())));
        common::write_files(&dir, &files);
        let stamp = UNIX_EPOCH + Duration::new(1_234_567_890, 123_456_789);
        std::fs::File::options()
            .write(true)
            .open(dir.join("a.txt"))
            .and_then(|f| f.set_modified(stamp))
            .unwrap();
        dir
    })
}

gnu_cases! {
    // ------------------------------------------------------- md5sum & co.
    md5_two_files: "md5sum" ["a.txt", "b.txt"];
    md5_stdin_dash_twice: "md5sum" ["-", "a.txt", "-"] stdin b"abc";
    md5_binary_marker: "md5sum" ["-b", "a.txt"];
    md5_text_marker: "md5sum" ["-t", "a.txt"];
    md5_binary_then_text: "md5sum" ["-b", "-t", "a.txt"];
    md5_tag: "md5sum" ["--tag", "a.txt", "-"] stdin b"";
    md5_tag_after_text: "md5sum" ["-t", "--tag", "a.txt"];
    md5_tag_then_text: "md5sum" ["--tag", "-t", "a.txt"];
    md5_tag_binary: "md5sum" ["--tag", "-b", "a.txt"];
    md5_zero: "md5sum" ["-z", "a.txt", "b.txt"];
    md5_zero_tag: "md5sum" ["-z", "--tag", "a.txt"];
    md5_escaped_names: "md5sum" ["back\\slash", "new\nline", "cr\rname"];
    md5_escaped_tag: "md5sum" ["--tag", "back\\slash", "new\nline"];
    md5_zero_no_escape: "md5sum" ["-z", "back\\slash", "new\nline"];
    md5_space_name: "md5sum" ["with space.txt"];
    md5_directory: "md5sum" ["d", "a.txt"];
    md5_missing_quoted: "md5sum" ["no such", "a.txt", "new\nmissing"];
    md5_binary_file: "md5sum" ["bin.dat", "empty.txt"];
    md5_check_ok: "md5sum" ["-c", "good.md5"];
    md5_check_long: "md5sum" ["--check", "good.md5"];
    md5_check_failed: "md5sum" ["-c", "bad.md5"];
    md5_check_quiet: "md5sum" ["-c", "--quiet", "bad.md5"];
    md5_check_quiet_all_ok: "md5sum" ["-c", "--quiet", "good.md5"];
    md5_check_status: "md5sum" ["-c", "--status", "bad.md5"];
    md5_check_status_then_quiet: "md5sum" ["-c", "--status", "--quiet", "bad.md5"];
    md5_check_quiet_then_status: "md5sum" ["-c", "--quiet", "--status", "bad.md5"];
    md5_check_misformatted: "md5sum" ["-c", "mixed.md5"];
    md5_check_strict: "md5sum" ["-c", "--strict", "mixed.md5"];
    md5_check_warn: "md5sum" ["-c", "-w", "mixed.md5"];
    md5_check_warn_strict_plural: "md5sum" ["-c", "--warn", "--strict", "multi.md5"];
    md5_check_warn_then_quiet: "md5sum" ["-c", "--warn", "--quiet", "multi.md5"];
    md5_check_missing: "md5sum" ["-c", "miss.md5"];
    md5_check_missing_plural: "md5sum" ["-c", "miss2.md5"];
    md5_check_ignore_missing: "md5sum" ["-c", "--ignore-missing", "miss.md5"];
    md5_check_ignore_missing_none: "md5sum" ["-c", "--ignore-missing", "miss2.md5"];
    md5_check_crlf: "md5sum" ["-c", "crlf.md5"];
    md5_check_uppercase: "md5sum" ["-c", "upper.md5"];
    md5_check_star: "md5sum" ["-c", "star.md5"];
    md5_check_reversed_bsd: "md5sum" ["-c", "reversed.md5"];
    md5_check_mixed_styles: "md5sum" ["-c", "mixstyle.md5"];
    md5_check_leading_blanks: "md5sum" ["-c", "lead.md5"];
    md5_check_comments: "md5sum" ["-c", "comment.md5"];
    md5_check_tag_format: "md5sum" ["-c", "tag.md5"];
    md5_check_other_algo_tag: "md5sum" ["-c", "mixtag.md5"];
    md5_check_tight_tag: "md5sum" ["-c", "tight.md5"];
    md5_check_paren_in_name: "md5sum" ["-c", "paren.md5"];
    md5_check_no_newline: "md5sum" ["-c", "nonl.md5"];
    md5_check_escaped: "md5sum" ["-c", "escaped.md5"];
    md5_check_bad_escape: "md5sum" ["-c", "badescape.md5"];
    md5_check_short_digest: "md5sum" ["-c", "short.md5"];
    md5_check_stdin_entry: "md5sum" ["-c", "dash.md5"] stdin b"";
    md5_check_from_stdin: "md5sum" ["-c"] stdin b"b1946ac92492d2347c6235b4d2611184  a.txt\n";
    md5_check_stdin_garbage: "md5sum" ["-c", "-"] stdin b"nothing useful\n";
    md5_check_dev_null: "md5sum" ["-c", "/dev/null"];
    md5_check_wrong_algo: "md5sum" ["-c", "a.sha256"];
    md5_check_directory_entry: "md5sum" ["-c", "dir.md5"];
    md5_check_directory_list: "md5sum" ["-c", "d"];
    md5_check_missing_list: "md5sum" ["-c", "good.md5", "nonexist", "bad.md5"];
    md5_quiet_without_check: "md5sum" ["--quiet", "a.txt"];
    md5_status_without_check: "md5sum" ["--quiet", "--status", "a.txt"];
    md5_warn_without_check: "md5sum" ["--warn", "a.txt"];
    md5_strict_without_check: "md5sum" ["--strict", "a.txt"];
    md5_ignore_missing_without_check: "md5sum" ["--ignore-missing", "a.txt"];
    md5_check_with_tag: "md5sum" ["-c", "--tag", "good.md5"];
    md5_check_with_zero: "md5sum" ["-c", "-z", "--tag", "good.md5"];
    md5_check_with_binary: "md5sum" ["-c", "-b", "good.md5"];
    md5_long_abbrev: "md5sum" ["--chec", "--qui", "good.md5"];
    sha256_files: "sha256sum" ["a.txt", "b.txt", "empty.txt"];
    sha256_tag: "sha256sum" ["--tag", "a.txt"];
    sha256_check: "sha256sum" ["-c", "a.sha256"];
    sha256_check_tag: "sha256sum" ["-c", "tag.sha256"];
    sha256_check_md5_list: "sha256sum" ["-c", "a.sha256", "good.md5"];
    sha256_directory: "sha256sum" ["d"];
    b2_files: "b2sum" ["a.txt", "b.txt"];
    b2_length_256: "b2sum" ["-l", "256", "a.txt"];
    b2_length_8_tag: "b2sum" ["--length=8", "--tag", "a.txt"];
    b2_length_512_tag: "b2sum" ["-l", "512", "--tag", "a.txt"];
    b2_length_zero: "b2sum" ["-l", "0", "a.txt"];
    b2_length_not_multiple: "b2sum" ["-l", "7", "a.txt"];
    b2_length_too_big: "b2sum" ["-l", "1024", "a.txt"];
    b2_length_invalid: "b2sum" ["-l", "x", "a.txt"];
    b2_tag: "b2sum" ["--tag", "a.txt"];
    b2_check_full: "b2sum" ["-c", "a.b2"];
    b2_check_auto_length: "b2sum" ["-c", "a256.b2", "tag.b2", "tag256.b2"];
    b2_check_with_length: "b2sum" ["-l", "256", "-c", "a.b2"];
    b2_check_bad_tag_length: "b2sum" ["-c", "badlen.b2"];
    b2_check_odd_digest: "b2sum" ["-c", "odd.b2"];
    md5_has_no_length_option: "md5sum" ["-l", "8", "a.txt"];

    // -------------------------------------------------------------- base64
    base64_files_wrap_default: "base64" ["bin.dat"];
    base64_wrap_4: "base64" ["-w", "4"] stdin b"hello world";
    base64_wrap_attached: "base64" ["--wrap=3"] stdin b"hello";
    base64_wrap_zero: "base64" ["-w0"] stdin b"hello";
    base64_wrap_invalid: "base64" ["-w", "x"] stdin b"hello";
    base64_wrap_negative: "base64" ["-w", "-1"] stdin b"hello";
    base64_decode_file: "base64" ["-d", "hello.b64"];
    base64_decode_wrapped: "base64" ["--decode", "wrapped.b64"];
    base64_decode_no_padding: "base64" ["-d"] stdin b"aGVsbG8";
    base64_decode_trailing_junk: "base64" ["-d"] stdin b"aGVsbG8=x";
    base64_decode_bad_char: "base64" ["-d"] stdin b"aGV*sbG8=";
    base64_decode_ignore_garbage: "base64" ["-di"] stdin b"aG V*sbG8=\n!";
    base64_decode_ignore_garbage_unpadded: "base64" ["-d", "-i"] stdin b"aGVsbG8";
    base64_decode_concatenated: "base64" ["-d"] stdin b"YQ==YQ==";
    base64_decode_bad_padding: "base64" ["-d"] stdin b"aG=Vs";
    base64_decode_single_char: "base64" ["-d"] stdin b"a";
    base64_decode_space: "base64" ["-d"] stdin b"YW Jj";
    base64_decode_tab: "base64" ["-d"] stdin b"YWJj\tZGVm";
    base64_decode_blank_lines: "base64" ["-d"] stdin b"aGVsbG8=\n\n";
    base64_decode_empty: "base64" ["-d"] stdin b"";
    base64_extra_operand: "base64" ["a.txt", "b.txt"];
    base64_directory: "base64" ["d"];
    base64_missing: "base64" ["nonexist"];
    basenc_missing_encoding: "basenc" [] stdin b"hi";
    basenc_base64: "basenc" ["--base64"] stdin b"hello?>";
    basenc_base64url: "basenc" ["--base64url"] stdin b"hello?>";
    basenc_last_encoding_wins: "basenc" ["--base64", "--base32"] stdin b"hello?>";
    basenc_base32: "basenc" ["--base32"] stdin b"hello";
    basenc_base32hex: "basenc" ["--base32hex"] stdin b"hello";
    basenc_base16: "basenc" ["--base16"] stdin b"hello";
    basenc_base16_wrap: "basenc" ["--base16", "-w", "10"] stdin b"hello world how are you";
    basenc_base2msbf: "basenc" ["--base2msbf"] stdin b"hello";
    basenc_base2lsbf: "basenc" ["--base2lsbf", "-w", "12"] stdin b"hello";
    basenc_z85: "basenc" ["--z85"] stdin b"hellohel";
    basenc_z85_bad_length: "basenc" ["--z85"] stdin b"hello";
    basenc_z85_empty: "basenc" ["--z85"] stdin b"";
    basenc_binary_file: "basenc" ["--base32", "bin.dat"];
    basenc_base64url_decode: "basenc" ["--base64url", "-d"] stdin b"aGVsbG8_Pg==";
    basenc_base64url_rejects_std: "basenc" ["--base64url", "-d"] stdin b"aGVsbG8/Pg==";
    basenc_base32_decode: "basenc" ["--base32", "-d"] stdin b"NBSWY3DP\n";
    basenc_base32_decode_lower: "basenc" ["--base32", "-d"] stdin b"nbswy3dp";
    basenc_base32_decode_short: "basenc" ["--base32", "-d"] stdin b"NBSWY3D";
    basenc_base32_decode_padded: "basenc" ["--base32", "-d"] stdin b"NBSWY3DPH47A====";
    basenc_base32_ignore_garbage: "basenc" ["--base32", "-di"] stdin b"NB*SWY3DP";
    basenc_base32hex_decode: "basenc" ["--base32hex", "-d"] stdin b"D1IMOR3F";
    basenc_base16_decode: "basenc" ["--base16", "-d"] stdin b"68656C6C6F";
    basenc_base16_decode_lower: "basenc" ["--base16", "-d"] stdin b"68656c6c6f";
    basenc_base16_decode_odd: "basenc" ["--base16", "-d"] stdin b"68656C6C6";
    basenc_base16_ignore_garbage: "basenc" ["--base16", "-d", "-i"] stdin b"6 8";
    basenc_base2msbf_decode: "basenc" ["--base2msbf", "-d"] stdin b"0110100001101001";
    basenc_base2lsbf_decode: "basenc" ["--base2lsbf", "-d"] stdin b"0001011010010110";
    basenc_base2_decode_partial: "basenc" ["--base2msbf", "-d"] stdin b"011010000110100";
    basenc_z85_decode: "basenc" ["--z85", "-d"] stdin b"xK#0@zY<mx";
    basenc_z85_decode_partial: "basenc" ["--z85", "-d"] stdin b"xK#0";
    basenc_z85_decode_invalid_char: "basenc" ["--z85", "-d"] stdin b"xK#0@~~~~~";
    // Streams larger than the codecs' blocks.
    base64_big_encode: "base64" ["big.dat"];
    base64_big_encode_wrap_7: "base64" ["-w", "7", "big.dat"];
    base64_big_encode_nowrap: "base64" ["-w0", "big.dat"];
    base64_big_encode_stdin: "base64" [] stdin big_data();
    base64_big_decode: "base64" ["-d", "big.b64"];
    base64_big_decode_stdin: "base64" ["-d"] stdin big_b64();
    base64_big_decode_bad_tail: "base64" ["-d", "bigbad.b64"];
    base64_big_decode_ignore_garbage: "base64" ["-di", "bigbad.b64"];
    basenc_big_base32_wrap_9: "basenc" ["--base32", "-w", "9", "big.dat"];
    basenc_big_base2lsbf: "basenc" ["--base2lsbf", "big.dat"];
    // GNU writes the 30720-byte blocks before the ragged one, then fails.
    basenc_z85_big_ragged: "basenc" ["--z85", "big.dat"];
    // GNU rejects the whole 5600-byte step holding the '+'.
    basenc_base64url_big_std_char: "basenc" ["--base64url", "-d", "bigurl.b64"];
    // GNU translates base32hex to standard base32, passing the rest through.
    basenc_base32hex_std_letters: "basenc" ["--base32hex", "-d"] stdin b"ZZZZZZZZ";
    // --ignore-garbage keeps '=' for every encoding.
    basenc_base16_ignore_garbage_padding: "basenc" ["--base16", "-di"] stdin b"6 8=6";
    basenc_base2_ignore_garbage_padding: "basenc" ["--base2msbf", "-di"] stdin b"01101000=0";

    // ---------------------------------------------------------- echo/printf
    echo_help_is_text_with_args: "echo" ["--help", "x"];
    echo_dash_dash_is_text: "echo" ["--", "-n"];
    echo_combined_flags: "echo" ["-neE", "a\\tb"];
    echo_e_octal_and_hex: "echo" ["-e", "\\0101\\x42\\x4"];
    echo_e_bad_hex: "echo" ["-e", "x\\xg"];
    echo_e_escapes: "echo" ["-e", "\\a\\b\\e\\f\\v\\r|"];
    printf_excess_args_warning: "printf" ["a\\n", "x", "y"];
    printf_version_as_format: "printf" ["--version", "x"];
    printf_missing_operand: "printf" [];
    printf_b_octal: "printf" ["%b\\n", "\\0101\\101"];
    printf_width_string: "printf" ["[%5s][%-5s][%.1s]\\n", "ab", "cd", "ef"];
    printf_float_flags: "printf" ["%+.2e|% f|%#.0f\\n", "12345", "1", "2"];
    printf_int_range: "printf" ["%d %d %u\\n", "9223372036854775808", "-9223372036854775809", "18446744073709551616"];
    printf_unsigned_wrap: "printf" ["%u %x\\n", "18446744073709551615", "-2"];
    printf_shell_quote: "printf" ["%q|%q|%q|%q\\n", "x", "a b", "a'b", "t\tab"];
    printf_hex_float_arg: "printf" ["%f %g\\n", "0x10", "0x1p-2"];
    printf_lone_quote: "printf" ["%d\\n", "'"];
    printf_float_partial: "printf" ["%f\\n", "1.5x"];
    printf_dash_dash: "printf" ["--", "%s\\n", "a"];
    printf_dash_dash_only: "printf" ["--"];
    // `long double`: the x86-64 80-bit format, glibc's printf_fp/printf_fphex.
    printf_hex_float: "printf" ["%a|%A|%La|%a\n", "1", "-0.1", "0", "-0"];
    printf_hex_float_precision: "printf" ["%.0a|%.1a|%#.0a|%.3A|%012.2a|%.0a\n", "1.5", "1.96875", "1", "1.999", "-1", "15.5"];
    printf_hex_float_specials: "printf" ["%a %A %5a %-6A|\n", "inf", "-nan", "nan", "-inf"];
    printf_hex_float_extremes: "printf" ["%a|%A|%a\n", "1e4932", "0x1p-16445", "0x1.fp-16385"];
    printf_ld_max: "printf" ["%Lg|%e|%.0f\n", "1.18973149535723176502e+4932", "1e4932", "1e4932"];
    printf_ld_overflow: "printf" ["%g|%f\n", "1e5000", "-1e4933x"];
    printf_ld_underflow: "printf" ["%g|%e|%a\n", "1e-5000", "1e-4950", "0x1p-16446"];
    printf_ld_smallest_subnormal: "printf" ["%e|%.30e|%g\n", "3.6451995318824746025e-4951", "0x1p-16445", "0x3p-16445"];
    printf_ld_precision_digits: "printf" ["%.30f|%.25e|%.21g\n", "0.1", "1e-20", "1.0000000000000000001"];
    printf_ld_rounding_ties: "printf" ["%.0f %.0f %.0f %.0f %.1f %.2e %.0e\n", "0.5", "1.5", "2.5", "-3.5", "0.25", "1.125", "0.95"];
    printf_ld_big_integers: "printf" ["%.0f %g %.20g %f\n", "18446744073709551617", "1e20", "18446744073709551615", "0x1p70"];
    printf_ld_g_edges: "printf" ["%g %g %#g %#g %#.0g %.0g %G\n", "0.0001", "0.0000999999999", "0.0000999999999", "999999.5", "1", "15", "1e-10"];
    printf_ld_upper_specials: "printf" ["%F %F %E %G %010f|%-6e|\n", "inf", "-nan", "inf", "nan", "-inf", "nan"];
    printf_ld_widths: "printf" ["%-20.5a|%+30.20Le|%040.30f|% .3g\n", "3", "-7", "2.5", "1"];
    printf_ld_many_digits: "printf" ["%.4950e\n", "0x1p-16445"];
    printf_empty_numeric_argument: "printf" ["%f|%d|%a|%u\n", "", "", "", ""];

    // -------------------------------------------------------------- env
    env_empty: "env" ["-i"];
    env_assignments_in_order: "env" ["-i", "B=2", "A=1"];
    env_assignment_replaces: "env" ["-i", "A=1", "B=2", "A=3"];
    env_empty_name: "env" ["-i", "=x", "A=1"];
    env_dash_means_i: "env" ["-", "A=1"];
    env_i_dash: "env" ["-i", "-", "A=1"];
    env_null: "env" ["-i", "-0", "A=1", "B=2"];
    env_unset_then_print: "env" ["-u", "HOME", "-u", "NO_SUCH_VAR"];
    env_unset_invalid: "env" ["-u", "A=B"];
    env_unset_missing_arg: "env" ["-u"];
    env_stops_at_operand: "env" ["-i", "A=1", "-u", "A"];
    env_double_dash: "env" ["-i", "--", "A=1"];
    env_double_dash_after_operand: "env" ["-i", "A=1", "--", "true"];
    env_run_command: "env" ["-i", "A=1", "B=2", "printenv"];
    env_run_command_unset: "env" ["-i", "A=1", "env", "-u", "A", "env"];
    env_exit_status: "env" ["-i", "sh", "-c", "exit 3"];
    env_not_found: "env" ["-i", "nonexist-cmd"];
    env_not_executable: "env" ["-i", "/etc"];
    env_empty_command: "env" ["-S", "'' x"];
    env_null_with_command: "env" ["-0", "-i", "A=1", "true"];
    env_chdir: "env" ["-C", "/", "pwd"];
    env_chdir_missing: "env" ["-C", "/nonexist", "true"];
    env_chdir_without_command: "env" ["-C", "/tmp"];
    env_chdir_missing_arg: "env" ["--chdir"];
    env_split: "env" ["-i", "-S", "A=1 B=\"x y\" env"];
    env_split_attached: "env" ["-iSprintf  [%s]\\n a b"];
    env_split_escapes: "env" ["-S", "printf [%s]\\n a\\tb \"c\\_d\" e\\_f"];
    env_split_stop: "env" ["-S", "printf [%s]\\n a\\c b"];
    env_split_comment: "env" ["-S", "printf [%s]\\n a #b c"];
    env_split_hash_inside: "env" ["-S", "printf [%s]\\n a#b"];
    env_split_single_quotes: "env" ["-S", "printf [%s]\\n 'a b' 'c\\'d' '\\x'"];
    env_split_double_quotes: "env" ["-S", "printf [%s]\\n \"a\\\"b\""];
    env_split_var: "env" ["-i", "-S", "printf [%s]\\n ${HOME}x ${NOPE}y ${NOPE}"];
    env_split_bad_var: "env" ["-S", "echo $HOME"];
    env_split_bad_var_name: "env" ["-S", "echo ${A-B}"];
    env_split_bad_escape: "env" ["-S", "echo \\q"];
    env_split_backslash_end: "env" ["-S", "a\\"];
    env_split_c_in_quotes: "env" ["-S", "\"a\\cb\""];
    env_split_unterminated: "env" ["-S", "echo \"oops"];
    env_split_missing_arg: "env" ["-S"];
    env_split_options: "env" ["-S", "-i A=2 printenv A"];
    env_debug: "env" ["-v", "-i", "A=1", "printf", "%s", "x"];
    env_debug_chdir: "env" ["-i", "-C", "/", "-v", "pwd"];
    env_bad_signal: "env" ["--block-signal=FOO", "true"];
    env_list_signals: "env" ["--block-signal=INT", "--ignore-signal=int,PIPE", "--list-signal-handling", "true"];
    env_list_signal_numbers: "env" ["--default-signal", "--ignore-signal=SIGUSR1,15", "--list-signal-handling", "true"];
    env_ignore_all_signals: "env" ["--ignore-signal", "--list-signal-handling", "true"];
    env_ignore_stop_fails: "env" ["--ignore-signal=STOP", "true"];
    env_block_kill_is_fine: "env" ["--block-signal=KILL", "true"];
    env_long_abbrev: "env" ["--ignore-env", "--nu", "A=1"];
    env_ambiguous: "env" ["--i", "true"];
    env_option_with_value: "env" ["--null=x"];
    env_shebang_space_hint: "env" ["-v -S cat -n"];
    env_shebang_tab_hint: "env" ["-i\tA=B"];
    env_shebang_vtab_hint: "env" ["-i\x0bA=B"];
    env_space_in_command_hint: "env" ["-i", "no-such cmd", "x"];

    // ------------------------------------------------------- printenv / id
    printenv_all: "printenv" [];
    printenv_some: "printenv" ["LANG", "NO_SUCH_VAR", "LC_ALL"];
    printenv_missing: "printenv" ["NO_SUCH_VAR"];
    printenv_null: "printenv" ["-0", "LANG", "LC_ALL"];
    printenv_assignment_is_missing: "printenv" ["LANG=C"];
    printenv_bad_option_status: "printenv" ["-x"];
    id_default: "id" [];
    id_user_arg: "id" ["root"];
    id_numeric_user: "id" ["0"];
    id_two_users: "id" ["root", "nobody"];
    id_no_such_user: "id" ["no-such-user-xyz"];
    id_u: "id" ["-u"];
    id_un: "id" ["-un"];
    id_ur: "id" ["-ur"];
    id_uz: "id" ["-uz"];
    id_g: "id" ["-g"];
    id_gn: "id" ["-gn"];
    id_G: "id" ["-G"];
    id_Gn: "id" ["-Gn"];
    id_Gz: "id" ["-Gz"];
    id_G_user: "id" ["-G", "daemon"];
    id_un_users: "id" ["-un", "root", "daemon"];
    id_Gz_users: "id" ["-Gz", "root", "daemon"];
    id_Gnz_users: "id" ["-Gnz", "root", "daemon", "no-such-user-xyz"];
    id_Gz_one_user: "id" ["-Gz", "root"];
    id_uz_users: "id" ["-uz", "root", "daemon"];
    id_a_ignored: "id" ["-a"];
    id_two_choices: "id" ["-u", "-g"];
    id_name_default_format: "id" ["-n"];
    id_real_default_format: "id" ["-r"];
    id_zero_default_format: "id" ["-z"];
    id_context: "id" ["-Z"];
    id_context_with_user: "id" ["-Z", "root"];
    whoami_plain: "whoami" [];
    whoami_extra: "whoami" ["x"];

    // ------------------------------------------------ uname / nproc / misc
    uname_default: "uname" [];
    uname_all: "uname" ["-a"];
    uname_each: "uname" ["-snrvmpio"];
    uname_order_is_fixed: "uname" ["-m", "-s"];
    uname_processor_platform: "uname" ["-p", "-i"];
    uname_os: "uname" ["--operating-system"];
    uname_long_abbrev: "uname" ["--kernel-n"];
    uname_extra_operand: "uname" ["x"];
    nproc_default: "nproc" [];
    nproc_all: "nproc" ["--all"];
    nproc_ignore: "nproc" ["--ignore=1"];
    nproc_ignore_all: "nproc" ["--ignore", "999"];
    nproc_ignore_invalid: "nproc" ["--ignore=x"];
    nproc_ignore_negative: "nproc" ["--ignore=-1"];
    nproc_ignore_overflow: "nproc" ["--ignore=99999999999999999999999"];
    nproc_omp: "nproc" [] env [("OMP_NUM_THREADS", "3")];
    nproc_omp_limit: "nproc" [] env [("OMP_NUM_THREADS", "3,4"), ("OMP_THREAD_LIMIT", "2")];
    nproc_omp_invalid: "nproc" [] env [("OMP_NUM_THREADS", "x")];
    nproc_extra: "nproc" ["x"];
    true_ignores_args: "true" ["--bogus", "-x"];
    false_ignores_args: "false" ["--bogus", "-x"];
    true_help_with_args: "true" ["--help", "x"];

    // ------------------------------------------------------------ sleep
    sleep_sum: "sleep" ["0.05", "0.01s", "0.0001m"];
    sleep_zero_hours: "sleep" ["0h", "0d", "0x0.0p1"];
    sleep_leading_space: "sleep" [" 0"];
    sleep_trailing_space: "sleep" ["0 "];
    sleep_invalid: "sleep" ["x"];
    sleep_invalid_suffix: "sleep" ["1x"];
    sleep_several_invalid: "sleep" ["x", "0", "1q"];
    sleep_negative_is_option: "sleep" ["-1"];
    sleep_empty: "sleep" [""];
    sleep_missing: "sleep" [];
    sleep_dot: "sleep" ["."];
    sleep_word_suffix: "sleep" ["0d", "0day"];
    sleep_uppercase_suffix: "sleep" ["0D"];
    sleep_plural_suffix: "sleep" ["0secs"];

    // ---------------------------------------------------------- timeout
    timeout_no_args: "timeout" [];
    timeout_only_duration: "timeout" ["1"];
    timeout_passes_status: "timeout" ["5", "sh", "-c", "exit 7"];
    timeout_expires: "timeout" ["0.1", "sleep", "5"];
    timeout_preserve_status: "timeout" ["--preserve-status", "0.1", "sleep", "5"];
    timeout_preserve_abbrev: "timeout" ["--pre", "0.1", "sleep", "5"];
    timeout_foreground: "timeout" ["--foreground", "0.1", "sleep", "5"];
    timeout_verbose: "timeout" ["-v", "0.1", "sleep", "5"];
    timeout_signal_int: "timeout" ["-v", "-s", "INT", "0.1", "sleep", "5"];
    timeout_signal_attached: "timeout" ["-sHUP", "0.1", "sleep", "5"];
    timeout_signal_number: "timeout" ["--signal=9", "0.1", "sleep", "5"];
    timeout_signal_lowercase: "timeout" ["-s", "sigusr1", "0.1", "sleep", "5"];
    timeout_kill_after: "timeout" ["-v", "-k", "0.1", "-s", "USR1", "0.1", "sh", "-c", "trap '' USR1; sleep 5"];
    timeout_child_signal: "timeout" ["5", "sh", "-c", "kill -USR1 $$"];
    timeout_hex_duration: "timeout" ["0x1p-3", "sleep", "5"];
    timeout_zero_disables: "timeout" ["0", "true"];
    timeout_days: "timeout" ["1d", "true"];
    timeout_inf: "timeout" ["inf", "true"];
    timeout_invalid_duration: "timeout" ["x", "true"];
    timeout_invalid_suffix: "timeout" ["1x", "true"];
    timeout_trailing_space: "timeout" ["1 ", "true"];
    timeout_invalid_kill_after: "timeout" ["-k", "x", "1", "true"];
    timeout_invalid_signal: "timeout" ["-s", "FOO", "1", "true"];
    timeout_signal_out_of_range: "timeout" ["-s", "65", "1", "true"];
    timeout_signal_missing_arg: "timeout" ["-s"];
    timeout_long_missing_arg: "timeout" ["--signal"];
    timeout_flag_with_value: "timeout" ["--verbose=3", "1", "true"];
    timeout_negative_is_option: "timeout" ["-1", "true"];
    timeout_not_found: "timeout" ["1", "nonexist-cmd"];
    timeout_not_executable: "timeout" ["1", "/etc"];
    timeout_dash_dash: "timeout" ["--", "1", "true"];
    timeout_stops_at_command: "timeout" ["1", "sh", "-c", "echo $0 $1", "-v", "x"];
    timeout_verbose_signal_zero: "timeout" ["-v", "-s0", "-k", "0.1", "0.1", "sleep", "5"];
    timeout_signal_zero: "timeout" ["-s", "0", "-k", "0.1", "0.1", "sleep", "5"];

    // ---------------------------------------------------------------- seq
    seq_empty_range_no_newline: "seq" ["1", "0"];
    // The fast integer path: runs of last digits, carries, block boundaries.
    seq_fast_carries: "seq" ["7", "3", "131"];
    seq_fast_end_mid_run: "seq" ["95", "1004"];
    seq_fast_end_same_digit: "seq" ["5", "5"];
    seq_fast_big_step: "seq" ["3", "200", "100000"];
    seq_fast_many: "seq" ["200000"];
    seq_fast_long_sep: "seq" ["-s", "<a separator longer than a line>", "95", "105"];
    seq_fast_huge_numbers: "seq" ["99999999999999999999999999999999990", "7", "100000000000000000000000000000000077"];
    seq_invalid_last: "seq" ["x"];
    seq_invalid_first: "seq" ["1", "x"];
    seq_invalid_quoting: "seq" ["a'b\\c"];
    seq_zero_increment: "seq" ["1", "0", "5"];
    seq_zero_increment_float: "seq" ["1", "0.0", "5"];
    seq_nan: "seq" ["1", "nan"];
    seq_hex: "seq" ["0x10", "0x12"];
    seq_hex_float: "seq" ["0x1p1", "3"];
    seq_exponent: "seq" ["1e2", "1e2"];
    seq_exponent_fraction: "seq" ["1.0e0", "3"];
    seq_exponent_negative: "seq" ["1e-3", "1e-3", "3e-3"];
    seq_format_g: "seq" ["-f", "%03g", "1", "3"];
    seq_format_f: "seq" ["-f", "%.2f", "0", "0.5", "1"];
    seq_format_e: "seq" ["--format=n=%e", "1", "2"];
    seq_format_flags: "seq" ["-f", "%-5g|%+g|%#g", "1", "2"];
    seq_format_long_double: "seq" ["-f", "%Lg", "1", "2"];
    seq_format_percent: "seq" ["-f", "a%%b%g", "1", "2"];
    seq_format_unknown: "seq" ["-f", "%x", "1", "2"];
    seq_format_no_directive: "seq" ["-f", "nopct", "1"];
    seq_format_two_directives: "seq" ["-f", "%g%g", "1"];
    seq_format_ends_in_percent: "seq" ["-f", "%5", "1"];
    seq_format_with_width_flag: "seq" ["-f", "%g", "-w", "1", "2"];
    seq_sep_attached: "seq" ["-s,", "1", "3"];
    seq_sep_empty: "seq" ["-s", "", "1", "3"];
    seq_sep_long: "seq" ["--separator=::", "1", "3"];
    seq_width_negative: "seq" ["-w", "-1", "1"];
    seq_width_fraction: "seq" ["-w", "0.5", "0.5", "2"];
    seq_width_step_fraction: "seq" ["-w", "1", "0.5", "2"];
    seq_width_negative_fraction: "seq" ["-w", "-0.5", "1"];
    seq_width_exponent: "seq" ["-w", "1e1", "12"];
    seq_width_sep: "seq" ["-s", ":", "-w", "98", "101"];
    seq_width_plus: "seq" ["-w", "+1", "10"];
    seq_negative_operands: "seq" ["-3", "-1"];
    seq_negative_last_only: "seq" ["-1"];
    seq_dash_dash_negative: "seq" ["--", "-2", "0"];
    seq_option_after_operand: "seq" ["1", "-w", "3"];
    seq_negative_step: "seq" ["1", "-1", "-2"];
    seq_float_steps: "seq" ["0.1", "0.1", "0.5"];
    seq_float_endpoint: "seq" ["0", "0.000001", "0.000003"];
    seq_float_precision_from_step: "seq" ["1", "1.0", "3"];
    seq_float_down: "seq" ["10", "-2.5", "0"];
    seq_last_precision_ignored: "seq" ["1.10", "1.15", "2"];
    seq_integral_float: "seq" ["3.0"];
    seq_leading_space: "seq" [" 1", "2"];
    seq_trailing_space: "seq" ["1 ", "2"];
    seq_plus_sign: "seq" ["+1", "+2", "5"];
    seq_negative_zero: "seq" ["-0", "2"];
    seq_big_integers: "seq" ["18446744073709551615", "18446744073709551617"];
    seq_big_step: "seq" ["99999999999999999998", "2", "100000000000000000004"];
    seq_i64_edge: "seq" ["9223372036854775806", "9223372036854775807"];
    seq_scientific_integers: "seq" ["1e20", "100000000000000000002"];
    seq_scientific_single: "seq" ["1e20", "1e20"];
    seq_dot: "seq" ["1", ".", "3"];
    seq_extra_operand: "seq" ["1", "2", "3", "4"];
    seq_missing_operand: "seq" [];
    seq_inf_last: "seq" ["-f", "%g", "inf", "1"];
    // `long double` arithmetic, as GNU's on x86-64.
    seq_ld_roadmap_case: "seq" ["-f", "%g", "1e20", "1e20"];
    seq_ld_integer_spelling: "seq" ["1e30", "1e30"];
    seq_ld_float_step_beyond_2_64: "seq" ["1", "1e19", "5e19"];
    seq_ld_integers_beyond_2_64: "seq" ["-s", ",", "18446744073709551615", "18446744073709551620"];
    seq_ld_integer_step_beyond_2_64: "seq" ["18446744073709551616", "18446744073709551616", "1e20"];
    seq_ld_negative_beyond_2_64: "seq" ["-18446744073709551617", "-18446744073709551615"];
    seq_ld_fraction_digits: "seq" ["-f", "%.25f", "0", "0.1", "0.5"];
    seq_ld_fraction_steps_long: "seq" ["0.000000001", "0.000000001", "0.00000002"];
    seq_ld_max: "seq" ["-f", "%Lg", "1e4932", "-1e4932", "-1e4932"];
    seq_ld_max_fast: "seq" ["1e4932", "1e4932"];
    seq_ld_overflow: "seq" ["1e4933"];
    seq_ld_subnormals: "seq" ["-f", "%Le", "0x1p-16445", "0x1p-16445", "0x4p-16445"];
    seq_ld_subnormal_inexact: "seq" ["1e-4950", "1"];
    seq_ld_underflow_to_zero: "seq" ["1e-5000", "1"];
    seq_ld_width_tiny: "seq" ["-w", "1e-30", "1e-30", "3e-30"];
    seq_ld_width_mixed: "seq" ["-w", "-1.5", "0.25", "1e1"];
    seq_ld_width_exponent_fraction: "seq" ["-w", "1.5e1", "2.25e1"];
    seq_ld_hex_format: "seq" ["-f", "%a", "1", "0.1", "1.3"];
    seq_ld_hex_format_upper: "seq" ["-f", "%012.3LA", "-2", "0.7", "2"];
    seq_ld_hex_operands: "seq" ["0x1.8p1", "0x.4p1", "4"];
    seq_ld_zero_increment_before_last: "seq" ["1", "0", "x"];
    seq_ld_format_before_operands: "seq" ["-f", "%x", "a"];
    seq_ld_endpoint_rounding: "seq" ["-f", "%.3f", "0", "0.1", "0.3"];
    seq_ld_user_format_integers: "seq" ["-f", "%+3.0f|%%", "-1", "2"];
    seq_ld_default_format_wide_integers: "seq" ["-w", "-s", ", ", "-9", "4", "10"];

    // --------------------------------------------------------------- date
    date_epoch_utc: "date" ["-d", "@0"] env [("TZ", "UTC")];
    date_epoch_new_york: "date" ["-d", "@0"] env [("TZ", "America/New_York")];
    date_epoch_negative: "date" ["-d", "@-1"] env [("TZ", "UTC")];
    date_epoch_fraction: "date" ["-d", "@1.5", "+%s.%N"] env [("TZ", "UTC")];
    date_epoch_negative_fraction: "date" ["-d", "@-1.5", "+%s.%N"] env [("TZ", "UTC")];
    date_epoch_space: "date" ["-d", "@ 5"] env [("TZ", "UTC")];
    date_utc_flag: "date" ["-u", "-d", "@1700000000"] env [("TZ", "America/New_York")];
    date_universal_long: "date" ["--universal", "--date=@0", "+%T %Z"] env [("TZ", "Asia/Tokyo")];
    date_iso_datetime: "date" ["-d", "2020-01-01 12:00:00", "+%F %T %Z"] env [("TZ", "UTC")];
    date_iso_t_zulu: "date" ["-d", "2020-01-01T10:20:30Z", "+%s"] env [("TZ", "America/New_York")];
    date_iso_offset: "date" ["-d", "2020-01-01T10:20:30+05:30", "+%F %T"] env [("TZ", "UTC")];
    date_zone_utc: "date" ["-d", "2020-01-01 12:00:00 UTC"] env [("TZ", "America/New_York")];
    date_zone_abbrev: "date" ["-d", "2020-01-01 14:00 EST", "+%F %T"] env [("TZ", "UTC")];
    date_zone_numeric: "date" ["-d", "2020-01-01 14:00 -0500", "+%F %T"] env [("TZ", "UTC")];
    date_zone_hours: "date" ["-d", "2020-01-01 14:00 +05", "+%F %T"] env [("TZ", "UTC")];
    date_zone_correction: "date" ["-d", "2000-01-01 12:00:00 utc+1", "+%T"] env [("TZ", "UTC")];
    date_zone_dst_names: "date" ["-d", "2000-01-01 12:00:00 PDT", "+%T"] env [("TZ", "UTC")];
    date_zone_bst: "date" ["-d", "2000-01-01 12:00:00 BST", "+%T"] env [("TZ", "UTC")];
    date_zone_dotted: "date" ["-d", "12:00 E.S.T. 2000-01-01", "+%T"] env [("TZ", "UTC")];
    date_zone_dst_suffix: "date" ["-d", "2000-01-01 12:00 EST DST", "+%T"] env [("TZ", "UTC")];
    date_zone_military: "date" ["-d", "2000-01-01 12:00 A", "+%H:%M"] env [("TZ", "UTC")];
    date_zone_offset_then_rel: "date" ["-d", "2020-01-01 12:00:00 +1 day", "+%F %T %z"] env [("TZ", "UTC")];
    date_dst_local: "date" ["-d", "2020-07-01 12:00:00"] env [("TZ", "America/New_York")];
    date_posix_tz: "date" ["-d", "2020-07-01 12:00", "+%F %T %Z %z"] env [("TZ", "EST5EDT")];
    date_rel_month_overflow: "date" ["-d", "2020-01-31 +1 month", "+%F"] env [("TZ", "UTC")];
    date_rel_leap_year: "date" ["-d", "2020-02-29 +1 year", "+%F %T"] env [("TZ", "UTC")];
    date_rel_yesterday: "date" ["-d", "2020-01-01 yesterday", "+%F"] env [("TZ", "UTC")];
    date_rel_tomorrow: "date" ["-d", "tomorrow 2020-01-01", "+%F %T"] env [("TZ", "UTC")];
    date_rel_week_ago: "date" ["-d", "2020-01-01 1 week ago", "+%F"] env [("TZ", "UTC")];
    date_rel_hours: "date" ["-d", "2020-01-01 +2 hours", "+%F %T"] env [("TZ", "UTC")];
    date_rel_hours_ago: "date" ["-d", "2020-01-01 2 hours ago", "+%F %T"] env [("TZ", "UTC")];
    date_rel_minute: "date" ["-d", "2020-01-01 -1 minute", "+%F %T"] env [("TZ", "UTC")];
    date_rel_next_day: "date" ["-d", "2020-01-01 10:00 next day", "+%F %T"] env [("TZ", "UTC")];
    date_rel_last_year: "date" ["-d", "2020-01-01 last year", "+%F %T"] env [("TZ", "UTC")];
    date_rel_fortnight: "date" ["-d", "2020-01-01 fortnight", "+%F"] env [("TZ", "UTC")];
    date_rel_several: "date" ["-d", "2020-01-01 1 day 2 hours", "+%F %T"] env [("TZ", "UTC")];
    date_rel_hence: "date" ["-d", "2 days hence 2000-01-01", "+%F"] env [("TZ", "UTC")];
    date_rel_hence_alone: "date" ["-d", "2000-01-01 hence", "+%F"] env [("TZ", "UTC")];
    date_rel_seconds_fraction: "date" ["-d", "2000-01-01 1.5 sec", "+%T.%N"] env [("TZ", "UTC")];
    date_rel_across_dst: "date" ["-d", "2020-03-07 12:00 +1 day", "+%F %T %Z"] env [("TZ", "America/New_York")];
    date_rel_hybrid: "date" ["-d", "20000101 +2 days", "+%F"] env [("TZ", "UTC")];
    date_weekday_ignored_with_date: "date" ["-d", "2020-01-01 next monday", "+%F"] env [("TZ", "UTC")];
    date_weekday_ignored_before_date: "date" ["-d", "last monday 2000-01-05", "+%F"] env [("TZ", "UTC")];
    date_weekday_plural_invalid: "date" ["-d", "2000-01-01 3 mondays", "+%F"] env [("TZ", "UTC")];
    date_month_day_year: "date" ["-d", "Jan 5 2021", "+%F"] env [("TZ", "UTC")];
    date_day_month_year: "date" ["-d", "5 Jan 2021", "+%F"] env [("TZ", "UTC")];
    date_full_month_comma: "date" ["-d", "January 5, 2021 3pm", "+%F %T"] env [("TZ", "UTC")];
    date_dashed_month: "date" ["-d", "17-jun-1992", "+%F"] env [("TZ", "UTC")];
    date_month_dashed: "date" ["-d", "jun-17-1992", "+%F"] env [("TZ", "UTC")];
    date_us_slashes: "date" ["-d", "01/05/2021", "+%F"] env [("TZ", "UTC")];
    date_two_digit_year: "date" ["-d", "1/2/03", "+%F"] env [("TZ", "UTC")];
    date_ymd_slashes: "date" ["-d", "2000/01/02", "+%F"] env [("TZ", "UTC")];
    date_compact: "date" ["-d", "20210105", "+%F"] env [("TZ", "UTC")];
    date_compact_time_first: "date" ["-d", "1230 20000101", "+%F %T"] env [("TZ", "UTC")];
    date_ctime_like: "date" ["-d", "Wed Jan  1 12:00:00 UTC 2020", "+%F %T"] env [("TZ", "Asia/Kolkata")];
    date_rfc_email_input: "date" ["-d", "Wed, 01 Jan 2020 12:00:00 +0000", "+%F %T"] env [("TZ", "UTC")];
    date_year_after_time: "date" ["-d", "Jan 1 12:00 2001", "+%F %T"] env [("TZ", "UTC")];
    date_pm: "date" ["-d", "2021-01-05 3:04 pm", "+%T"] env [("TZ", "UTC")];
    date_midnight_am: "date" ["-d", "12am 2021-01-05", "+%T"] env [("TZ", "UTC")];
    date_dotted_am: "date" ["-d", "2000-01-01 12:00 A.M.", "+%T"] env [("TZ", "UTC")];
    date_short_time: "date" ["-d", "2020-01-01 1:2:3", "+%T"] env [("TZ", "UTC")];
    date_iso_t_short: "date" ["-d", "2020-01-01T1:2", "+%T"] env [("TZ", "UTC")];
    date_iso_t_hour_only: "date" ["-d", "2000-01-01T10", "+%T"] env [("TZ", "UTC")];
    date_fraction_comma: "date" ["-d", "2000-01-01 12:00:00,5", "+%N"] env [("TZ", "UTC")];
    date_nanoseconds: "date" ["-d", "2020-01-01 10:20:30.123456789", "+%N %3N %-N"] env [("TZ", "UTC")];
    date_comment: "date" ["-d", "2000-01-01 (comment) 12:00", "+%T"] env [("TZ", "UTC")];
    date_single_digits: "date" ["-d", "2020-1-2", "+%F"] env [("TZ", "UTC")];
    date_year_5: "date" ["-d", "0005-01-01", "+%Y|%C|%y|%G|%g|%F"] env [("TZ", "UTC")];
    date_year_99: "date" ["-d", "0099-01-01", "+%Y %C %y %+4Y %+2C %G %g %F"] env [("TZ", "UTC")];
    date_invalid_word: "date" ["-d", "bogus"] env [("TZ", "UTC")];
    date_invalid_month: "date" ["-d", "2021-13-01"] env [("TZ", "UTC")];
    date_invalid_day: "date" ["-d", "2021-02-30"] env [("TZ", "UTC")];
    date_invalid_leap_second: "date" ["-d", "2020-01-01 00:00:60"] env [("TZ", "UTC")];
    date_invalid_hour_24: "date" ["-d", "2020-01-01 24:00"] env [("TZ", "UTC")];
    date_invalid_noon: "date" ["-d", "2020-01-01 noon"] env [("TZ", "UTC")];
    date_invalid_month_year: "date" ["-d", "Jan 2020"] env [("TZ", "UTC")];
    date_invalid_year_month: "date" ["-d", "2020-06"] env [("TZ", "UTC")];
    date_invalid_two_dates: "date" ["-d", "2020-01-01 2021-01-01"] env [("TZ", "UTC")];
    date_invalid_quoting: "date" ["-d", "it's"] env [("TZ", "UTC")];
    date_dst_gap_invalid: "date" ["-d", "2020-03-08 02:30"] env [("TZ", "America/New_York")];
    date_dst_gap_with_zone: "date" ["-d", "2020-03-08 02:30 -0500", "+%F %T %Z"] env [("TZ", "America/New_York")];
    date_all_conversions: "date" ["-d", "@1700000000", "+%a|%A|%b|%B|%c|%C|%d|%D|%e|%F|%g|%G|%h|%H|%I|%j|%k|%l|%m|%M|%n|%p|%P|%q|%r|%R|%s|%S|%t|%T|%u|%U|%V|%w|%W|%x|%X|%y|%Y|%z|%:z|%::z|%:::z|%Z|%%"] env [("TZ", "UTC")];
    date_zone_conversions_ny: "date" ["-d", "@1700000000", "+%z|%:z|%::z|%:::z|%Z"] env [("TZ", "America/New_York")];
    date_zone_conversions_kolkata: "date" ["-d", "@1700000000", "+%z|%:z|%::z|%:::z|%Z"] env [("TZ", "Asia/Kolkata")];
    date_zone_conversions_negative_half: "date" ["-d", "@1700000000", "+%z|%:z|%:::z|%-z|%_z|%8:z"] env [("TZ", "America/St_Johns")];
    date_flags: "date" ["-d", "@1700000000", "+%-d|%_H|%^a|%#a|%#Z|%#p|%^p|%-j|%_j|%05d|%10A|%-10A|%_5m|%^B|%^10b|%3S|%-:z"] env [("TZ", "UTC")];
    date_flags_hours: "date" ["-d", "@1700000005", "+%-I|%_I|%-l|%-k|%e|%-e|%0e|%_d|%0k|%_y|%_C"] env [("TZ", "UTC")];
    date_flags_plus: "date" ["-d", "2023-11-14 05:06:07", "+%+d|%+3d|%+Y|%+5Y|%+C|%+3C|%+m|%+s"] env [("TZ", "UTC")];
    date_flags_year_width: "date" ["-d", "@0", "+%+4Y|%+Y|%_Y|%-Y|%10Y|%010Y"] env [("TZ", "UTC")];
    date_flags_year_sign: "date" ["-d", "1999-01-01", "+%+4Y|%+Y|%+6Y"] env [("TZ", "UTC")];
    date_flags_nanoseconds: "date" ["-d", "@0", "+%5N|%12N|%_3N|%-3N|%ON"] env [("TZ", "UTC")];
    date_flags_zone: "date" ["-d", "@0", "+%5z|%-z|%_z|%8:z|%Ez|%Oz"] env [("TZ", "UTC")];
    date_flags_text: "date" ["-d", "@0", "+%5p|%-5p|%^5a|%_5a|%05a|%10Z|%010Z|%#b|%#A|%#P|%^#p"] env [("TZ", "UTC")];
    date_flags_epoch: "date" ["-d", "@-100", "+%s|%010s|%_10s|%5s|%-s"] env [("TZ", "UTC")];
    date_flags_composite: "date" ["-d", "@0", "+[%5c][%^c][%#c][%12F][%_12F][%-F][%015D][%^r][%10T]"] env [("TZ", "UTC")];
    date_bad_conversions: "date" ["-d", "@0", "+[%5%][%-%][%_5%][%^%][%E%][%5n][%3t][%Q][%E|][%::::z][%:a][%Ed][%Oa][%Ec][%Oy]"] env [("TZ", "UTC")];
    date_percent_at_end: "date" ["-d", "@0", "+a%"] env [("TZ", "UTC")];
    date_percent_width_at_end: "date" ["-d", "@0", "+a%5"] env [("TZ", "UTC")];
    date_empty_format: "date" ["-d", "@0", "+"] env [("TZ", "UTC")];
    date_iso_weeks_end: "date" ["-d", "2020-12-31", "+%U|%W|%V|%G|%g|%j|%u|%w"] env [("TZ", "UTC")];
    date_iso_weeks_start: "date" ["-d", "2021-01-03", "+%U|%W|%V|%G|%g|%j|%u|%w"] env [("TZ", "UTC")];
    date_iso_weeks_2026: "date" ["-d", "2026-12-28", "+%U|%W|%V|%G|%g"] env [("TZ", "UTC")];
    date_rfc_email: "date" ["-R", "-d", "@0"] env [("TZ", "America/New_York")];
    date_rfc_email_utc: "date" ["-u", "-R", "-d", "@0"] env [("TZ", "UTC")];
    date_iso_default: "date" ["-I", "-d", "@0"] env [("TZ", "UTC")];
    date_iso_hours: "date" ["-Ihours", "-d", "@0"] env [("TZ", "Asia/Kolkata")];
    date_iso_minutes: "date" ["--iso-8601=minutes", "-d", "@0"] env [("TZ", "UTC")];
    date_iso_seconds: "date" ["-Iseconds", "-d", "@0"] env [("TZ", "America/New_York")];
    date_iso_ns: "date" ["-Ins", "-d", "@0.5"] env [("TZ", "UTC")];
    date_iso_abbrev: "date" ["-Ih", "-d", "@0"] env [("TZ", "UTC")];
    date_iso_invalid: "date" ["-Ix", "-d", "@0"] env [("TZ", "UTC")];
    date_rfc3339_date: "date" ["--rfc-3339=date", "-d", "@0"] env [("TZ", "UTC")];
    date_rfc3339_seconds: "date" ["--rfc-3339=seconds", "-d", "@0"] env [("TZ", "UTC")];
    date_rfc3339_ns: "date" ["--rfc-3339=ns", "-d", "@0"] env [("TZ", "America/New_York")];
    date_rfc3339_missing: "date" ["--rfc-3339"] env [("TZ", "UTC")];
    date_rfc3339_invalid: "date" ["--rfc-3339=hours", "-d", "@0"] env [("TZ", "UTC")];
    date_multiple_formats: "date" ["-R", "-I", "-d", "@0"] env [("TZ", "UTC")];
    date_format_and_option: "date" ["-R", "-d", "@0", "+%s"] env [("TZ", "UTC")];
    date_extra_operand: "date" ["-d", "@0", "+%s", "+%s"] env [("TZ", "UTC")];
    date_lacks_plus: "date" ["-d", "@0", "x"] env [("TZ", "UTC")];
    date_lacks_plus_empty: "date" ["-d", "@0", ""] env [("TZ", "UTC")];
    date_options_after_format: "date" ["+%s", "-d", "@42"] env [("TZ", "UTC")];
    date_reference: "date" ["-r", "a.txt", "+%s.%N"] env [("TZ", "UTC")];
    date_reference_formatted: "date" ["--reference=a.txt"] env [("TZ", "Europe/Paris")];
    date_reference_missing: "date" ["-r", "nonexist"] env [("TZ", "UTC")];
    date_reference_and_date: "date" ["-r", "a.txt", "-d", "@0"] env [("TZ", "UTC")];
    date_file: "date" ["-f", "dates.txt", "+%F %T"] env [("TZ", "UTC")];
    date_file_stdin: "date" ["-f", "-", "+%s"] env [("TZ", "UTC")] stdin b"@1\n@2\n";
    date_file_missing: "date" ["-f", "nonexist"] env [("TZ", "UTC")];
    date_missing_date_arg: "date" ["-d"] env [("TZ", "UTC")];
    date_empty_tz: "date" ["-d", "@0"] env [("TZ", "")];
    date_unknown_tz: "date" ["-d", "@0"] env [("TZ", "Foo/Bar")];
    date_utc0: "date" ["-d", "@0"] env [("TZ", "UTC0")];

    // ---------------------------------------------------------------- tee
    tee_stdout_only: "tee" [] stdin b"hello\n";
    tee_to_files: "tee" ["out1.tmp", "out2.tmp"] stdin b"hello\n";
    tee_append: "tee" ["-a", "out3.tmp"] stdin b"again\n";
    tee_dash_is_a_file: "tee" ["-"] stdin b"dash\n";
    tee_open_error: "tee" ["/nonexist/x", "out4.tmp"] stdin b"hi\n";
    tee_directory: "tee" ["d"] stdin b"hi\n";
    tee_write_error: "tee" ["/dev/full"] stdin b"hi\n";
    tee_write_error_p: "tee" ["-p", "/dev/full"] stdin b"hi\n";
    tee_write_error_warn: "tee" ["--output-error=warn", "/dev/full"] stdin b"hi\n";
    tee_write_error_exit: "tee" ["--output-error=exit", "/dev/full", "out5.tmp"] stdin b"hi\n";
    tee_output_error_default: "tee" ["--output-error", "/dev/null"] stdin b"hi\n";
    tee_output_error_abbrev: "tee" ["--output-error=warn-n", "/dev/null"] stdin b"hi\n";
    tee_output_error_ambiguous: "tee" ["--output-error=warn"] stdin b"hi\n";
    tee_output_error_invalid: "tee" ["--output-error=bogus"] stdin b"hi\n";
    tee_ignore_interrupts: "tee" ["-i"] stdin b"hi\n";
    tee_binary: "tee" [] stdin &[0u8, 255, 10, 13, 0];

    // ------------------------------------------- yes (error paths only)
    yes_bad_option: "yes" ["a", "-x"];
    yes_bad_long_option: "yes" ["--bogus"];

    // ---------------------------- unknown options: every GNU coreutils tool
    // (chmod and tr parse their own operands and are not covered yet).
    unknown_long_ls: "ls" ["--bogus"];
    unknown_short_ls: "ls" ["-j"];
    unknown_long_cp: "cp" ["--bogus"];
    unknown_short_cp: "cp" ["-j"];
    unknown_long_mv: "mv" ["--bogus"];
    unknown_short_mv: "mv" ["-j"];
    unknown_long_rm: "rm" ["--bogus"];
    unknown_short_rm: "rm" ["-j"];
    unknown_long_mkdir: "mkdir" ["--bogus"];
    unknown_short_mkdir: "mkdir" ["-j"];
    unknown_long_ln: "ln" ["--bogus"];
    unknown_short_ln: "ln" ["-j"];
    unknown_long_chown: "chown" ["--bogus"];
    unknown_short_chown: "chown" ["-j"];
    unknown_long_stat: "stat" ["--bogus"];
    unknown_short_stat: "stat" ["-j"];
    unknown_long_touch: "touch" ["--bogus"];
    unknown_short_touch: "touch" ["-j"];
    unknown_long_df: "df" ["--bogus"];
    unknown_short_df: "df" ["-j"];
    unknown_long_du: "du" ["--bogus"];
    unknown_short_du: "du" ["-j"];
    unknown_long_realpath: "realpath" ["--bogus"];
    unknown_short_realpath: "realpath" ["-j"];
    unknown_long_basename: "basename" ["--bogus"];
    unknown_short_basename: "basename" ["-j"];
    unknown_long_dirname: "dirname" ["--bogus"];
    unknown_short_dirname: "dirname" ["-j"];
    unknown_long_mktemp: "mktemp" ["--bogus"];
    unknown_short_mktemp: "mktemp" ["-j"];
    unknown_long_cat: "cat" ["--bogus"];
    unknown_short_cat: "cat" ["-j"];
    unknown_long_tac: "tac" ["--bogus"];
    unknown_short_tac: "tac" ["-j"];
    unknown_long_head: "head" ["--bogus"];
    unknown_short_head: "head" ["-j"];
    unknown_long_tail: "tail" ["--bogus"];
    unknown_short_tail: "tail" ["-j"];
    unknown_long_wc: "wc" ["--bogus"];
    unknown_short_wc: "wc" ["-j"];
    unknown_long_sort: "sort" ["--bogus"];
    unknown_short_sort: "sort" ["-j"];
    unknown_long_uniq: "uniq" ["--bogus"];
    unknown_short_uniq: "uniq" ["-j"];
    unknown_long_cut: "cut" ["--bogus"];
    unknown_short_cut: "cut" ["-j"];
    unknown_long_paste: "paste" ["--bogus"];
    unknown_short_paste: "paste" ["-j"];
    unknown_long_nl: "nl" ["--bogus"];
    unknown_short_nl: "nl" ["-j"];
    unknown_long_shuf: "shuf" ["--bogus"];
    unknown_short_shuf: "shuf" ["-j"];
    unknown_long_b2sum: "b2sum" ["--bogus"];
    unknown_short_b2sum: "b2sum" ["-j"];
    unknown_long_sha256sum: "sha256sum" ["--bogus"];
    unknown_short_sha256sum: "sha256sum" ["-j"];
    unknown_long_md5sum: "md5sum" ["--bogus"];
    unknown_short_md5sum: "md5sum" ["-j"];
    unknown_long_base64: "base64" ["--bogus"];
    unknown_short_base64: "base64" ["-j"];
    unknown_long_basenc: "basenc" ["--bogus"];
    unknown_short_basenc: "basenc" ["-j"];
    unknown_long_echo: "echo" ["--bogus"];
    unknown_short_echo: "echo" ["-j"];
    unknown_long_printf: "printf" ["--bogus"];
    unknown_short_printf: "printf" ["-j"];
    unknown_long_env: "env" ["--bogus"];
    unknown_short_env: "env" ["-j"];
    unknown_long_printenv: "printenv" ["--bogus"];
    unknown_short_printenv: "printenv" ["-j"];
    unknown_long_id: "id" ["--bogus"];
    unknown_short_id: "id" ["-j"];
    unknown_long_uname: "uname" ["--bogus"];
    unknown_short_uname: "uname" ["-j"];
    unknown_long_nproc: "nproc" ["--bogus"];
    unknown_short_nproc: "nproc" ["-j"];
    unknown_long_whoami: "whoami" ["--bogus"];
    unknown_short_whoami: "whoami" ["-j"];
    unknown_long_yes: "yes" ["--bogus=1"];
    unknown_short_yes: "yes" ["-j"];
    unknown_long_true: "true" ["--bogus"];
    unknown_short_false: "false" ["-j"];
    unknown_long_sleep: "sleep" ["--bogus"];
    unknown_short_sleep: "sleep" ["-j"];
    unknown_long_timeout: "timeout" ["--bogus=3"];
    unknown_short_timeout: "timeout" ["-j"];
    unknown_long_seq: "seq" ["--bogus"];
    unknown_short_seq: "seq" ["-j"];
    unknown_long_date: "date" ["--bogus"];
    unknown_short_date: "date" ["-j"];
    unknown_long_tee: "tee" ["--bogus"];
    unknown_short_tee: "tee" ["-j"];
    unknown_cluster_cat: "cat" ["-nj"];
    missing_arg_short_base64: "base64" ["-w"];
    missing_arg_long_base64: "base64" ["--wrap"];
    flag_with_value_base64: "base64" ["--decode=1"];
    extra_operand_whoami: "whoami" ["a", "b"];
    extra_operand_base64: "basenc" ["--base64", "a.txt", "b.txt"];

    // ------------------------------- Dogfish switches never change GNU's
    echo_color_is_an_operand: "echo" ["--color", "x"];
    echo_switches_are_operands: "echo" ["--stats", "--no-color", "--color=always", "x"];
    printf_stats_is_the_format: "printf" ["--stats"];
    printf_stats_is_an_argument: "printf" ["%s|", "--stats", "--color"];
    true_ignores_switches: "true" ["--stats"];
    false_ignores_switches: "false" ["--no-color"];
    env_command_keeps_its_switches: "env" ["echo", "--color", "--stats", "x"];
    env_assignment_then_command_switches: "env" ["A=1", "echo", "--no-color"];
    timeout_command_keeps_its_switches: "timeout" ["5", "echo", "--stats", "--color=never"];
    switch_after_double_dash_is_an_operand: "cat" ["--", "--stats"];
    switch_is_never_abbreviated: "cat" ["--stat", "a.txt"];
    switch_with_argument_is_unrecognized: "cat" ["--stats=1", "a.txt"];
    color_only_where_granted: "cat" ["--color", "a.txt"];
    extension_not_in_ambiguity_ls: "ls" ["--t", "a.txt"];
    extension_not_abbreviated_ls: "ls" ["--tre", "a.txt"];
    extension_not_in_ambiguity_sort: "sort" ["--b", "lines.txt"];
    extension_not_shadowing_prefix_cut: "cut" ["--fi=1", "a.txt"];
}

// ------------------------------------------------------------------ yes
//
// `yes` never ends, so compare a bounded prefix of its output instead.

/// The first `limit` bytes `cmd` writes, after which it is killed.
fn output_prefix(mut cmd: Command, limit: usize) -> Vec<u8> {
    let mut child = cmd
        .current_dir(fixtures())
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let mut buf = vec![0u8; limit];
    child
        .stdout
        .take()
        .unwrap()
        .read_exact(&mut buf)
        .expect("read output");
    let _ = child.kill();
    let _ = child.wait();
    buf
}

fn check_yes(args: &[&str]) {
    let Some(gnu) = gnu_tool("yes") else {
        eprintln!("note: skipping — system `yes` is missing or not GNU coreutils");
        return;
    };
    let mut ours = Command::new(dogfish_bin());
    ours.arg("yes").args(args);
    let mut theirs = Command::new(gnu);
    theirs.arg0("yes").args(args);
    // More than one 64 KiB buffer, so buffer boundaries are covered too.
    let limit = 200_000;
    assert_eq!(
        output_prefix(ours, limit),
        output_prefix(theirs, limit),
        "yes {args:?}"
    );
}

#[test]
fn yes_default() {
    check_yes(&[]);
}

#[test]
fn yes_joins_arguments() {
    check_yes(&["a", "b c", ""]);
}

#[test]
fn yes_dash_dash() {
    check_yes(&["--", "-x", "--"]);
}

#[test]
fn yes_long_line() {
    let long = "x".repeat(70_000);
    check_yes(&[&long]);
}

// ----------------------------------------------- tee / timeout environments

/// Runs `cmd` with stdout on a non-blocking pipe that is drained only after a
/// pause, so that writes hit EAGAIN; returns the exit status and the bytes read.
fn run_nonblocking_stdout(mut cmd: Command, input: &[u8]) -> (Option<i32>, usize) {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let (mut read_end, write_end) = std::io::pipe().expect("pipe");
    // SAFETY: fcntl on a descriptor owned by `write_end` takes no pointers;
    // O_NONBLOCK is shared with the child through the open file description.
    unsafe {
        let fd = write_end.as_raw_fd();
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::from(write_end))
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    // The Command still holds a copy of the write end; close it so the read
    // sees end of file when the child exits.
    drop(cmd);
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    std::thread::sleep(Duration::from_millis(200));
    let mut out = Vec::new();
    read_end.read_to_end(&mut out).unwrap();
    feeder.join().unwrap();
    (child.wait().unwrap().code(), out.len())
}

#[test]
fn tee_nonblocking_stdout_waits() {
    let Some(gnu) = gnu_tool("tee") else {
        return;
    };
    let input = vec![b'x'; 1 << 20];
    let mut ours = Command::new(dogfish_bin());
    ours.arg("tee");
    let mut theirs = Command::new(gnu);
    theirs.arg0("tee");
    let a = run_nonblocking_stdout(ours, &input);
    let b = run_nonblocking_stdout(theirs, &input);
    assert_eq!(a, b);
    assert_eq!(a, (Some(0), input.len()));
}

#[test]
fn timeout_with_sigchld_ignored() {
    let Some(gnu) = gnu_tool("timeout") else {
        return;
    };
    let run = |mut cmd: Command| {
        cmd.args(["10", "sh", "-c", "exit 3"]);
        // SAFETY: only signal(2) between fork and exec, like `trap '' CHLD`.
        unsafe {
            cmd.pre_exec(|| {
                libc::signal(libc::SIGCHLD, libc::SIG_IGN);
                Ok(())
            });
        }
        exec(cmd, fixtures(), None, "C")
    };
    let mut ours = Command::new(dogfish_bin());
    ours.arg("timeout");
    let mut theirs = Command::new(gnu);
    theirs.arg0("timeout");
    let (a, b) = (run(ours), run(theirs));
    assert_eq!(a.status.code(), b.status.code());
    assert_eq!(a.stderr, b.stderr);
    assert_eq!(a.status.code(), Some(3));
}

// ----------------------------------------------------------------- --help
//
// Help text differs by design; GNU's exit status for `--help` must match.
// These checks need no fixtures, so they run anywhere.

#[test]
fn help_exits_like_gnu() {
    let tools = [
        "echo",
        "printf",
        "env",
        "printenv",
        "id",
        "uname",
        "nproc",
        "whoami",
        "yes",
        "true",
        "false",
        "sleep",
        "timeout",
        "seq",
        "date",
        "tee",
        "b2sum",
        "sha256sum",
        "md5sum",
        "base64",
        "basenc",
    ];
    for tool in tools {
        let mut ours = Command::new(dogfish_bin());
        ours.args([tool, "--help"]);
        let ours = exec(ours, &std::env::temp_dir(), None, "C");
        assert!(!ours.stdout.is_empty(), "{tool} --help printed nothing");
        assert!(ours.stderr.is_empty(), "{tool} --help wrote to stderr");
        let Some(gnu) = gnu_tool(tool) else {
            assert_eq!(
                ours.status.code(),
                Some(i32::from(tool == "false")),
                "{tool}"
            );
            continue;
        };
        let mut theirs = Command::new(gnu);
        theirs.arg0(tool).arg("--help");
        let theirs = exec(theirs, &std::env::temp_dir(), None, "C");
        assert_eq!(
            ours.status.code(),
            theirs.status.code(),
            "{tool} --help exit status"
        );
    }
}

// ------------------------------------------------------------ hostname
//
// `hostname` is not part of GNU coreutils; check it against uname -n.

#[test]
fn hostname_matches_uname_nodename() {
    let run = |args: &[&str]| {
        let mut cmd = Command::new(dogfish_bin());
        cmd.args(args);
        exec(cmd, &std::env::temp_dir(), None, "C")
    };
    let host = run(&["hostname"]);
    assert!(host.status.success());
    assert_eq!(host.stdout, run(&["uname", "-n"]).stdout);
    let short = run(&["hostname", "-s"]);
    let full = String::from_utf8_lossy(&host.stdout).into_owned();
    let expected = format!("{}\n", full.trim_end().split('.').next().unwrap());
    assert_eq!(String::from_utf8_lossy(&short.stdout), expected);
    assert_eq!(run(&["hostname", "--bogus"]).status.code(), Some(1));
}

// ---------------------------------------------------------------- watch
//
// `watch` comes from procps, not coreutils: a sanity check only.

#[test]
fn watch_runs_the_command_count_times() {
    let mut cmd = Command::new(dogfish_bin());
    cmd.args(["watch", "-n", "0.01", "-c", "2", "echo", "tick"]);
    let out = exec(cmd, &std::env::temp_dir(), None, "C");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(text.lines().filter(|l| *l == "tick").count(), 2, "{text}");
}

// ------------------------------------------------------------ timeout
//
// The signal must reach the whole process group the command runs in.

#[test]
fn timeout_kills_grandchildren() {
    let start = std::time::Instant::now();
    let mut cmd = Command::new(dogfish_bin());
    cmd.args(["timeout", "0.2", "sh", "-c", "sleep 5 & sleep 5; wait"]);
    let out = exec(cmd, &std::env::temp_dir(), None, "C");
    assert_eq!(out.status.code(), Some(124));
    assert!(
        start.elapsed() < Duration::from_secs(4),
        "{:?}",
        start.elapsed()
    );
}

// ---------------------------------------------------- getopt_long & stdout
//
// `--help` and `--version` text differs by design, so these compare exit
// status and stderr: like glibc getopt_long, both are ordinary options that
// may come anywhere (and be abbreviated), and the first one wins.

/// Exit status and stderr of one run.
type Outcome = (Option<i32>, String);

/// Runs `tool args` through both implementations, with `env` set and stdout
/// sent to `stdout`; returns `(dogfish, GNU)`, or `None` without GNU.
fn run_both(
    tool: &str,
    args: &[&str],
    env: &[(&str, &str)],
    stdout: impl Fn() -> Stdio,
) -> Option<(Outcome, Outcome)> {
    let Some(gnu) = gnu_tool(tool) else {
        eprintln!("note: skipping — system `{tool}` is missing or not GNU coreutils");
        return None;
    };
    let run = |mut cmd: Command| {
        let out = cmd
            .args(args)
            .current_dir(fixtures())
            .env("LC_ALL", "C")
            .env_remove("POSIXLY_CORRECT")
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(stdout())
            .stderr(Stdio::piped())
            .output()
            .expect("spawn");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let mut ours = Command::new(dogfish_bin());
    ours.arg(tool);
    let mut theirs = Command::new(gnu);
    theirs.arg0(tool);
    Some((run(ours), run(theirs)))
}

#[test]
fn help_and_version_anywhere_like_gnu() {
    let cases: &[(&str, &[&str])] = &[
        ("sleep", &["100", "--help"]),
        ("cat", &["nonexist", "--help"]),
        ("head", &["--he"]),
        ("tail", &["--vers"]),
        ("yes", &["x", "--version"]),
        ("seq", &["--help", "--bogus"]),
        ("wc", &["--bogus", "--help"]),
        ("sort", &["--help", "--version"]),
        ("env", &["--version", "--help"]),
        ("timeout", &["--he"]),
        ("printenv", &["--he"]),
        ("true", &["--help", "x"]),
        ("false", &["--version"]),
        ("echo", &["--help", "x"]),
    ];
    for (tool, args) in cases {
        let Some((ours, theirs)) = run_both(tool, args, &[], Stdio::null) else {
            continue;
        };
        assert_eq!(ours, theirs, "{tool} {args:?}");
    }
}

/// `yes` never ends when it works, so only its failures are compared.
#[test]
fn yes_takes_no_dogfish_switches() {
    for args in [&["--stats"][..], &["--no-color", "x"], &["--color"]] {
        let Some((ours, theirs)) = run_both("yes", args, &[], Stdio::null) else {
            return;
        };
        assert_eq!(ours, theirs, "yes {args:?}");
    }
}

#[test]
fn posixly_correct_stops_at_first_operand() {
    let cases: &[(&str, &[&str])] = &[
        ("cat", &["a.txt", "-n"]),
        ("head", &["a.txt", "-n", "1"]),
        ("wc", &["a.txt", "-l"]),
        ("uniq", &["a.txt", "-c"]),
    ];
    for (tool, args) in cases {
        let env = [("POSIXLY_CORRECT", "1")];
        let Some((ours, theirs)) = run_both(tool, args, &env, Stdio::null) else {
            continue;
        };
        assert_eq!(ours, theirs, "POSIXLY_CORRECT {tool} {args:?}");
    }
}

/// Like GNU's `close_stdout`, a failed write to standard output is
/// diagnosed once and fails the tool.
#[test]
fn write_error_like_gnu() {
    if !Path::new("/dev/full").exists() {
        eprintln!("note: skipping — no /dev/full");
        return;
    }
    let full = || {
        let f = std::fs::OpenOptions::new().write(true).open("/dev/full");
        Stdio::from(f.expect("open /dev/full"))
    };
    let cases: &[(&str, &[&str])] = &[
        ("cat", &["a.txt"]),
        ("echo", &["hi"]),
        ("printf", &["x"]),
        ("seq", &["3"]),
        ("head", &["a.txt"]),
        ("tail", &["a.txt"]),
        ("wc", &["a.txt"]),
        ("md5sum", &["a.txt"]),
        ("base64", &["a.txt"]),
        ("date", &["-d", "@0"]),
        ("id", &["-u"]),
        ("uname", &[]),
        ("cat", &["--help"]),
        ("sleep", &["--version"]),
    ];
    for (tool, args) in cases {
        let Some((ours, theirs)) = run_both(tool, args, &[("TZ", "UTC")], full) else {
            continue;
        };
        assert_eq!(ours, theirs, "{tool} {args:?} >/dev/full");
    }
}

/// How standard output fails in `write_errors_match_gnu`.
#[derive(Clone, Copy, Debug)]
enum WriteFailure {
    /// `>/dev/full`: ENOSPC.
    Full,
    /// A pipe whose reader is gone, with SIGPIPE ignored (`trap '' PIPE`):
    /// EPIPE.
    BrokenPipe,
    /// `>&-`: EBADF.
    Closed,
}

/// Runs `tool args` through both implementations with standard output
/// failing as `how` says; returns `(dogfish, GNU)`, or `None` without GNU.
fn run_failing(tool: &str, args: &[&str], how: WriteFailure) -> Option<(Outcome, Outcome)> {
    let gnu = gnu_tool(tool)?;
    let run = |mut cmd: Command| {
        let stdout = match how {
            WriteFailure::Full => Stdio::from(
                std::fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/full")
                    .expect("open /dev/full"),
            ),
            WriteFailure::BrokenPipe => {
                let (read_end, write_end) = std::io::pipe().expect("pipe");
                drop(read_end);
                Stdio::from(write_end)
            }
            WriteFailure::Closed => Stdio::null(),
        };
        // SAFETY: the closure runs in the forked child and only calls
        // signal(2) and close(2), which are async-signal-safe.
        unsafe {
            cmd.pre_exec(move || {
                match how {
                    WriteFailure::Full => {}
                    WriteFailure::BrokenPipe => {
                        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
                    }
                    WriteFailure::Closed => {
                        libc::close(libc::STDOUT_FILENO);
                    }
                }
                Ok(())
            });
        }
        let out = cmd
            .args(args)
            .current_dir(fixtures())
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .env_remove("POSIXLY_CORRECT")
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(Stdio::piped())
            .output()
            .expect("spawn");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let mut ours = Command::new(dogfish_bin());
    ours.arg(tool);
    let mut theirs = Command::new(gnu);
    theirs.arg0(tool);
    Some((run(ours), run(theirs)))
}

/// Every way a write to standard output can fail gets GNU's diagnostic (or
/// none), at GNU's point, with GNU's exit status: `close_stdout`'s `write
/// error` with or without the reason, the tools' own messages (`yes:
/// standard output`, `head: error writing`, `sort: write failed`), tools
/// that stop at the first failed write and tools that go on.
#[test]
fn write_errors_match_gnu() {
    if !Path::new("/dev/full").exists() {
        eprintln!("note: skipping — no /dev/full");
        return;
    }
    let cases: &[(&str, &[&str])] = &[
        ("yes", &[]),
        ("yes", &["abc"]),
        ("head", &["-c", "1000000", "/dev/zero"]),
        ("head", &["-c", "4096", "/dev/zero"]),
        ("head", &["-c", "4095", "/dev/zero"]),
        ("head", &["-n", "-3", "big.dat"]),
        ("head", &["a.txt", "nonexist"]),
        ("tail", &["-c", "5000", "big.dat"]),
        ("tail", &["-c", "100", "big.dat"]),
        ("tail", &["a.txt", "nonexist"]),
        ("printf", &["%01000000d", "0"]),
        ("printf", &["%04096d", "0"]),
        ("printf", &["%04095d", "0"]),
        ("printf", &["a%04096d", "0"]),
        ("printf", &["%05000d\\n", "0"]),
        ("printf", &["%d\\n", "x"]),
        ("printf", &["abc%z"]),
        ("printf", &["%s", "hello"]),
        ("printf", &["%2147483647d", "0"]),
        ("printf", &["a%2147483647db\\n", "0"]),
        ("echo", &["hi"]),
        ("seq", &["100000"]),
        ("cat", &["a.txt"]),
        ("cat", &["big.dat"]),
        ("wc", &["a.txt", "nonexist"]),
        ("wc", &["--help"]),
        ("md5sum", &["a.txt", "nonexist"]),
        ("md5sum", &["--version"]),
        ("stat", &["-c", "%s", "a.txt", "nonexist"]),
        ("base64", &["big.dat"]),
        ("base64", &["-d", "bigbad.b64"]),
        ("sort", &["big.dat"]),
        ("sort", &["lines.txt"]),
        ("sort", &["--help"]),
        ("sort", &["-m", "big.dat"]),
        ("sort", &["-m", "lines.txt", "lines.txt"]),
        ("sort", &["-S", "1k", "big.dat"]),
        ("sort", &["-S", "1k", "lines.txt"]),
        ("sort", &["--debug", "lines.txt"]),
        ("sort", &["-c", "big.dat"]),
        ("du", &["a.txt"]),
        ("du", &["--version"]),
        ("ls", &["--color=always", "d"]),
        ("ls", &["nonexist", "d"]),
        ("cut", &["-c1", "a.txt", "nonexist"]),
        ("nl", &["a.txt", "nonexist"]),
        ("date", &["-f", "dates.txt"]),
        ("env", &["yes"]),
        ("timeout", &["10", "yes"]),
    ];
    for how in [
        WriteFailure::Full,
        WriteFailure::BrokenPipe,
        WriteFailure::Closed,
    ] {
        for (tool, args) in cases {
            let Some((ours, theirs)) = run_failing(tool, args, how) else {
                continue;
            };
            assert_eq!(ours, theirs, "{tool} {args:?} with {how:?}");
        }
    }
}

/// `env` and `timeout` run their command with SIGPIPE ignored when it was
/// ignored for them, as GNU's do.
#[test]
fn commands_inherit_ignored_sigpipe() {
    for (tool, args) in [
        ("env", &["grep", "^SigIgn", "/proc/self/status"][..]),
        ("timeout", &["10", "grep", "^SigIgn", "/proc/self/status"]),
    ] {
        if !Path::new("/proc/self/status").exists() {
            return;
        }
        let run = |mut cmd: Command| {
            // SAFETY: the closure runs in the forked child and only calls
            // signal(2), which is async-signal-safe.
            unsafe {
                cmd.pre_exec(|| {
                    libc::signal(libc::SIGPIPE, libc::SIG_IGN);
                    Ok(())
                });
            }
            let out = cmd.args(args).output().expect("spawn");
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let mut ours = Command::new(dogfish_bin());
        ours.arg(tool);
        let shown = run(ours);
        let mask = shown.trim().rsplit('\t').next().unwrap_or("");
        let mask = u64::from_str_radix(mask, 16).unwrap_or(0);
        assert_ne!(mask & (1 << (libc::SIGPIPE - 1)), 0, "{tool}: {shown:?}");
    }
}

/// A small deterministic generator for the randomized cases.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A random `strtold` argument: decimal numbers over the whole 80-bit range
/// (subnormals and overflow included), hexadecimal ones, integers around
/// powers of two, halfway cases and specials.
fn random_number(r: &mut Lcg, max_exp: i64) -> String {
    let sign = ["", "", "-", "+"][r.below(4) as usize];
    let digits = |r: &mut Lcg, n: u64| -> String {
        (0..n)
            .map(|_| char::from(b'0' + r.below(10) as u8))
            .collect()
    };
    let body = match r.below(10) {
        0..=3 => {
            let n = 1 + r.below(25);
            let d = digits(r, n);
            let point = r.below(n + 1) as usize;
            let e = r.below(2 * max_exp as u64 + 1) as i64 - max_exp;
            format!("{}.{}e{e}", &d[..point], &d[point..])
        }
        4 => {
            let (n, m) = (1 + r.below(8), r.below(8));
            let int = digits(r, n);
            format!("{int}.{}", digits(r, m))
        }
        5 => {
            let hex: String = (0..1 + r.below(18))
                .map(|_| char::from(b"0123456789abcdef"[r.below(16) as usize]))
                .collect();
            let e = r.below(2 * max_exp as u64 * 3 + 1) as i64 - max_exp * 3;
            format!("0x{hex}p{e}")
        }
        6 => {
            let k = r.below(70);
            let delta = r.below(5) as i128 - 2;
            ((1i128 << k) + delta).to_string()
        }
        7 => {
            // Halfway between two decimal digits at some precision.
            let (n, m) = (1 + r.below(3), r.below(6));
            let int = digits(r, n);
            format!("{int}.{}5", digits(r, m))
        }
        8 => [
            "inf",
            "nan",
            "0",
            "1",
            "0.5",
            "1e4932",
            "0x1p-16445",
            "1e-4950",
        ][r.below(8) as usize]
            .to_string(),
        _ => format!(
            "0x{:x}.{:x}p{}",
            r.below(16),
            r.next(),
            r.below(200) as i64 - 100
        ),
    };
    format!("{sign}{body}")
}

/// A random floating-point directive.
fn random_directive(r: &mut Lcg) -> (String, u8) {
    let conv = b"aAeEfFgG"[r.below(8) as usize];
    let mut d = String::from("%");
    for flag in ["-", "+", " ", "#", "0"] {
        if r.below(5) == 0 {
            d.push_str(flag);
        }
    }
    if r.below(2) == 0 {
        d.push_str(&r.below(30).to_string());
    }
    if r.below(3) != 0 {
        d.push('.');
        d.push_str(
            &r.below(if conv | 0x20 == b'a' { 20 } else { 40 })
                .to_string(),
        );
    }
    if r.below(2) == 0 {
        d.push('L');
    }
    d.push(char::from(conv));
    (d, conv)
}

/// Thousands of random numbers through random `long double` directives, in
/// printf and in seq, against GNU.
#[test]
fn printf_seq_long_double_randomized() {
    let mut r = Lcg(0x5eed_1d0b);
    for _ in 0..60 {
        let (directive, conv) = random_directive(&mut r);
        // %f of numbers near 1e4932 prints thousands of digits each.
        let max_exp = if conv | 0x20 == b'f' { 40 } else { 4960 };
        let numbers: Vec<String> = (0..100).map(|_| random_number(&mut r, max_exp)).collect();
        let format = format!("{directive}|\n");
        let mut args = vec![format.as_str()];
        args.extend(numbers.iter().map(String::as_str));
        common::check_lazy(
            fixtures,
            common::Case {
                tool: "printf",
                args: &args,
                stdin: None,
                compare_stderr: true,
                locale: "C",
                env: &[],
            },
        );
        // seq: a few steps from one number by another of a similar size, so
        // that every step moves (FIRST + i STEP never stalls).
        for _ in 0..4 {
            let first = random_number(&mut r, 6);
            let Ok(f) = first.parse::<f64>() else {
                continue;
            };
            if !f.is_finite() || f == 0.0 {
                continue;
            }
            let step = random_number(&mut r, 6);
            let Ok(st) = step.parse::<f64>() else {
                continue;
            };
            if !st.is_finite() || st.abs() < f.abs() * 1e-9 || st.abs() < 1e-30 {
                continue;
            }
            let last = format!("{:e}", f + st * r.below(20) as f64);
            let fmt = format!("-f{directive}");
            let fmt_arg = if r.below(3) == 0 { "-w" } else { fmt.as_str() };
            common::check_lazy(
                fixtures,
                common::Case {
                    tool: "seq",
                    args: &[fmt_arg, "--", &first, &step, &last],
                    stdin: None,
                    compare_stderr: true,
                    locale: "C",
                    env: &[],
                },
            );
        }
    }
}
