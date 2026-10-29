//! Differential GNU-compatibility suite for the listing tools: `ls`, `du`,
//! `df` and `stat`.
//!
//! Cases run with stdout piped (not a terminal), where Dogfish must print
//! exactly what GNU coreutils prints. The fixture tree pins every timestamp
//! that the output shows (with `filetime`), so long listings are stable:
//!
//! * Change and birth times cannot be set, but they do not move between the
//!   Dogfish and GNU runs of a case, so they compare byte-for-byte too.
//! * Directory access times can move while the suite runs (other cases read
//!   directories), so cases never print them, or mask them ([`check_masked`]).
//! * `df` and `stat -f` show live free-space counters. Exact cases use only
//!   columns that cannot change (source, type, size, inode total, target...);
//!   full default layouts go through [`check_volatile`], which retries until
//!   both runs agree and otherwise compares with the moving figures masked.

// Case names mirror the flags they test (`ls_S` vs `ls_s`), where case matters.
#![allow(non_snake_case)]

mod common;

use std::ffi::{CString, OsStr};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{dogfish_bin, exec, gnu_tool};
use filetime::FileTime;

/// 2020-03-04 05:06:07 UTC: "old" (more than six months ago) for `ls -l`.
const OLD: i64 = 1_583_298_367;
/// 2038-01-19 03:14:08 UTC: in the future, which `ls -l` also shows with a year.
const FUTURE: i64 = 2_147_483_648;

fn fixtures() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = common::fresh_fixture_dir();
        build_fixture(&dir);
        dir
    })
}

fn build_fixture(dir: &Path) {
    let files: &[(&str, &[u8])] = &[
        ("a.txt", b"hello\n"),
        ("empty", b""),
        ("exec.sh", b"#!/bin/sh\necho hi\n"),
        (".hidden", b"secret\n"),
        ("backup~", b"old\n"),
        ("recent.txt", b"new\n"),
        ("future.txt", b"later\n"),
        ("big.bin", &[7; 70_000]),
        ("setuid", b""),
        ("setgid", b""),
        ("noperm", b"x"),
        ("private", b"p"),
        ("dir/one", b"1"),
        ("dir/.dot", b"."),
        ("dir/notes.md", b"# notes\n"),
        ("dir/sub/three", &[b'3'; 300]),
        ("dir/sub/deep/big", &[0; 9000]),
        ("times/t1", b"1"),
        ("times/t2", b"22"),
        ("times/t3", b"333"),
        ("times/t4", b"4444"),
        ("names/a.txt", b""),
        ("names/b.TXT", b""),
        ("names/c.tar.gz", b""),
        ("names/e.c", b""),
        ("names/with space", b""),
        ("names/it's", b""),
        ("names/semi;colon", b""),
        ("names/star*", b""),
        ("names/q?mark", b""),
        ("names/$dollar", b""),
        ("names/-dash", b""),
        ("names/~tilde", b""),
        ("names/#hash", b""),
        ("names/back\\slash", b""),
        ("names/brace{", b""),
        ("names/{", b""),
        ("names/dq\"x", b""),
        ("names/\u{fc}n\u{ef}", b""),
        ("names/tab\tin", b""),
        ("names/new\nline", b""),
        ("vers/file1", b""),
        ("vers/file10", b""),
        ("vers/file2", b""),
        ("vers/file1.10", b""),
        ("vers/file1.9", b""),
        ("vers/file-1.0", b""),
        ("vers/file-1.0~rc1", b""),
        ("vers/a.1", b""),
        ("vers/a.10", b""),
        ("vers/a.2", b""),
        ("vers/.a.b", b""),
        ("vers/007", b""),
        ("vers/7", b""),
        ("vers/x.tar.gz", b""),
        ("vers/x", b""),
    ];
    common::write_files(dir, files);
    let p = |rel: &str| dir.join(rel);

    // A name that is not valid UTF-8. Some filesystems (APFS) refuse such names with
    // EILSEQ; cases only run against GNU coreutils on Linux, so skip it there.
    match fs::write(dir.join(OsStr::from_bytes(b"names/bad\xffbyte")), b"") {
        Err(e) if e.raw_os_error() == Some(libc::EILSEQ) => {}
        res => res.unwrap(),
    }
    fs::create_dir_all(p("empty_dir")).unwrap();
    fs::create_dir_all(p("sticky")).unwrap();
    fs::File::create(p("sparse.img"))
        .unwrap()
        .set_len(5 << 20)
        .unwrap();
    fs::hard_link(p("a.txt"), p("hardlink")).unwrap();
    fs::hard_link(p("dir/one"), p("dir/sub/one.link")).unwrap();
    symlink("a.txt", p("link_file")).unwrap();
    symlink("dir", p("link_dir")).unwrap();
    symlink("missing", p("link_dangling")).unwrap();
    symlink("../a.txt", p("dir/to_a")).unwrap();
    let fifo = CString::new(p("fifo").as_os_str().as_bytes()).unwrap();
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);

    for (rel, mode) in [
        ("exec.sh", 0o755),
        ("setuid", 0o4755),
        ("setgid", 0o2644),
        ("noperm", 0o000),
        ("private", 0o600),
        ("sticky", 0o1777),
    ] {
        fs::set_permissions(p(rel), fs::Permissions::from_mode(mode)).unwrap();
    }

    let at = |secs: i64, nanos: u32| FileTime::from_unix_time(secs, nanos);
    let set = |rel: &str, atime: FileTime, mtime: FileTime| {
        // Path-based utimensat: `set_file_times` opens the file first, which fails
        // on the mode-000 fixture unless the suite runs as root (CI runners don't).
        // None of these entries are symlinks, so not following them is moot.
        filetime::set_symlink_file_times(p(rel), atime, mtime).unwrap();
    };
    for rel in [
        "empty",
        "exec.sh",
        ".hidden",
        "backup~",
        "big.bin",
        "setuid",
        "setgid",
        "noperm",
        "private",
        "sparse.img",
        "dir/.dot",
        "dir/notes.md",
        "dir/sub/three",
        "dir/sub/deep/big",
    ] {
        set(rel, at(OLD + 60, 0), at(OLD, 0));
    }
    // a.txt and its hard link share one inode; dir/one likewise.
    set("a.txt", at(OLD + 100, 5), at(OLD, 123_456_789));
    set("dir/one", at(OLD + 7, 0), at(OLD - 86_400 * 400, 0));
    let three_days_ago = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        - 3 * 86_400;
    set("recent.txt", at(three_days_ago, 0), at(three_days_ago, 500));
    set("future.txt", at(FUTURE, 0), at(FUTURE, 0));
    // -t/-u ordering, including an mtime tie (broken by name).
    set("times/t1", at(OLD + 40, 0), at(OLD + 10, 0));
    set("times/t2", at(OLD + 30, 0), at(OLD + 20, 0));
    set("times/t3", at(OLD + 20, 0), at(OLD + 30, 0));
    set("times/t4", at(OLD + 10, 0), at(OLD + 30, 0));
    for rel in ["names/a.txt", "names/b.TXT", "names/with space"] {
        set(rel, at(OLD, 0), at(OLD, 0));
    }
    // The `symlink` variant uses utimensat(AT_SYMLINK_NOFOLLOW): it neither
    // follows links nor opens the FIFO (which would block).
    for rel in ["link_file", "link_dir", "link_dangling", "dir/to_a", "fifo"] {
        filetime::set_symlink_file_times(p(rel), at(OLD - 3600, 0), at(OLD - 3600, 0)).unwrap();
    }
    // Directories last (creating entries bumps their mtime), deepest first.
    for rel in [
        "dir/sub/deep",
        "dir/sub",
        "dir",
        "empty_dir",
        "sticky",
        "times",
        "names",
        "vers",
    ] {
        set(rel, at(OLD, 0), at(OLD + 3600, 0));
    }
    build_color_fixture(&dir.join("colors"));
}

