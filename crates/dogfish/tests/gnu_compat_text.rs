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
        ];
        common::write_files(&dir, files);
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
}