/// `colors/`: what `ls --color` tells apart beyond the main tree — directory
/// permission colours, links to each kind of file (and a link loop), hard links,
/// suffixes in several cases, and a name long enough to wrap.
fn build_color_fixture(dir: &Path) {
    let long_name = format!("long_{}.txt", "n".repeat(90));
    let files: &[(&str, &[u8])] = &[
        ("plain", b"p"),
        ("run.sh", b"#!/bin/sh\n"),
        ("photo.JPG", b""),
        ("photo.jpg", b""),
        ("Photo.Jpg", b""),
        ("archive.TAR.GZ", b""),
        ("archive.tar.gz", b""),
        ("notes.md", b""),
        ("README", b""),
        ("multi1", b"m"),
        ("ow_dir/inner", b""),
        ("st_dir/inner", b""),
        ("tw_dir/inner", b""),
        (&long_name, b""),
    ];
    common::write_files(dir, files);
    let p = |rel: &str| dir.join(rel);
    fs::hard_link(p("multi1"), p("multi2")).unwrap();
    for (link, target) in [
        ("to_ow", "ow_dir"),
        ("to_plain", "plain"),
        ("to_run", "run.sh"),
        ("to_md", "notes.md"),
        ("to_setuid", "../setuid"),
        ("to_fifo", "../fifo"),
        ("to_nowhere", "nowhere.txt"),
        ("loop", "loop"),
        ("to_link", "to_plain"),
    ] {
        symlink(target, p(link)).unwrap();
    }
    for (rel, mode) in [
        ("run.sh", 0o755),
        ("ow_dir", 0o777),
        ("st_dir", 0o1755),
        ("tw_dir", 0o1777),
    ] {
        fs::set_permissions(p(rel), fs::Permissions::from_mode(mode)).unwrap();
    }
    let old = FileTime::from_unix_time(OLD, 0);
    for rel in files.iter().map(|(name, _)| *name).chain(["multi2"]) {
        filetime::set_symlink_file_times(p(rel), old, old).unwrap();
    }
    for rel in ["ow_dir", "st_dir", "tw_dir", "."] {
        filetime::set_symlink_file_times(p(rel), old, old).unwrap();
    }
}

gnu_cases! {
    // ------------------------------------------------------------- ls basic
    ls_default: "ls" [];
    ls_all: "ls" ["-a"];
    ls_almost_all: "ls" ["-A"];
    ls_one_per_line: "ls" ["-1"];
    ls_dir_operand: "ls" ["dir"];
    ls_two_dirs: "ls" ["dir", "empty_dir"];
    ls_files_then_dirs: "ls" ["dir", "a.txt", "empty_dir", "exec.sh"];
    ls_missing_operand_among_others: "ls" ["a.txt", "nonexist", "dir"];
    ls_missing_only: "ls" ["nonexist"];
    ls_missing_two: "ls" ["nonexist1", "nonexist2"];
    ls_empty_dir: "ls" ["-la", "empty_dir"];
    ls_dash_name: "ls" ["names/-dash"];
    ls_double_dash: "ls" ["--", "names/-dash", "a.txt"];
    ls_options_after_operands: "ls" ["dir", "-a"];
    ls_directory: "ls" ["-d", "dir", "empty_dir", "."];
    ls_directory_dot: "ls" ["-d"];
    ls_unsorted_all: "ls" ["-f", "vers"];
    ls_unsorted: "ls" ["-U", "-a", "vers"];
    ls_unsorted_reverse_is_unsorted: "ls" ["-Ur", "vers"];

    // ------------------------------------------------------------ ls -l
    ls_long: "ls" ["-l"];
    ls_long_all: "ls" ["-la"];
    ls_long_almost_all: "ls" ["-lA"];
    ls_long_dir: "ls" ["-l", "dir"];
    ls_long_files_and_dir_share_widths: "ls" ["-l", "a.txt", "dir"];
    ls_long_directory: "ls" ["-ld", "dir", "sticky", "empty_dir"];
    ls_long_modes: "ls" ["-ld", "setuid", "setgid", "noperm", "private", "sticky", "fifo", "exec.sh"];
    ls_long_devices: "ls" ["-l", "/dev/null", "/dev/zero"];
    ls_long_recent_old_future: "ls" ["-l", "a.txt", "recent.txt", "future.txt"];
    ls_long_hard_links: "ls" ["-l", "a.txt", "hardlink", "dir/one", "dir/sub/one.link"];
    ls_long_symlinks: "ls" ["-l", "link_file", "link_dir", "link_dangling"];
    ls_long_numeric: "ls" ["-n", "dir"];
    ls_long_no_owner: "ls" ["-g", "dir"];
    ls_long_no_group: "ls" ["-o", "dir"];
    ls_long_no_owner_no_group: "ls" ["-go", "dir"];
    ls_long_G: "ls" ["-lG", "dir"];
    ls_long_author: "ls" ["-l", "--author", "dir"];
    ls_long_inode: "ls" ["-li", "dir"];
    ls_long_size: "ls" ["-ls"];
    ls_long_size_human: "ls" ["-lsh"];
    ls_long_human: "ls" ["-lh"];
    ls_long_si: "ls" ["-l", "--si"];
    ls_long_size_k: "ls" ["-lsk", "dir"];
    ls_long_block_size_K: "ls" ["-l", "--block-size=K"];
    ls_long_block_size_KB: "ls" ["-l", "--block-size=KB", "sparse.img", "big.bin"];
    ls_long_block_size_MiB: "ls" ["-l", "--block-size=MiB", "sparse.img"];
    ls_long_block_size_1000: "ls" ["-ls", "--block-size=1000", "sparse.img", "big.bin"];
    ls_long_format_word: "ls" ["--format=long", "dir"];
    ls_long_format_verbose: "ls" ["--format=verbose", "dir"];
    ls_long_recursive: "ls" ["-lR", "dir"];
    ls_long_classify: "ls" ["-lF"];
    ls_long_deref: "ls" ["-lL", "dir"];
    ls_long_deref_cwd: "ls" ["-lL"];
    ls_long_dereference_command_line: "ls" ["-lH", "link_dir"];
    ls_long_symlink_operand: "ls" ["-l", "link_dir"];
    ls_long_context: "ls" ["-lZ", "dir"];
    ls_long_zero: "ls" ["-l", "--zero", "dir"];
    ls_unsorted_long: "ls" ["-fl", "dir/sub"];

    // ------------------------------------------------------ ls time styles
    ls_full_time: "ls" ["--full-time", "dir"];
    ls_time_style_full_iso: "ls" ["-l", "--time-style=full-iso", "dir"];
    ls_time_style_long_iso: "ls" ["-l", "--time-style=long-iso", "dir"];
    ls_time_style_iso: "ls" ["-l", "--time-style=iso", "a.txt", "recent.txt", "future.txt"];
    ls_time_style_locale: "ls" ["-l", "--time-style=locale", "dir"];
    ls_time_style_posix: "ls" ["-l", "--time-style=posix-long-iso", "dir"];
    ls_time_style_format: "ls" ["-l", "--time-style=+%Y-%m-%d %H:%M:%S.%3N %Z|%s|%j|%a %A %B", "dir"];
    ls_time_style_two_formats: "ls" ["-l", "--time-style=+old %Y\nnew %b %e", "a.txt", "recent.txt"];
    ls_time_style_padding: "ls" ["-l", "--time-style=+%-d|%_m|%e|%k|%l|%I %p|%^b|%y %C|%u %w|%U %W %V %G %g", "a.txt"];
    ls_time_style_bad: "ls" ["-l", "--time-style=bogus"];
    ls_time_style_three_formats: "ls" ["-l", "--time-style=+a\nb\nc"];
    ls_time_atime_long: "ls" ["-lu", "times"];
    ls_time_ctime_long: "ls" ["-lc", "times"];
    ls_time_word: "ls" ["-l", "--time=atime", "times"];
    ls_time_word_bad: "ls" ["--time=foo"];

    // -------------------------------------------------------------- ls sort
    ls_sort_size: "ls" ["-S"];
    ls_sort_size_long: "ls" ["-lS"];
    ls_sort_size_reverse: "ls" ["-Sr"];
    ls_sort_word_size: "ls" ["--sort=size", "-r"];
    ls_sort_time: "ls" ["-t", "times"];
    ls_sort_time_long: "ls" ["-lt", "times"];
    ls_sort_time_reverse: "ls" ["-ltr", "times"];
    ls_sort_atime: "ls" ["-tu", "times"];
    ls_sort_u_alone: "ls" ["-u", "times"];
    ls_sort_ctime_long: "ls" ["-lct", "times"];
    ls_sort_time_word_mtime: "ls" ["--time=mtime", "times"];
    ls_sort_reverse: "ls" ["-r"];
    ls_sort_extension: "ls" ["-X", "names"];
    ls_sort_extension_vers: "ls" ["--sort=extension", "vers"];
    ls_sort_version: "ls" ["-v", "vers"];
    ls_sort_version_all_reverse: "ls" ["-var", "vers"];
    ls_sort_width: "ls" ["--sort=width", "names"];
    ls_sort_none_word: "ls" ["--sort=none", "-a", "vers"];
    ls_sort_bad: "ls" ["--sort=foo"];
    ls_sort_ambiguous: "ls" ["--sort=t"];
    ls_group_directories_first: "ls" ["--group-directories-first"];
    ls_group_directories_first_long: "ls" ["--group-directories-first", "-l"];
    ls_group_directories_first_classify: "ls" ["--group-directories-first", "-F"];

    // ----------------------------------------------------------- ls formats
    ls_columns: "ls" ["-C"];
    ls_across: "ls" ["-x"];
    ls_commas: "ls" ["-m"];
    ls_columns_narrow: "ls" ["-C", "-w", "40"];
    ls_across_narrow: "ls" ["-x", "-w", "30"];
    ls_commas_narrow: "ls" ["-m", "-w", "20"];
    ls_columns_unlimited: "ls" ["-C", "-w", "0", "vers"];
    ls_columns_tabsize: "ls" ["-C", "-T", "4", "-w", "40"];
    ls_columns_no_tabs: "ls" ["-x", "-T", "0", "-w", "50", "vers"];
    ls_columns_names: "ls" ["-C", "-w", "50", "names", "vers"];
    ls_columns_inode_size: "ls" ["-Cis", "-w", "60", "dir"];
    ls_format_commas_word: "ls" ["--format=commas", "vers"];
    ls_format_across_word: "ls" ["--format=across", "vers"];
    ls_format_vertical_word: "ls" ["--format=vertical", "vers"];
    ls_format_single_column: "ls" ["--format=single-column", "vers"];
    ls_format_bad: "ls" ["--format=x"];
    ls_last_format_wins: "ls" ["-l", "-1", "dir"];
    ls_zero: "ls" ["--zero", "dir"];
    ls_inode: "ls" ["-i", "dir"];
    ls_inode_one: "ls" ["-1i", "dir"];
    ls_size: "ls" ["-s"];
    ls_size_human: "ls" ["-sh"];
    ls_size_si: "ls" ["--si", "-s"];
    ls_size_k: "ls" ["-k", "-s"];
    ls_size_block_M: "ls" ["-s", "--block-size=M"];
    ls_size_inode: "ls" ["-is", "dir"];
    ls_size_sparse: "ls" ["-ls", "sparse.img", "big.bin"];
    ls_human_beats_k: "ls" ["-s", "-h", "-k", "dir"];
    ls_context: "ls" ["-Z", "dir"];

    // ------------------------------------------------------ ls indicators
    ls_classify: "ls" ["-F"];
    ls_classify_always: "ls" ["--classify=always"];
    ls_classify_never: "ls" ["--classify=never"];
    ls_classify_auto: "ls" ["--classify=auto"];
    ls_slash: "ls" ["-p"];
    ls_file_type: "ls" ["--file-type"];
    ls_indicator_style_slash: "ls" ["--indicator-style=slash"];
    ls_indicator_style_classify: "ls" ["--indicator-style=classify", "dir"];
    ls_indicator_last_wins: "ls" ["-Fp", "dir", "."];

    // ---------------------------------------------------- ls recursion
    ls_recursive: "ls" ["-R", "dir"];
    ls_recursive_all: "ls" ["-Ra", "dir"];
    ls_recursive_cwd: "ls" ["-R", "times", "dir/sub"];
    ls_recursive_trailing_slash: "ls" ["-R", "dir/"];
    ls_recursive_missing: "ls" ["-R", "dir/sub", "nonexist"];
    ls_recursive_deref: "ls" ["-RL", "dir"];

    // ---------------------------------------------- ls symlinks & filters
    ls_symlink_to_dir_operand: "ls" ["link_dir"];
    ls_symlink_to_dir_classify: "ls" ["-F", "link_dir"];
    ls_symlink_to_dir_directory: "ls" ["-d", "link_dir"];
    ls_symlink_dangling_long: "ls" ["-l", "link_dangling", "fifo"];
    ls_symlink_dangling_deref: "ls" ["-L", "link_dangling"];
    ls_deref_names_only: "ls" ["-L"];
    ls_ignore: "ls" ["-I", "*.txt"];
    ls_ignore_long: "ls" ["--ignore=link*"];
    ls_hide: "ls" ["--hide=*.txt"];
    ls_hide_with_all: "ls" ["-a", "--hide=*.txt"];
    ls_hide_with_almost_all: "ls" ["-A", "--hide=*~"];
    ls_ignore_backups: "ls" ["-B"];

    // ----------------------------------------------------- ls quoting
    ls_names_literal_default: "ls" ["names"];
    ls_names_escape: "ls" ["-b", "names"];
    ls_names_quote_name: "ls" ["-Q", "names"];
    ls_names_hide_control: "ls" ["-q", "names"];
    ls_names_literal: "ls" ["-N", "names"];
    ls_names_shell: "ls" ["--quoting-style=shell", "names"];
    ls_names_shell_always: "ls" ["--quoting-style=shell-always", "names"];
    ls_names_shell_escape: "ls" ["--quoting-style=shell-escape", "names"];
    ls_names_shell_escape_always: "ls" ["--quoting-style=shell-escape-always", "names"];
    ls_names_c: "ls" ["--quoting-style=c", "names"];
    ls_names_escape_word: "ls" ["--quoting-style=escape", "names"];
    ls_names_locale: "ls" ["--quoting-style=locale", "names"];
    ls_names_clocale: "ls" ["--quoting-style=clocale", "names"];
    ls_names_c_maybe: "ls" ["--quoting-style=c-maybe", "names"];
    ls_names_long_c_maybe: "ls" ["-l", "--quoting-style=c-maybe", "names"];
    ls_names_long_shell_escape: "ls" ["-l", "--quoting-style=shell-escape", "names"];
    ls_names_columns_shell: "ls" ["-C", "--quoting-style=shell", "names"];
    ls_names_bad_style: "ls" ["--quoting-style=nope", "names"];

    // ------------------------------------------------------ ls usage errors
    ls_invalid_option: "ls" ["-z"];
    ls_unrecognized_option: "ls" ["--foo"];
    ls_ambiguous_option: "ls" ["--s"];
    ls_option_with_argument: "ls" ["--all=yes"];
    ls_missing_argument: "ls" ["-w"];
    ls_missing_long_argument: "ls" ["--width"];
    ls_bad_width: "ls" ["-w", "x"];
    ls_bad_tabsize: "ls" ["-T", "x"];
    ls_bad_block_size: "ls" ["--block-size=x"];
    ls_bad_block_size_suffix: "ls" ["--block-size=1x"];

    // ------------------------------------------------------------------- du
    du_default: "du" [];
    du_all: "du" ["-a"];
    du_dir: "du" ["dir"];
    du_all_dir: "du" ["-a", "dir"];
    du_human: "du" ["-h", "dir"];
    du_human_all: "du" ["-ah", "dir"];
    du_si: "du" ["--si", "-a", "dir"];
    du_summarize: "du" ["-s"];
    du_summarize_many: "du" ["-sh", "dir", "names", "vers"];
    du_total: "du" ["-c", "dir", "names"];
    du_total_human: "du" ["-hc", "dir/sub", "sparse.img"];
    du_separate_dirs: "du" ["-S", "dir"];
    du_separate_dirs_all: "du" ["-Sa", "dir"];
    du_separate_dirs_total: "du" ["-Sc", "dir"];
    du_kilobytes: "du" ["-k", "dir"];
    du_megabytes: "du" ["-m", "dir"];
    du_bytes: "du" ["-b", "dir"];
    du_bytes_all: "du" ["-ab", "dir"];
    du_apparent: "du" ["--apparent-size", "-a", "dir"];
    du_apparent_human: "du" ["--apparent-size", "-sh", "."];
    du_block_size_4K: "du" ["-B", "4K", "dir"];
    du_block_size_4KB: "du" ["-B4KB", "dir"];
    du_block_size_MiB: "du" ["-BMiB", "-s", "dir"];
    du_block_size_K: "du" ["-BK", "-s", "dir"];
    du_block_size_1K: "du" ["--block-size=1K", "-s", "dir"];
    du_max_depth: "du" ["-d", "1"];
    du_max_depth_zero: "du" ["--max-depth=0", "dir"];
    du_max_depth_all: "du" ["-d", "2", "-a", "dir"];
    du_hard_links_once: "du" ["a.txt", "hardlink"];
    du_hard_links_order: "du" ["hardlink", "a.txt"];
    du_hard_links_in_tree: "du" ["-a", "dir"];
    du_count_links: "du" ["-al", "dir"];
    du_count_links_summary: "du" ["-ls", "dir", "."];
    du_duplicate_operands: "du" ["dir", "dir/sub"];
    du_duplicate_operands_reverse: "du" ["dir/sub", "dir"];
    du_same_operand_twice: "du" ["-s", "dir", "dir"];
    du_trailing_slash: "du" ["dir/"];
    du_double_trailing_slash: "du" ["-a", "dir//"];
    du_null: "du" ["-0", "dir"];
    du_inodes: "du" ["--inodes"];
    du_inodes_all: "du" ["--inodes", "-a", "dir"];
    du_inodes_summary: "du" ["--inodes", "-s", "dir"];
    du_inodes_apparent: "du" ["--inodes", "-b", "dir"];
    du_threshold: "du" ["-t", "8K"];
    du_threshold_negative: "du" ["-a", "-t", "-1K", "dir"];
    du_exclude: "du" ["-a", "--exclude=*.txt"];
    du_exclude_dir: "du" ["--exclude=sub", "dir"];
    du_exclude_path: "du" ["-a", "--exclude=dir/sub/*", "dir"];
    du_one_file_system: "du" ["-x", "dir"];
    du_dereference: "du" ["-L", "-a", "dir"];
    du_dereference_args: "du" ["-D", "link_dir"];
    du_dereference_args_H: "du" ["-H", "link_dir"];
    du_symlink_operand: "du" ["link_dir"];
    du_dangling_dereferenced: "du" ["-L", "link_dangling"];
    du_dangling: "du" ["link_dangling"];
    du_sparse: "du" ["sparse.img", "big.bin"];
    du_sparse_apparent: "du" ["--apparent-size", "-h", "sparse.img", "big.bin"];
    du_human_then_k: "du" ["-h", "-k", "dir"];
    du_k_then_human: "du" ["-k", "-h", "dir"];
    du_missing: "du" ["nonexist", "dir"];
    du_invalid_option: "du" ["-z"];
    du_bad_max_depth: "du" ["--max-depth=x"];
    du_summarize_all: "du" ["-sa"];
    du_summarize_depth: "du" ["-s", "-d", "1"];
    du_summarize_depth_zero: "du" ["-s", "-d", "0", "dir"];
    du_bad_block_size: "du" ["-B", "x"];
    du_bad_block_suffix: "du" ["-B", "1x"];
    du_zero_block_size: "du" ["--block-size=0"];
    du_huge_block_size: "du" ["-B", "99999999999999999999999"];
    du_bad_threshold: "du" ["-t", "x"];
    du_negative_zero_threshold: "du" ["--threshold=-0"];

    // ---------------------------------- df (only columns that cannot move)
    df_output_stable: "df" ["--output=source,fstype,itotal,size,file,target", "a.txt"];
    df_output_human: "df" ["--output=size,itotal", "-h", "a.txt"];
    df_output_si: "df" ["--output=size,itotal", "--si", "a.txt"];
    df_output_block_M: "df" ["--output=size", "-BM", "a.txt"];
    df_output_block_1000: "df" ["--output=size", "-B1000", "a.txt"];
    df_output_block_512: "df" ["--output=size,itotal", "-B512", "a.txt"];
    df_output_block_KB: "df" ["--output=size", "-BKB", "a.txt"];
    df_output_block_3000: "df" ["--output=size", "-B", "3000", "a.txt"];
    df_output_k: "df" ["--output=size", "-k", "a.txt"];
    df_output_m: "df" ["--output=size", "-m", "a.txt"];
    df_output_order: "df" ["--output=target,file,source", "a.txt", "dir"];
    df_output_total: "df" ["--total", "--output=source,size,itotal,target", "a.txt", "dir"];
    df_output_symlink_operand: "df" ["--output=source,target,file", "link_dir"];
    df_output_device_file: "df" ["--output=source,file", "/dev/null"];
    // Distros disagree on which file systems are "dummy" (Ubuntu patches coreutils to
    // hide devtmpfs and squashfs snaps), so whole-table cases exclude those types
    // explicitly; `-a` cases show everything and need no exclusion.
    df_all_mounts_stable: "df" ["--output=source,fstype,target", "-x", "squashfs", "-x", "devtmpfs"];
    df_dangling: "df" ["link_dangling"];
    df_missing: "df" ["nonexist"];
    df_missing_and_file: "df" ["--output=source,target", "nonexist", "a.txt"];
    df_type_none_selected: "df" ["-t", "nosuchfs", "a.txt"];
    df_type_excluded_other: "df" ["-x", "nosuchfs", "--output=source,target", "a.txt"];
    df_type_conflict: "df" ["-t", "x", "-x", "x"];
    df_output_unknown_field: "df" ["--output=foo"];
    df_output_field_twice: "df" ["--output=size,size"];
    df_output_with_inodes: "df" ["-i", "--output"];
    df_output_with_posix: "df" ["--output", "-P"];
    df_output_with_type: "df" ["-T", "--output=source"];
    df_bad_block_size: "df" ["-B", "x", "a.txt"];
    df_zero_block_size: "df" ["-B", "0", "a.txt"];
    df_invalid_option: "df" ["-z"];

    // ----------------------------------------------------------------- stat
    stat_default_file: "stat" ["a.txt"];
    stat_default_many: "stat" ["a.txt", "empty", "big.bin", "sparse.img", "exec.sh"];
    stat_default_symlink: "stat" ["link_file", "link_dangling"];
    stat_default_deref: "stat" ["-L", "link_file"];
    stat_default_fifo: "stat" ["fifo"];
    stat_default_modes: "stat" ["setuid", "setgid", "noperm", "private"];
    stat_terse: "stat" ["-t", "a.txt", "link_file", "fifo", "sparse.img"];
    stat_terse_deref: "stat" ["--terse", "--dereference", "link_file"];
    stat_format_all: "stat" ["-c", "%n|%s|%b|%B|%f|%F|%a|%A|%u|%U|%g|%G|%h|%i|%o|%d|%D|%t|%T|%Hd|%Ld", "a.txt", "dir", "link_file", "fifo", "empty"];
    stat_format_quoted_names: "stat" ["-c", "%N", "link_file", "link_dangling", "dir/to_a", "a.txt"];
    stat_format_quoted_weird_names: "stat" ["-c", "%N", "names/with space", "names/it's", "names/tab\tin", "names/new\nline", "names/dq\"x", "names/$dollar", "names/\u{fc}n\u{ef}", "names/back\\slash", "names/{", "names/~tilde"];
    stat_format_times: "stat" ["-c", "%x|%X|%y|%Y", "a.txt", "recent.txt", "future.txt", "link_file"];
    stat_format_time_precision: "stat" ["-c", "%.3Y|%.Y|%.0Y|%.9Y|%.1Y|%.12Y|%10.3Y|%-20.5Y|%020.5Y|%+Y|% Y|%-15Y|", "a.txt"];
    stat_format_widths: "stat" ["-c", "%5s|%-5s|%05s|%+5s|%.3s|%10n|%-10n|%.3n|%10.3n|%5h|%05h|%x", "a.txt"];
    stat_format_alternate: "stat" ["-c", "%#a|%#f|%#D|%08a|%-6a|%#o", "setuid", "a.txt"];
    stat_format_sizes: "stat" ["-c", "%b %B %o %s", "sparse.img", "big.bin", "empty"];
    stat_format_mount_point: "stat" ["-c", "%m", "a.txt", "dir", "/dev/null", "/"];
    stat_format_devices: "stat" ["-c", "%n %F %t %T %Hr %Lr %r %R", "/dev/null", "/dev/zero"];
    stat_format_types: "stat" ["-c", "%F", "/dev/null", "fifo", "empty_dir", "empty", "a.txt", "link_file"];
    stat_format_stdin: "stat" ["-c", "%n %F", "-"];
    stat_format_deref: "stat" ["-L", "-c", "%n %F %s", "link_file", "link_dir"];
    stat_format_empty: "stat" ["-c", "", "a.txt"];
    stat_format_percent: "stat" ["-c", "%%%n%%", "a.txt"];
    stat_format_unknown_directive: "stat" ["-c", "%Q%j|%H|%L|%Hq", "a.txt"];
    stat_format_invalid_directive: "stat" ["-c", "%5%", "a.txt"];
    stat_format_trailing_percent: "stat" ["-c", "x%", "a.txt"];
    stat_format_dangling_flags: "stat" ["-c", "%n %-", "a.txt"];
    stat_printf: "stat" ["--printf=%n:%s\n", "a.txt", "big.bin"];
    stat_printf_escapes: "stat" ["--printf=\\a\\b\\e\\f\\n\\r\\t\\v\\\"\\\\|\\101\\x42\\x4a|\\0101|\\q|%%|\\n", "a.txt"];
    stat_printf_bad_hex: "stat" ["--printf=\\x|\\xg\\n", "a.txt"];
    stat_printf_trailing_backslash: "stat" ["--printf=%n\\", "a.txt"];
    stat_format_then_printf: "stat" ["--format=%n", "--printf=%s\n", "a.txt"];
    stat_printf_then_format: "stat" ["--printf=%n", "--format=%s", "a.txt"];
    stat_fs_format: "stat" ["-f", "-c", "%n %i %l %t %T %s %S %b %c", "a.txt", "dir"];
    stat_fs_deref: "stat" ["-f", "-L", "-c", "%n %T", "link_dir"];
    stat_fs_missing: "stat" ["-f", "nonexist"];
    stat_missing: "stat" ["nonexist", "a.txt"];
    stat_dangling_deref: "stat" ["-L", "link_dangling"];
    stat_missing_operand: "stat" [];
    stat_missing_format: "stat" ["-c"];
    stat_cached: "stat" ["--cached=never", "-c", "%n", "a.txt"];
    stat_cached_bad: "stat" ["--cached=bogus", "a.txt"];
    stat_invalid_option: "stat" ["-z", "a.txt"];

    // ------------------------------------------------------------ ls --color
    // Every case pins the colour environment: an empty LS_COLORS means GNU's
    // built-in table, which GNU uses only for a known TERM (or a COLORTERM).
    ls_color_default_cwd: "ls" ["--color=always"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_default_all: "ls" ["--color=always", "-a"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_default_colors_dir: "ls" ["--color=always", "colors"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_default_bare_flag: "ls" ["--color", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_default_yes: "ls" ["--color=yes", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_default_force: "ls" ["--color=force", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_default_prefix: "ls" ["--color=al", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_default_abbreviated_option: "ls" ["--colo=always", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_auto_not_tty: "ls" ["--color=auto", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_tty_not_tty: "ls" ["--color=tty", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_if_tty_not_tty: "ls" ["--color=if-tty", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_never: "ls" ["--color=never", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_none: "ls" ["--color=none", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_last_wins: "ls" ["--color=always", "--color=no", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_last_wins_always: "ls" ["--color=never", "--color=always", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_invalid: "ls" ["--color=x"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_empty_ambiguous: "ls" ["--color="] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_ambiguous_prefix: "ls" ["--color=n", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_hyperlink_invalid: "ls" ["--hyperlink=x", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_unsorted_after_disables: "ls" ["--color=always", "-f", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_unsorted_before_keeps: "ls" ["-f", "--color=always", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_zero_disables: "ls" ["--color=always", "--zero", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_zero_then_color: "ls" ["--zero", "--color=always", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_devices: "ls" ["--color=always", "/dev/null", "/dev/zero"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_devices_long: "ls" ["--color=always", "-l", "/dev/null"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_missing_operand: "ls" ["--color=always", "nonexist", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_dir_operands: "ls" ["--color=always", "-d", "dir", ".", "..", "link_dir", "sticky", "colors/ow_dir", "colors/st_dir", "colors/tw_dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_symlink_dir_operand: "ls" ["--color=always", "link_dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_symlink_operands: "ls" ["--color=always", "colors/to_ow", "colors/to_run", "colors/loop", "colors/to_nowhere"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_recursive: "ls" ["--color=always", "-R", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_recursive_long: "ls" ["--color=always", "-lR", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_sorted_size: "ls" ["--color=always", "-S"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_sorted_time_reverse: "ls" ["--color=always", "-tr", "times"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_group_directories_first: "ls" ["--color=always", "--group-directories-first", "colors"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_group_directories_first_long: "ls" ["--color=always", "-l", "--group-directories-first", "colors"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_dereference: "ls" ["--color=always", "-L", "colors"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_dereference_long: "ls" ["--color=always", "-lL", "colors"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_long: "ls" ["--color=always", "-l"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_long_colors: "ls" ["--color=always", "-l", "colors"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_long_symlinks: "ls" ["--color=always", "-l", "link_dir", "link_dangling", "link_file", "dir/to_a"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_long_inode_size: "ls" ["--color=always", "-lis", "colors"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_long_narrow_clears_line: "ls" ["--color=always", "-l", "-w", "70", "colors"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_long_unlimited: "ls" ["--color=always", "-l", "-w", "0", "colors"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_long_quoted: "ls" ["--color=always", "-l", "--quoting-style=shell-escape", "colors", "names"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_long_quote_name: "ls" ["--color=always", "-lQ", "colors"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_classify: "ls" ["--color=always", "-F"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_classify_colors: "ls" ["--color=always", "-F", "colors"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_classify_long: "ls" ["--color=always", "-lF", "colors"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_slash: "ls" ["--color=always", "-p"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_slash_long: "ls" ["--color=always", "-lp", "colors"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_file_type: "ls" ["--color=always", "--file-type", "colors"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_one_per_line_long_name: "ls" ["--color=always", "-1", "colors"] env [("LS_COLORS", "*.txt=31"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_one_per_line_columns: "ls" ["--color=always", "-1", "colors"] env [("LS_COLORS", "*.txt=31"), ("TERM", "xterm"), ("COLORTERM", ""), ("COLUMNS", "101")];
    ls_color_one_per_line_narrow: "ls" ["--color=always", "-1", "-w", "3", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_columns: "ls" ["--color=always", "-C"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_columns_env_width: "ls" ["--color=always", "-C"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", ""), ("COLUMNS", "40")];
    ls_color_columns_no_tabs: "ls" ["--color=always", "-C", "-T", "4", "-w", "40", "vers"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_columns_narrow: "ls" ["--color=always", "-C", "-w", "10", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_columns_wide_names: "ls" ["--color=always", "-C", "-w", "200", "colors"] env [("LS_COLORS", "*.txt=31"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_columns_inode: "ls" ["--color=always", "-Ci", "-w", "60", "colors"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_columns_unlimited: "ls" ["--color=always", "-C", "-w", "0", "vers"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_across: "ls" ["--color=always", "-x", "colors"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_across_env_width: "ls" ["--color=always", "-x"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", ""), ("COLUMNS", "40")];
    ls_color_commas: "ls" ["--color=always", "-m", "colors"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_commas_narrow: "ls" ["--color=always", "-m", "-w", "12", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_commas_long_name: "ls" ["--color=always", "-m", "colors"] env [("LS_COLORS", "*.txt=31"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_commas_size: "ls" ["--color=always", "-sm", "colors"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_columns_env_invalid: "ls" ["--color=always", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", ""), ("COLUMNS", "abc")];
    ls_color_columns_env_invalid_grid: "ls" ["-C", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", ""), ("COLUMNS", "abc")];
    ls_color_columns_env_zero: "ls" ["--color=always", "-C", "vers"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", ""), ("COLUMNS", "0")];
    ls_color_columns_env_hex: "ls" ["-C", "vers"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", ""), ("COLUMNS", "0x28")];
    ls_color_quote_name: "ls" ["--color=always", "-Q", "names"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_escape: "ls" ["--color=always", "-b", "names"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_hide_control: "ls" ["--color=always", "-q", "names"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_shell: "ls" ["--color=always", "--quoting-style=shell", "names"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_shell_columns: "ls" ["--color=always", "-C", "--quoting-style=shell", "names"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_c_commas: "ls" ["--color=always", "-m", "--quoting-style=c", "names"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_term_dumb: "ls" ["--color=always", "dir"] env [("LS_COLORS", ""), ("TERM", "dumb"), ("COLORTERM", "")];
    ls_color_term_unset_colorterm: "ls" ["--color=always", "dir"] env [("LS_COLORS", ""), ("TERM", ""), ("COLORTERM", "truecolor")];
    ls_color_term_256color: "ls" ["--color=always", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm-256color"), ("COLORTERM", "")];
    ls_color_term_screen: "ls" ["--color=always", "dir"] env [("LS_COLORS", ""), ("TERM", "screen.xterm-256color"), ("COLORTERM", "")];
    ls_color_term_console_size: "ls" ["--color=always", "dir"] env [("LS_COLORS", ""), ("TERM", "con80x25"), ("COLORTERM", "")];
    ls_color_term_vt102: "ls" ["--color=always", "dir"] env [("LS_COLORS", ""), ("TERM", "vt102"), ("COLORTERM", "")];
    ls_color_term_dumb_ls_colors: "ls" ["--color=always", "dir"] env [("LS_COLORS", "di=35"), ("TERM", "dumb"), ("COLORTERM", "")];
    ls_color_no_color_env_ignored: "ls" ["--color=always", "dir"] env [("LS_COLORS", ""), ("TERM", "xterm"), ("COLORTERM", ""), ("NO_COLOR", "1")];
    ls_color_custom_types: "ls" ["--color=always"] env [("LS_COLORS", "di=35:ln=33:ex=4:pi=1:so=2:bd=3:cd=4:do=5"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_custom_devices: "ls" ["--color=always", "fifo", "/dev/null"] env [("LS_COLORS", "pi=1:cd=4"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_unrecognized_key: "ls" ["--color=always", "dir"] env [("LS_COLORS", "di=1:xx=3"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_missing_equals: "ls" ["--color=always", "dir"] env [("LS_COLORS", "di"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_one_letter: "ls" ["--color=always", "dir"] env [("LS_COLORS", "d"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_suffix_missing_equals: "ls" ["--color=always", "dir"] env [("LS_COLORS", "*.md"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_bad_escape_value: "ls" ["--color=always", "dir"] env [("LS_COLORS", "di=\\"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_bad_caret_value: "ls" ["--color=always", "dir"] env [("LS_COLORS", "di=^1"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_empty_entries: "ls" ["--color=always", "dir"] env [("LS_COLORS", ":::di=31:::"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_escapes: "ls" ["--color=always", "dir"] env [("LS_COLORS", "di=\\x41\\X4a\\101\\0\\a\\b\\e\\f\\n\\r\\t\\v\\?\\_\\:\\=\\q"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_escapes_overflow: "ls" ["--color=always", "dir"] env [("LS_COLORS", "di=\\777\\x1ff"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_caret_escapes: "ls" ["--color=always", "dir"] env [("LS_COLORS", "di=^[x^?^@"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_suffix_case_insensitive: "ls" ["--color=always", "colors"] env [("LS_COLORS", "*.jpg=35"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_suffix_case_split: "ls" ["--color=always", "colors"] env [("LS_COLORS", "*.jpg=35:*.JPG=36"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_suffix_case_same: "ls" ["--color=always", "colors"] env [("LS_COLORS", "*.jpg=35:*.JPG=35"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_suffix_case_three: "ls" ["--color=always", "colors"] env [("LS_COLORS", "*.jpg=35:*.JPG=35:*.Jpg=34"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_suffix_case_three_other: "ls" ["--color=always", "colors"] env [("LS_COLORS", "*.md=31:*.MD=31:*.Md=35:*.JPG=35:*.jpg=35:*.Jpg=34"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_suffix_longest_first: "ls" ["--color=always", "colors"] env [("LS_COLORS", "*.tar.gz=31:*.TAR.GZ=32:*.gz=33"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_suffix_later_wins: "ls" ["--color=always", "names", "vers"] env [("LS_COLORS", "*.tar.gz=32:*.gz=31"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_suffix_redefined: "ls" ["--color=always"] env [("LS_COLORS", "*.txt=31:*.txt=32"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_suffix_whole_name: "ls" ["--color=always", "-l", "colors"] env [("LS_COLORS", "*README=1:*.md=4"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_suffix_any: "ls" ["--color=always", "dir"] env [("LS_COLORS", "*=7"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_suffix_not_on_exec: "ls" ["--color=always", "exec.sh", "colors/run.sh"] env [("LS_COLORS", "*.sh=31"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_suffix_with_exec_off: "ls" ["--color=always", "exec.sh", "a.txt"] env [("LS_COLORS", "*.sh=31:ex=00"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_suffix_link_target: "ls" ["--color=always", "-l", "colors/to_md", "colors/to_plain"] env [("LS_COLORS", "*.md=4"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_file_color: "ls" ["--color=always"] env [("LS_COLORS", "fi=36:*.txt=0"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_normal_long: "ls" ["--color=always", "-l", "colors"] env [("LS_COLORS", "no=7:fi=33"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_normal_across: "ls" ["--color=always", "-x", "colors"] env [("LS_COLORS", "no=7:fi=33"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_normal_inode: "ls" ["--color=always", "-i", "dir"] env [("LS_COLORS", "no=7:fi=32"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_normal_reset_value: "ls" ["--color=always", "dir"] env [("LS_COLORS", "no=00"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_end_and_brackets: "ls" ["--color=always", "dir"] env [("LS_COLORS", "ec=X:lc=<:rc=>"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_brackets_restored_at_end: "ls" ["--color=always", "-l", "dir"] env [("LS_COLORS", "lc=<:rc=>"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_end_code: "ls" ["--color=always", "-l", "colors"] env [("LS_COLORS", "ec=\\e[m"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_empty_reset: "ls" ["--color=always", "colors"] env [("LS_COLORS", "rs=:ec="), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_clear_to_eol_custom: "ls" ["--color=always", "-w", "3", "dir"] env [("LS_COLORS", "cl=CLR"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_clear_to_eol_empty: "ls" ["--color=always", "-w", "3", "dir"] env [("LS_COLORS", "cl="), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_orphan_missing: "ls" ["--color=always", "-l", "link_dir", "link_dangling", "link_file"] env [("LS_COLORS", "or=31:mi=32"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_orphan_missing_colors: "ls" ["--color=always", "colors"] env [("LS_COLORS", "or=40;31;01:mi=05;37;41"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_orphan_missing_long: "ls" ["--color=always", "-l", "colors"] env [("LS_COLORS", "or=40;31;01:mi=05;37;41"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_orphan_missing_classify: "ls" ["--color=always", "-lF", "colors"] env [("LS_COLORS", "or=40;31;01:mi=05;37;41"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_missing_only_long: "ls" ["--color=always", "-l", "colors"] env [("LS_COLORS", "mi=05;37;41"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_missing_only_short: "ls" ["--color=always", "link_dangling", "link_dir", "colors"] env [("LS_COLORS", "mi=32"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_orphan_zero: "ls" ["--color=always", "-l", "link_dangling", "link_dir"] env [("LS_COLORS", "or=0"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_orphan_dereference: "ls" ["--color=always", "-L"] env [("LS_COLORS", "or=31"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_link_target: "ls" ["--color=always", "colors"] env [("LS_COLORS", "ln=target"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_link_target_long: "ls" ["--color=always", "-l", "colors"] env [("LS_COLORS", "ln=target"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_link_target_classify: "ls" ["--color=always", "-F", "colors"] env [("LS_COLORS", "ln=target:or=31"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_link_target_exec_off: "ls" ["--color=always", "colors"] env [("LS_COLORS", "ln=target:ex=00"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_link_target_operands: "ls" ["--color=always", "link_dir", "link_dangling", "link_file"] env [("LS_COLORS", "or=31:mi=32:ln=target"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_multi_hardlink: "ls" ["--color=always", "dir", "dir/sub", "colors"] env [("LS_COLORS", "mh=44"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_multi_hardlink_long: "ls" ["--color=always", "-l", "colors"] env [("LS_COLORS", "mh=44"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_multi_hardlink_unstated: "ls" ["--color=always", ".", "dir/sub"] env [("LS_COLORS", "mh=44:ex=00:su=00:sg=00"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_multi_hardlink_classify: "ls" ["--color=always", "-F", ".", "dir/sub"] env [("LS_COLORS", "mh=44:ex=00:su=00:sg=00"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_setid_off: "ls" ["--color=always", "setuid", "setgid"] env [("LS_COLORS", "su=00:sg=00"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_setid_exec_off: "ls" ["--color=always", "setuid", "setgid"] env [("LS_COLORS", "su=00:sg=00:ex=00"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_dir_bits: "ls" ["--color=always", "-d", "sticky", "colors/ow_dir", "colors/st_dir", "colors/tw_dir"] env [("LS_COLORS", "st=1:ow=2:tw=3"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_dir_bits_off: "ls" ["--color=always", "colors"] env [("LS_COLORS", "ow=00:tw=00:st=00"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_dir_sticky_writable_off: "ls" ["--color=always", "colors"] env [("LS_COLORS", "tw=00"), ("TERM", "xterm"), ("COLORTERM", "")];
    ls_color_capability_unsupported: "ls" ["--color=always", "exec.sh", "a.txt"] env [("LS_COLORS", "ca=30;41"), ("TERM", "xterm"), ("COLORTERM", "")];
}

// ---------------------------------------------------------------------------
// Cases whose output contains figures that may legitimately move between the
// Dogfish and GNU runs.
// ---------------------------------------------------------------------------

fn run_pair(tool: &str, args: &[&str]) -> Option<(Output, Output)> {
    let gnu = gnu_tool(tool).or_else(|| {
        eprintln!("note: skipping — system `{tool}` is missing or not GNU coreutils");
        None
    })?;
    let mut ours = Command::new(dogfish_bin());
    ours.arg(tool).args(args);
    let mut theirs = Command::new(gnu);
    theirs.arg0(tool).args(args);
    Some((
        exec(ours, fixtures(), None, "C"),
        exec(theirs, fixtures(), None, "C"),
    ))
}

fn assert_same(tool: &str, args: &[&str], a: &Output, b: &Output, mask: fn(&str) -> String) {
    let text = |o: &Output| mask(&String::from_utf8_lossy(&o.stdout));
    let ctx = format!(
        "\ncase: {tool} {args:?}\n=== dogfish ===\n{}\n{}\n=== GNU ===\n{}\n{}\n",
        String::from_utf8_lossy(&a.stdout),
        String::from_utf8_lossy(&a.stderr),
        String::from_utf8_lossy(&b.stdout),
        String::from_utf8_lossy(&b.stderr),
    );
    assert_eq!(text(a), text(b), "stdout differs{ctx}");
    assert_eq!(a.status.code(), b.status.code(), "exit status differs{ctx}");
    assert_eq!(a.stderr, b.stderr, "stderr differs{ctx}");
}

/// Compares after `mask` has blanked parts that differ between runs anyway.
fn check_masked(tool: &str, args: &[&str], mask: fn(&str) -> String) {
    if let Some((a, b)) = run_pair(tool, args) {
        assert_same(tool, args, &a, &b, mask);
    }
}

/// For live file-system figures (`df`, `stat -f`): demands byte-identical
/// output, retrying a few times in case the disk changed between the two runs,
/// then falls back to comparing with those figures masked. Either way the
/// layout (headers, columns, row order) must match exactly.
fn check_volatile(tool: &str, args: &[&str], mask: fn(&str) -> String) {
    for _ in 0..5 {
        let Some((a, b)) = run_pair(tool, args) else {
            return;
        };
        if a.stdout == b.stdout && a.stderr == b.stderr && a.status.code() == b.status.code() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if let Some((a, b)) = run_pair(tool, args) {
        assert_same(tool, args, &a, &b, mask);
    }
}

/// Masks `stat`'s `Access:` time line (directories' access times move as the
/// suite lists them) and, for devices, the other time lines too.
fn mask_stat_times(out: &str) -> String {
    out.lines()
        .map(|line| {
            let is_time = ["Access: 1", "Access: 2", "Modify: ", "Change: ", " Birth: "]
                .iter()
                .any(|p| line.starts_with(p));
            if is_time { "<time>" } else { line }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Masks `df`'s used/available/percentage columns, located by header title.
/// Rows are compared as whitespace-separated fields, since a changed figure
/// may also change a column's width.
fn mask_df_usage(out: &str) -> String {
    let mut lines = out.lines();
    let Some(header) = lines.next() else {
        return String::new();
    };
    let header = header.replace("Mounted on", "Mounted_on");
    let titles: Vec<&str> = header.split_whitespace().collect();
    let volatile = [
        "Used",
        "Avail",
        "Available",
        "Use%",
        "Capacity",
        "IUsed",
        "IFree",
        "IUse%",
    ];
    let mut masked = vec![titles.join(" ")];
    for line in lines {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let row: Vec<&str> = fields
            .iter()
            .enumerate()
            .map(|(i, f)| match titles.get(i) {
                Some(t) if volatile.contains(t) => "#",
                _ => f,
            })
            .collect();
        masked.push(row.join(" "));
    }
    masked.join("\n")
}

/// Masks the free-space figures of `stat -f`.
fn mask_statfs_free(out: &str) -> String {
    let mut masked = Vec::new();
    for line in out.lines() {
        let mut words: Vec<String> = Vec::new();
        let mut previous = "";
        for word in line.split_whitespace() {
            let free = matches!(previous, "Free:" | "Available:");
            words.push(if free { "#".into() } else { word.to_string() });
            previous = word;
        }
        masked.push(words.join(" "));
    }
    masked.join("\n")
}

#[test]
fn stat_default_directory() {
    check_masked("stat", &["dir", "empty_dir", "sticky"], mask_stat_times);
}

#[test]
fn stat_default_device() {
    check_masked("stat", &["/dev/null"], mask_stat_times);
}

#[test]
fn stat_terse_directory() {
    // %X (access time) is the 12th field; directories' may move.
    fn mask(out: &str) -> String {
        out.lines()
            .map(|l| {
                let mut f: Vec<&str> = l.split(' ').collect();
                if f.len() > 11 {
                    f[11] = "#";
                }
                f.join(" ")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    check_masked("stat", &["-t", "dir"], mask);
}

#[test]
fn stat_file_system_default() {
    check_volatile("stat", &["-f", "a.txt"], mask_statfs_free);
}

#[test]
fn stat_file_system_terse() {
    // Fields 8, 9 and 11 are free blocks, available blocks and free inodes.
    fn mask(out: &str) -> String {
        out.lines()
            .map(|l| {
                let mut f: Vec<&str> = l.split(' ').collect();
                for i in [7, 8, 10] {
                    if i < f.len() {
                        f[i] = "#";
                    }
                }
                f.join(" ")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    check_volatile("stat", &["-f", "-t", "a.txt", "dir"], mask);
}

#[test]
fn df_default_layout() {
    check_volatile("df", &["a.txt"], mask_df_usage);
}

#[test]
fn df_posix_layout() {
    check_volatile("df", &["-P", "a.txt", "dir"], mask_df_usage);
    check_volatile("df", &["-Pk", "a.txt"], mask_df_usage);
    check_volatile("df", &["-PT", "a.txt"], mask_df_usage);
    check_volatile("df", &["-P", "-B512", "a.txt"], mask_df_usage);
}

#[test]
fn df_human_layout() {
    check_volatile("df", &["-h", "a.txt"], mask_df_usage);
    check_volatile("df", &["-H", "a.txt"], mask_df_usage);
    check_volatile("df", &["-Ph", "a.txt"], mask_df_usage);
    check_volatile("df", &["-hT", "a.txt"], mask_df_usage);
}

#[test]
fn df_type_layout() {
    check_volatile("df", &["-T", "a.txt", "/dev/null"], mask_df_usage);
}

#[test]
fn df_inode_layout() {
    check_volatile("df", &["-i", "a.txt"], mask_df_usage);
    check_volatile("df", &["-ih", "a.txt"], mask_df_usage);
    check_volatile("df", &["-iT", "a.txt"], mask_df_usage);
    check_volatile("df", &["-iP", "a.txt"], mask_df_usage);
}

#[test]
fn df_block_size_layout() {
    check_volatile("df", &["-B1000", "a.txt"], mask_df_usage);
    check_volatile("df", &["-BM", "a.txt"], mask_df_usage);
    check_volatile("df", &["-B512", "a.txt"], mask_df_usage);
    check_volatile("df", &["-k", "a.txt"], mask_df_usage);
}

#[test]
fn df_total_layout() {
    check_volatile("df", &["--total", "a.txt", "dir"], mask_df_usage);
    check_volatile(
        "df",
        &["--total", "-T", "a.txt", "/dev/null"],
        mask_df_usage,
    );
}

#[test]
fn df_all_fields_layout() {
    check_volatile("df", &["--output", "a.txt"], mask_df_usage);
}

#[test]
fn df_all_file_systems_layout() {
    // See `df_all_mounts_stable` for why these exclude squashfs and devtmpfs.
    const DISTRO_DUMMIES: [&str; 4] = ["-x", "squashfs", "-x", "devtmpfs"];
    check_volatile("df", &DISTRO_DUMMIES, mask_df_usage);
    check_volatile(
        "df",
        &[&["-T"][..], &DISTRO_DUMMIES].concat(),
        mask_df_usage,
    );
    check_volatile(
        "df",
        &[&["-i"][..], &DISTRO_DUMMIES].concat(),
        mask_df_usage,
    );
    check_volatile("df", &["-a", "-T"], mask_df_usage);
}

// ---------------------------------------------------------------------------
// The Dogfish extensions keep their own output when piped.
// ---------------------------------------------------------------------------

fn dogfish(args: &[&str]) -> Output {
    let mut cmd = Command::new(dogfish_bin());
    cmd.args(args);
    exec(cmd, fixtures(), None, "C")
}

#[test]
fn ls_json_stays_ndjson_when_piped() {
    let out = dogfish(&["ls", "-J", "dir"]);
    assert!(out.status.success());
    let names: Vec<String> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["name"].to_string())
        .collect();
    assert_eq!(names, ["\"notes.md\"", "\"one\"", "\"sub\"", "\"to_a\""]);
}

#[test]
fn du_and_df_json_stay_ndjson_when_piped() {
    let out = dogfish(&["du", "-s", "-J", "dir"]);
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record["path"], "dir");
    assert!(record["size"].as_u64().unwrap() > 0);

    let out = dogfish(&["df", "-J", "a.txt"]);
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(record["total_bytes"].as_u64().unwrap() > 0);
}

#[test]
fn ls_tree_keeps_its_level_option() {
    let out = dogfish(&["ls", "--tree", "-L", "1", "dir"]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("sub/"), "{text}");
    assert!(!text.contains("three"), "-L 1 must not descend: {text}");
}
