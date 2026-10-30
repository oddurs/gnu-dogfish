//! The `-J` record contract (doc/ndjson.md), held for every producer: each
//! line is one compact JSON object, every record validates against the
//! tool's published schema (`doc/schema/<tool>.v1.json`, also built into
//! the binary as `dogfish schema TOOL`) and has no field the schema does not
//! declare, names that are not UTF-8 carry their bytes, and every consumer
//! takes any producer's records.

#[path = "ndjson/validator.rs"]
mod validator;

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine as _;
use serde_json::Value;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_dogfish")
}

fn schema_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../doc/schema")
}

/// The name of the fixture file whose name is not UTF-8.
const BAD: &[u8] = b"bad\xffname";

/// A directory with a few files (one with a non-UTF-8 name, a symlink, a
/// subdirectory) and an undo journal with an entry of each kind.
fn fixture() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ndjson-fixture");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("d/sub")).unwrap();
        fs::write(dir.join("d/a.txt"), "hello world\nsecond line\n").unwrap();
        fs::write(dir.join("d/sub/b.txt"), "x".repeat(5000)).unwrap();
        // Some file systems (APFS) refuse names that are not UTF-8 (EILSEQ):
        // the tests that need one check `has_bad_name` first.
        // `DOGFISH_TEST_NO_BAD_NAMES` runs them as such a file system would.
        if std::env::var_os("DOGFISH_TEST_NO_BAD_NAMES").is_none() {
            let _ = fs::write(dir.join("d").join(OsStr::from_bytes(BAD)), "odd\n");
        }
        std::os::unix::fs::symlink("a.txt", dir.join("d/link")).unwrap();
        let sums = run_in(&dir, &["md5sum", "d/a.txt", "d/sub/b.txt"], b"");
        fs::write(dir.join("sums.md5"), &sums.stdout).unwrap();
        // One journal entry of each kind: copy, move, trash.
        fs::write(dir.join("j1"), "1").unwrap();
        for args in [
            &["cp", "j1", "j2"][..],
            &["mv", "j2", "j3"],
            &["rm", "--safe", "j3"],
        ] {
            assert!(run_in(&dir, args, b"").status.success(), "{args:?}");
        }
        dir
    })
}

/// The tools that change files: each of their runs gets a tree of its own
/// ([`action_tree`]).
const ACTION_TOOLS: &[&str] = &[
    "cp",
    "mv",
    "rm",
    "mkdir",
    "ln",
    "chmod",
    "chown",
    "touch",
    "mktemp",
    "tee",
    "undo",
    "install-links",
];

/// What `tee` reads in the runs of [`producers`].
const TEE_INPUT: &[u8] = b"to every file\n";

/// Fills the journal of `dir` (an [`action_tree`]) with an entry of each
/// kind, for `undo` to reverse: a copy over a file, a move, a trash.
fn journal_some(dir: &Path) {
    for args in [
        &["cp", "a", "d/f"][..],
        &["mv", "d/sub/g", "e/g"],
        &["rm", "--safe", "d/l"],
        &["cp", "-r", "d/sub", "copy"],
    ] {
        assert!(run_in(dir, args, b"").status.success(), "{args:?}");
    }
}

/// A new directory for one run of a tool that changes files: `a` (a file),
/// `d/` with `f`, `l` (a symlink to `f`), `sub/g` and, where the file system
/// takes it, the non-UTF-8 name; and `e/`, an empty directory. Its journal
/// is its own (`DOGFISH_HOME` is under it, see [`run_os`]).
fn action_tree() -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("ndjson-actions")
        .join(N.fetch_add(1, Ordering::Relaxed).to_string());
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("d/sub")).unwrap();
    fs::create_dir_all(dir.join("e")).unwrap();
    fs::write(dir.join("a"), "hello\n").unwrap();
    fs::write(dir.join("d/f"), "x".repeat(3000)).unwrap();
    fs::write(dir.join("d/sub/g"), "g\n").unwrap();
    std::os::unix::fs::symlink("f", dir.join("d/l")).unwrap();
    if std::env::var_os("DOGFISH_TEST_NO_BAD_NAMES").is_none() {
        let _ = fs::write(dir.join("d").join(OsStr::from_bytes(BAD)), "odd\n");
    }
    dir
}

/// Runs one of `tool`'s command lines from [`producers`]: in a tree of its
/// own if the tool changes files, else in the shared fixture.
fn run_producer(tool: &str, args: &[&[u8]]) -> Output {
    if ACTION_TOOLS.contains(&tool) {
        let dir = action_tree();
        if tool == "undo" {
            journal_some(&dir);
        }
        let input = if tool == "tee" { TEE_INPUT } else { b"" };
        run_os(&dir, &os_args(args), input)
    } else {
        run_os(fixture(), &os_args(args), b"")
    }
}

/// Whether the fixture holds the non-UTF-8 name.
fn has_bad_name() -> bool {
    fixture().join("d").join(OsStr::from_bytes(BAD)).exists()
}

fn run_in(dir: &Path, args: &[&str], stdin: &[u8]) -> Output {
    run_os(dir, &args.iter().map(OsStr::new).collect::<Vec<_>>(), stdin)
}

fn run_os(dir: &Path, args: &[&OsStr], stdin: &[u8]) -> Output {
    let mut child = Command::new(bin())
        .args(args)
        .current_dir(dir)
        .env("DOGFISH_HOME", dir.join(".dogfish"))
        .env("TZ", "UTC0")
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

/// Each producer's schema name, and command lines that exercise it.
fn producers() -> Vec<(&'static str, Vec<Vec<&'static [u8]>>)> {
    let c = |args: &[&'static str]| -> Vec<&'static [u8]> {
        args.iter().map(|a| a.as_bytes()).collect()
    };
    vec![
        (
            "ls",
            vec![
                c(&["ls", "-J", "d"]),
                c(&["ls", "-lRJ", "d"]),
                c(&["ls", "-J", "/dev/null"]),
            ],
        ),
        (
            "stat",
            vec![c(&["stat", "-J", "d/a.txt", "d/link", "d", "/dev/null"])],
        ),
        (
            "df",
            vec![c(&["df", "-J", "d"]), c(&["df", "-J", "--total", "d"])],
        ),
        (
            "du",
            vec![
                c(&["du", "-aJ", "d"]),
                c(&["du", "-J", "--inodes", "d"]),
                c(&["du", "-J", "--time", "-c", "d"]),
            ],
        ),
        ("wc", vec![c(&["wc", "-J", "d/a.txt", "d/sub/b.txt"])]),
        ("nproc", vec![c(&["nproc", "-J"])]),
        ("env", vec![c(&["env", "-J"])]),
        (
            "printenv",
            vec![c(&["printenv", "-J"]), c(&["printenv", "-J", "TZ"])],
        ),
        (
            "realpath",
            vec![c(&["realpath", "-J", "d/a.txt", "d/link"])],
        ),
        (
            "md5sum",
            vec![
                c(&["md5sum", "-J", "d/a.txt"]),
                c(&["md5sum", "-J", "-c", "sums.md5"]),
            ],
        ),
        (
            "sha256sum",
            vec![c(&["sha256sum", "-J", "d/a.txt", "d/sub/b.txt"])],
        ),
        ("b2sum", vec![c(&["b2sum", "-J", "d/a.txt"])]),
        ("id", vec![c(&["id", "-J"]), c(&["id", "-J", "root"])]),
        ("uname", vec![c(&["uname", "-J"])]),
        (
            "date",
            vec![c(&["date", "-J"]), c(&["date", "-J", "-d", "@-1.5", "+%s"])],
        ),
        ("history", vec![c(&["history", "-J"])]),
        (
            "seq",
            vec![c(&["seq", "-J", "3"]), c(&["seq", "-J", "0", "0.25", "1"])],
        ),
        ("whoami", vec![c(&["whoami", "-J"])]),
        (
            "hostname",
            vec![c(&["hostname", "-J"]), c(&["hostname", "-sJ"])],
        ),
        (
            "basename",
            vec![
                c(&["basename", "-J", "d/a.txt", ".txt"]),
                c(&["basename", "-aJ", "d/a.txt", "/", "d/sub/"]),
                c(&["basename", "-J"]),
            ],
        ),
        (
            "dirname",
            vec![
                c(&["dirname", "-J", "d/a.txt", "/", "x", "d/sub/"]),
                c(&["dirname", "-zJ", "d/a.txt"]),
            ],
        ),
        (
            "bench",
            vec![c(&["bench", "-J", "-n", "1", "--size-mb", "1"])],
        ),
        // The tools that change files (each run in its own `action_tree`),
        // every one with a failing operand, for its error record.
        (
            "cp",
            vec![
                c(&["cp", "-rJ", "d", "missing", "e"]),
                c(&["cp", "-J", "-b", "a", "d/f"]),
                c(&["cp", "-sJ", "a", "s"]),
                c(&["cp", "-rlJ", "d", "h"]),
                c(&["cp", "-J", "--parents", "d/f", "e"]),
            ],
        ),
        (
            "mv",
            vec![
                c(&["mv", "-J", "d", "a", "missing", "e"]),
                c(&["mv", "-J", "-b", "a", "d/f"]),
                c(&["mv", "-J", "a", "b"]),
            ],
        ),
        (
            "rm",
            vec![
                c(&["rm", "-rJ", "d", "missing", "a"]),
                c(&["rm", "-J", "--safe", "a", "missing"]),
                c(&["rm", "-dJ", "e", "d"]),
            ],
        ),
        (
            "mkdir",
            vec![
                c(&["mkdir", "-pJ", "x/y", "a/z"]),
                c(&["mkdir", "-J", "n", "e"]),
            ],
        ),
        (
            "ln",
            vec![
                c(&["ln", "-sJ", "a", "s"]),
                c(&["ln", "-J", "-b", "a", "d/f"]),
                c(&["ln", "-J", "a", "missing", "e"]),
            ],
        ),
        (
            "chmod",
            vec![
                c(&["chmod", "-RJ", "u+x", "d", "missing"]),
                c(&["chmod", "-cJ", "700", "a", "e"]),
            ],
        ),
        ("chown", vec![c(&["chown", "-RJ", "", "d", "missing"])]),
        ("touch", vec![c(&["touch", "-J", "a", "new", "missing/x"])]),
        (
            "mktemp",
            vec![
                c(&["mktemp", "-J", "-p", ".", "t.XXXXXX"]),
                c(&["mktemp", "-dJ", "-p", "."]),
                c(&["mktemp", "-uJ"]),
                c(&["mktemp", "-J", "missing/XXXXXX"]),
            ],
        ),
        (
            "tee",
            vec![
                c(&["tee", "-J", "out", "d/f", "missing/x"]),
                c(&["tee", "-aJ", "a"]),
            ],
        ),
        ("undo", vec![c(&["undo", "-J", "-n", "9"])]),
        (
            "install-links",
            vec![
                c(&["install-links", "-J", "bin/x"]),
                c(&["install-links", "-J", "a"]),
            ],
        ),
        // Only error records.
        (
            "error",
            vec![
                c(&["ls", "-J", "missing"]),
                c(&["stat", "-J", "missing", "nope"]),
                c(&["wc", "-J", "missing"]),
            ],
        ),
    ]
}

fn schema(tool: &str) -> Value {
    let text = fs::read_to_string(schema_dir().join(format!("{tool}.v1.json"))).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// The records of one run: every line must be one compact JSON object.
fn records(args: &[&[u8]], out: &Output) -> Vec<Value> {
    let shown = String::from_utf8_lossy(&args.concat()).into_owned();
    let text = std::str::from_utf8(&out.stdout).unwrap_or_else(|_| panic!("{shown}: not UTF-8"));
    assert!(
        text.is_empty() || text.ends_with('\n'),
        "{shown}: unterminated"
    );
    text.lines()
        .map(|line| {
            assert!(
                line.starts_with('{') && line.ends_with('}'),
                "{shown}: not one object per line: {line}"
            );
            let v: Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}"));
            assert!(v.is_object(), "{shown}: {line}");
            // Compact: serializing again gives the line back, byte for byte
            // (no pretty-printing, no stray spaces).
            assert_eq!(
                serde_json::to_string(&v).unwrap().len(),
                line.len(),
                "{line}"
            );
            v
        })
        .collect()
}

fn os_args(args: &[&[u8]]) -> Vec<&'static OsStr> {
    args.iter()
        .map(|a| &*Box::leak(OsStr::from_bytes(a).to_os_string().into_boxed_os_str()))
        .collect()
}

#[test]
fn schema_files_are_valid_schemas_and_built_into_the_binary() {
    let mut files: Vec<String> = fs::read_dir(schema_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    let listed = run_in(fixture(), &["schema"], b"");
    let listed: Vec<String> = String::from_utf8(listed.stdout)
        .unwrap()
        .lines()
        .map(|t| format!("{t}.v1.json"))
        .collect();
    assert_eq!(files, listed, "doc/schema and `dogfish schema` differ");
    for f in &files {
        let text = fs::read_to_string(schema_dir().join(f)).unwrap();
        let s: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{f}: {e}"));
        let errs = validator::check_schema(&s);
        assert!(errs.is_empty(), "{f}: {errs:#?}");
        let tool = f.trim_end_matches(".v1.json");
        let shown = run_in(fixture(), &["schema", tool], b"");
        assert!(shown.status.success());
        assert_eq!(
            shown.stdout,
            text.as_bytes(),
            "{tool}: built-in schema is stale"
        );
    }
    // Every producer here has a schema, and every schema a producer here.
    let tested: Vec<String> = producers()
        .iter()
        .map(|(t, _)| format!("{t}.v1.json"))
        .collect();
    let mut tested_sorted = tested.clone();
    tested_sorted.sort();
    assert_eq!(files, tested_sorted);
}

#[test]
fn every_producer_writes_ndjson_that_its_schema_accepts() {
    for (tool, runs) in producers() {
        let schema = schema(tool);
        for args in runs {
            let out = run_producer(tool, &args);
            let recs = records(&args, &out);
            assert!(!recs.is_empty(), "{tool} {args:?}: no records");
            for r in &recs {
                let errs = validator::validate(&schema, r, true);
                assert!(errs.is_empty(), "{tool}: {r}: {errs:#?}");
            }
        }
    }
}

#[test]
fn single_record_tools_write_one_line() {
    for args in [
        &["id", "-J"][..],
        &["uname", "-J"],
        &["nproc", "-J"],
        &["whoami", "-J"],
        &["date", "-J"],
        &["hostname", "-J"],
        &["mktemp", "-uJ"],
    ] {
        let out = run_in(fixture(), args, b"");
        assert_eq!(
            out.stdout.iter().filter(|&&b| b == b'\n').count(),
            1,
            "{args:?}"
        );
    }
    let out = run_in(fixture(), &["seq", "-J", "2", "-1", "0"], b"");
    assert_eq!(out.stdout, b"{\"value\":2}\n{\"value\":1}\n{\"value\":0}\n");
}

#[test]
fn names_that_are_not_utf8_carry_their_bytes() {
    if !has_bad_name() {
        eprintln!("skipped: this file system refuses names that are not UTF-8");
        return;
    }
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let mut path = b"d/".to_vec();
    path.extend_from_slice(BAD);
    let path: &'static [u8] = Box::leak(path.into_boxed_slice());
    let check = |args: &[&[u8]], key: &str, want: &[u8]| {
        let out = run_os(fixture(), &os_args(args), b"");
        let recs = records(args, &out);
        let hit = recs
            .iter()
            .find(|r| r.get(format!("{key}_bytes")).is_some())
            .unwrap_or_else(|| panic!("{args:?}: no {key}_bytes in {recs:?}"));
        assert_eq!(hit[format!("{key}_bytes")], b64(want), "{args:?}");
        assert_eq!(hit[key], String::from_utf8_lossy(want).as_ref(), "{args:?}");
        // Fields that are valid UTF-8 have no twin.
        assert!(
            recs.iter()
                .filter(|r| r[key].as_str().is_some_and(|s| !s.contains('\u{fffd}')))
                .all(|r| r.get(format!("{key}_bytes")).is_none())
        );
    };
    check(&[b"ls", b"-J", b"d"], "path", path);
    check(&[b"ls", b"-J", b"d"], "name", BAD);
    check(&[b"stat", b"-J", path], "path", path);
    check(&[b"du", b"-aJ", b"d"], "path", path);
    check(&[b"realpath", b"-J", path], "path", path);
    check(&[b"md5sum", b"-J", path], "path", path);
}

#[test]
fn operands_that_are_not_utf8_carry_their_bytes() {
    // basename and dirname work on the operand alone, so this needs no file
    // system that takes such names.
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let mut path = b"d\xfe/".to_vec();
    path.extend_from_slice(BAD);
    let args: Vec<&[u8]> = vec![b"basename", b"-J", &path];
    let recs = records(&args, &run_os(fixture(), &os_args(&args), b""));
    assert_eq!(recs[0]["path_bytes"], b64(&path));
    assert_eq!(recs[0]["name_bytes"], b64(BAD));
    assert_eq!(recs[0]["name"], String::from_utf8_lossy(BAD).as_ref());
    let args: Vec<&[u8]> = vec![b"dirname", b"-J", &path];
    let recs = records(&args, &run_os(fixture(), &os_args(&args), b""));
    assert_eq!(recs[0]["dir_bytes"], b64(b"d\xfe"));
    // A name that is UTF-8 has no twin.
    let recs = records(
        &[b"basename"],
        &run_in(fixture(), &["basename", "-J", "d/a"], b""),
    );
    assert_eq!(recs, [serde_json::json!({"path": "d/a", "name": "a"})]);
}

#[test]
fn json_takes_version_1_only() {
    let one = run_in(fixture(), &["seq", "--json=1", "1"], b"");
    assert_eq!(one.stdout, b"{\"value\":1}\n");
    for tool in ["ls", "seq", "id", "wc", "query", "sort"] {
        let out = run_in(fixture(), &[tool, "--json=2"], b"");
        assert!(!out.status.success(), "{tool}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.starts_with(&format!(
                "{tool}: unsupported JSON version '2' (supported: 1)\nTry '{tool} --help'"
            )),
            "{tool}: {err}"
        );
    }
    // -J takes no argument, so it bundles with other options.
    let out = run_in(fixture(), &["ls", "-1J", "d"], b"");
    assert!(out.status.success());
    assert!(out.stdout.starts_with(b"{\"path\":"));
    // --json is never abbreviated (`--js` stays GNU's unrecognized option).
    let out = run_in(fixture(), &["wc", "--js"], b"");
    assert!(!out.status.success());
}

#[test]
fn consumers_take_every_producers_records() {
    for (tool, runs) in producers() {
        let args = &runs[0];
        let produced = run_producer(tool, args);
        let lines: Vec<&[u8]> = produced
            .stdout
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .collect();
        let n = lines.len();
        // query, head, tail, sort and shuf pass the records on unchanged.
        let q = run_in(fixture(), &["query", "-J"], &produced.stdout);
        assert!(
            q.status.success(),
            "{tool} | query: {}",
            String::from_utf8_lossy(&q.stderr)
        );
        assert_eq!(q.stdout, produced.stdout, "{tool} | query");
        let head = run_in(fixture(), &["head", "-J", "-n", "1"], &produced.stdout);
        assert_eq!(
            head.stdout.split(|&b| b == b'\n').next(),
            lines.first().copied(),
            "{tool} | head"
        );
        let tail = run_in(fixture(), &["tail", "-J", "-n", "1"], &produced.stdout);
        assert_eq!(
            &tail.stdout[..tail.stdout.len() - 1],
            *lines.last().unwrap(),
            "{tool} | tail"
        );
        for consumer in [&["sort", "-J"][..], &["shuf", "-J"]] {
            let out = run_in(fixture(), consumer, &produced.stdout);
            let mut got: Vec<&[u8]> = out
                .stdout
                .split(|&b| b == b'\n')
                .filter(|l| !l.is_empty())
                .collect();
            let mut want = lines.clone();
            got.sort();
            want.sort();
            assert_eq!(got, want, "{tool} | {consumer:?}");
        }
        // uniq adds a count; cut and query -f project fields, keeping the
        // first field's raw-bytes twin.
        let first: Value = serde_json::from_slice(lines[0]).unwrap();
        let key = first.as_object().unwrap().keys().next().unwrap().clone();
        let uniq = run_in(
            fixture(),
            &["uniq", "-J", "-c", "--by", &key],
            &produced.stdout,
        );
        let u = records(&[b"uniq"], &uniq);
        assert!(!u.is_empty() && u.len() <= n, "{tool} | uniq");
        assert!(
            u.iter().all(|r| r["count"].as_u64().unwrap() >= 1),
            "{tool} | uniq"
        );
        for consumer in [&["cut", "-J", "-f"][..], &["query", "-J", "-f"]] {
            let mut argv = consumer.to_vec();
            argv.push(&key);
            let out = run_in(fixture(), &argv, &produced.stdout);
            let recs = records(&[b"cut"], &out);
            assert_eq!(recs.len(), n, "{tool} | {argv:?}");
            for r in &recs {
                let keys: Vec<&String> = r.as_object().unwrap().keys().collect();
                assert!(
                    keys.iter()
                        .all(|k| **k == key || **k == format!("{key}_bytes")),
                    "{r}"
                );
            }
        }
    }
}

#[test]
fn consumers_keep_raw_bytes_twins_and_filter_on_new_fields() {
    let ls = run_in(fixture(), &["ls", "-J", "d"], b"");
    let q = run_in(
        fixture(),
        &["query", "-w", "type=file", "-s", "size", "-f", "name", "-J"],
        &ls.stdout,
    );
    let recs = records(&[b"query"], &q);
    assert_eq!(recs.len(), 1 + usize::from(has_bad_name()));
    assert_eq!(
        recs.iter().any(|r| r.get("name_bytes").is_some()),
        has_bad_name()
    );
    // Integer nanoseconds compare exactly.
    let du = run_in(fixture(), &["du", "-aJ", "--time", "d"], b"");
    let sorted = run_in(fixture(), &["sort", "-J", "--by", "mtime_ns"], &du.stdout);
    let times: Vec<i64> = records(&[b"sort"], &sorted)
        .iter()
        .map(|r| r["mtime_ns"].as_i64().unwrap())
        .collect();
    assert!(times.windows(2).all(|w| w[0] <= w[1]), "{times:?}");
    // No records in, none out.
    let empty = run_in(fixture(), &["query", "-J"], b"");
    assert!(empty.status.success());
    assert!(empty.stdout.is_empty());
}

#[test]
fn without_json_output_is_unchanged_text() {
    // -J is the only switch to records: the same command without it prints
    // GNU's text (the gnu_compat suites compare that text byte for byte).
    let out = run_in(fixture(), &["seq", "2"], b"");
    assert_eq!(out.stdout, b"1\n2\n");
    let out = run_in(fixture(), &["wc", "-l", "d/a.txt"], b"");
    assert_eq!(out.stdout, b"2 d/a.txt\n");
}

// ----------------------------------------------------------- action records

/// Runs a tool that changes files in `dir`; returns its records (each
/// checked against the tool's schema) and the run.
fn act(dir: &Path, args: &[&str]) -> (Vec<Value>, Output) {
    let out = run_in(dir, args, b"");
    let bytes: Vec<&[u8]> = args.iter().map(|a| a.as_bytes()).collect();
    let recs = records(&bytes, &out);
    let schema = schema(args[0]);
    for r in &recs {
        let errs = validator::validate(&schema, r, true);
        assert!(errs.is_empty(), "{args:?}: {r}: {errs:#?}");
    }
    (recs, out)
}

/// A record in short: its action (or `error`) and its names.
fn brief(r: &Value) -> String {
    let field = |k: &str| r[k].as_str().unwrap_or("").to_string();
    match r["action"].as_str() {
        None => format!("error {}", field("path")),
        Some(a) if r.get("source").is_some() => {
            format!("{a} {} {}", field("source"), field("dest"))
        }
        Some(a) => format!("{a} {}", field("path")),
    }
}

fn briefs(recs: &[Value]) -> Vec<String> {
    recs.iter().map(brief).collect()
}

/// The names a record is about, which its `-v` line names too.
fn names(r: &Value) -> Vec<String> {
    ["path", "source", "dest", "backup"]
        .iter()
        .filter_map(|k| r[*k].as_str().map(str::to_string))
        .collect()
}

/// The device of `p`.
fn dev(p: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(p).ok().map(|m| m.dev())
}

/// A directory on another file system than the test trees (`/dev/shm` on
/// Linux), for moves across file systems; `None` if there is none.
fn other_fs_dir() -> Option<PathBuf> {
    let shm = Path::new("/dev/shm");
    if dev(shm)? == dev(Path::new(env!("CARGO_TARGET_TMPDIR")))? {
        return None;
    }
    let dir = shm.join(format!("dogfish-ndjson-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir(&dir).ok()?;
    Some(dir)
}

/// The command lines whose `-J` records are compared with `-v`'s lines.
const PARITY: &[&[&str]] = &[
    &["cp", "-r", "d", "missing", "e"],
    &["cp", "-b", "a", "d/f"],
    &["cp", "-rb", "d", "e"],
    &["cp", "-rl", "d", "h"],
    &["cp", "-s", "a", "s"],
    &["cp", "-a", "d", "h"],
    &["cp", "--parents", "d/f", "d/sub/g", "e"],
    &["cp", "--remove-destination", "a", "d/f"],
    &["mv", "d", "a", "missing", "e"],
    &["mv", "-b", "a", "d/f"],
    &["mv", "--backup=numbered", "a", "d/f"],
    &["rm", "-r", "d", "missing", "a"],
    &["rm", "-d", "e", "d"],
    &["rm", "--safe", "a", "d"],
    &["mkdir", "-p", "x/y/z", "a/q", "e"],
    &["ln", "-s", "a", "s"],
    &["ln", "-b", "a", "d/f"],
    &["ln", "-sf", "a", "d/sub/g", "missing", "e"],
    &["chmod", "-R", "go-r", "d", "missing"],
    &["chmod", "-c", "u+x", "a", "e", "missing"],
    &["chmod", "-R", "-c", "u+x", "d"],
    &["chown", "-R", "", "d", "missing"],
    &["chown", "-c", "", "a"],
];

/// `args` with `flag` after the tool's name.
fn with(args: &[&str], flag: &str) -> Vec<&'static str> {
    let mut v: Vec<&'static str> = args
        .iter()
        .map(|a| &*Box::leak(a.to_string().into_boxed_str()))
        .collect();
    v.insert(1, Box::leak(flag.to_string().into_boxed_str()));
    v
}

/// Empties `dir` (the other file system's directory) before a run.
fn renew(dir: Option<&PathBuf>) {
    if let Some(o) = dir {
        let _ = fs::remove_dir_all(o);
        fs::create_dir(o).unwrap();
    }
}

#[test]
fn action_records_are_the_lines_of_v_in_order() {
    let other = other_fs_dir();
    let mut runs: Vec<Vec<&str>> = PARITY.iter().map(|a| a.to_vec()).collect();
    if let Some(o) = &other {
        let o: &'static str = Box::leak(o.to_string_lossy().into_owned().into_boxed_str());
        runs.push(vec!["mv", "d", "a", "missing", o]);
    }
    for args in runs {
        // -v (or -c) prints a line per action; -J writes a record instead.
        let verbose = if args.contains(&"-c") {
            args.clone()
        } else {
            with(&args, "-v")
        };
        let (vdir, jdir) = (action_tree(), action_tree());
        renew(other.as_ref());
        let v = run_in(&vdir, &verbose, b"");
        let vtree = other.as_ref().map(|o| listing(o));
        renew(other.as_ref());
        let (recs, j) = act(&jdir, &with(&args, "-J"));
        assert_eq!(v.status.code(), j.status.code(), "{args:?}");
        let vlines: Vec<String> = String::from_utf8_lossy(&v.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        // One record per -v line, naming what the line names; a
        // --backup has a record of its own, before the action's, whose line
        // names it. Diagnostics are error records.
        let actions: Vec<&Value> = recs
            .iter()
            .filter(|r| r.get("action").is_some() && r["action"] != "backup")
            .collect();
        assert_eq!(
            vlines.len(),
            actions.len(),
            "{args:?}:\n{vlines:#?}\n{:#?}",
            briefs(&recs)
        );
        // Each record names what one of the lines names. They are matched by
        // name, not position: the two runs use two trees, and siblings come
        // in inode order, which the file system need not repeat (ext4 puts a
        // new directory in another block group).
        let mut unmatched: Vec<&String> = vlines.iter().collect();
        for r in &actions {
            let wanted: Vec<String> = names(r)
                .into_iter()
                .filter(|n| !n.contains('\u{fffd}'))
                .collect();
            // A name quoted whole first: `d/sub` is also a substring of the
            // line for `d/sub/g`, and taking that line for the directory's
            // record (as inode order may have it come first) leaves none
            // for the file's.
            let hit = unmatched
                .iter()
                .position(|line| {
                    wanted
                        .iter()
                        .all(|n| line.contains(format!("'{n}'").as_str()))
                })
                .or_else(|| {
                    unmatched
                        .iter()
                        .position(|line| wanted.iter().all(|n| line.contains(n.as_str())))
                });
            let Some(i) = hit else {
                panic!("{args:?}: no line for {r} in {unmatched:#?}");
            };
            unmatched.remove(i);
        }
        for (i, r) in recs.iter().enumerate() {
            if r["action"] == "backup" {
                let next = &recs[i + 1];
                assert_eq!(next["backup"], r["dest"], "{args:?}");
                assert_eq!(next["dest"], r["source"], "{args:?}");
            }
        }
        let errors = recs.iter().filter(|r| r.get("error").is_some()).count();
        let diagnostics = String::from_utf8_lossy(&j.stderr)
            .lines()
            .filter(|l| l.starts_with(&format!("{}: ", args[0])))
            .count();
        assert_eq!(errors, diagnostics, "{args:?}");
        // And both runs did the same.
        assert_eq!(listing(&vdir), listing(&jdir), "{args:?}");
        assert_eq!(vtree, other.as_ref().map(|o| listing(o)), "{args:?}");
    }
    if let Some(o) = other {
        let _ = fs::remove_dir_all(o);
    }
}

/// The names under `dir` with their modes (the journal left out), sorted.
fn listing(dir: &Path) -> Vec<String> {
    use std::os::unix::fs::MetadataExt;
    fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if rel.is_empty() && name == ".dogfish" {
                continue;
            }
            let path = format!("{rel}/{name}");
            let m = e.path().symlink_metadata().unwrap();
            out.push(format!("{path} {:o}", m.mode()));
            if m.is_dir() {
                walk(&e.path(), &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, "", &mut out);
    out.sort();
    out
}

#[test]
fn an_error_record_comes_where_its_diagnostic_does() {
    let dir = action_tree();
    let (recs, out) = act(&dir, &["rm", "-rJ", "d", "missing", "a"]);
    assert!(!out.status.success());
    let b = briefs(&recs);
    let at = b.iter().position(|r| r == "error missing").unwrap();
    // Everything under d first, then the failing operand, then a.
    assert!(b[..at].iter().all(|r| r.contains(" d")), "{b:#?}");
    assert_eq!(b[at - 1], "rmdir d");
    assert_eq!(b[at + 1..], ["remove a".to_string()], "{b:#?}");
    let err = &recs[at];
    assert_eq!(
        err["error"],
        "cannot remove 'missing': No such file or directory"
    );
    assert_eq!(err["errno"], 2);
    assert_eq!(
        out.stderr,
        b"rm: cannot remove 'missing': No such file or directory\n"
    );

    // Phase-1 producers too: the error record is in order with the others.
    let dir = action_tree();
    for args in [
        &["wc", "-J", "a", "missing", "d/f"][..],
        &["md5sum", "-J", "a", "missing", "d/f"],
        &["sha256sum", "-J", "a", "missing", "d/f"],
        &["realpath", "-J", "a", "missing/x", "d/f"],
        &["du", "-J", "a", "missing", "d/f"],
        &["stat", "-J", "a", "missing", "d/f"],
    ] {
        let out = run_in(&dir, args, b"");
        let recs = records(&[args[0].as_bytes()], &out);
        let kinds: Vec<bool> = recs.iter().map(|r| r.get("error").is_some()).collect();
        assert_eq!(kinds, [false, true, false], "{args:?}: {recs:#?}");
        assert_eq!(recs[1]["errno"], 2, "{args:?}");
        assert!(recs[1]["path"].as_str().unwrap().starts_with("missing"));
        let schema = schema(args[0]);
        for r in &recs {
            assert!(validator::validate(&schema, r, true).is_empty(), "{r}");
        }
    }
    // ls and df report missing operands first, as GNU does.
    let out = run_in(&dir, &["df", "-J", "a", "missing"], b"");
    let recs = records(&[b"df"], &out);
    assert_eq!(recs[0]["path"], "missing");
    assert_eq!(recs[1]["path"], "a");
    let out = run_in(&dir, &["ls", "-J", "a", "missing"], b"");
    let recs = records(&[b"ls"], &out);
    assert_eq!(recs[0]["path"], "missing");
    assert_eq!(recs[1]["path"], "a");
    // A name that is not UTF-8 is given as bytes in the error record too.
    if has_bad_name() {
        let mut missing = b"nope".to_vec();
        missing.extend_from_slice(BAD);
        let args: Vec<&[u8]> = vec![b"rm", b"-J", &missing];
        let out = run_os(&dir, &os_args(&args), b"");
        let recs = records(&args, &out);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&missing);
        assert_eq!(recs[0]["path_bytes"], b64.as_str());
    }
}

#[test]
fn action_records_of_each_tool() {
    let dir = action_tree();
    let (recs, _) = act(&dir, &["cp", "-J", "-b", "a", "d/f"]);
    assert_eq!(briefs(&recs), ["backup d/f d/f~", "copy a d/f"]);
    assert_eq!(recs[1]["bytes"], 6);
    assert_eq!(recs[1]["type"], "file");
    assert_eq!(recs[1]["backup"], "d/f~");
    let (recs, _) = act(&dir, &["mv", "-J", "a", "b"]);
    assert_eq!(briefs(&recs), ["move a b"]);
    assert_eq!(recs[0]["type"], "file");
    let (recs, _) = act(&dir, &["ln", "-sJ", "b", "c"]);
    assert_eq!(briefs(&recs), ["symlink b c"]);
    let (recs, _) = act(&dir, &["ln", "-J", "b", "e"]);
    assert_eq!(briefs(&recs), ["link b e/b"]);
    let (recs, _) = act(&dir, &["mkdir", "-pJ", "x/y", "e"]);
    assert_eq!(briefs(&recs), ["mkdir x", "mkdir x/y"]);
    let (recs, out) = act(&dir, &["mkdir", "-J", "e"]);
    assert_eq!(briefs(&recs), ["error e"]);
    assert_eq!(recs[0]["errno"], 17);
    assert_eq!(out.status.code(), Some(1));
    let (recs, _) = act(&dir, &["touch", "-J", "b", "t", "nodir/t"]);
    assert_eq!(briefs(&recs), ["touch b", "touch t", "error nodir/t"]);
    assert_eq!(recs[0]["created"], false);
    assert_eq!(recs[1]["created"], true);

    // chmod: changed and retained, with both modes.
    let (recs, _) = act(&dir, &["chmod", "-J", "0644", "b", "t"]);
    assert_eq!(recs[0]["status"], "retained");
    assert_eq!(recs[0]["changed"], false);
    let (recs, _) = act(&dir, &["chmod", "-J", "0600", "t"]);
    assert_eq!(recs[0]["status"], "changed");
    assert_eq!(recs[0]["changed"], true);
    assert_eq!(recs[0]["mode_before"], "0644");
    assert_eq!(recs[0]["mode_before_symbolic"], "-rw-r--r--");
    assert_eq!(recs[0]["mode"], "0600");
    assert_eq!(recs[0]["mode_symbolic"], "-rw-------");
    // -c: only the changes.
    let (recs, _) = act(&dir, &["chmod", "-cJ", "0600", "b", "t", "nope"]);
    assert_eq!(briefs(&recs), ["chmod b", "error nope"]);

    // chown to the owner the file has: retained, with the ids.
    let uid = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(dir.join("b")).unwrap().uid()
    };
    let spec: &'static str = Box::leak(uid.to_string().into_boxed_str());
    let (recs, _) = act(&dir, &["chown", "-J", spec, "b"]);
    assert_eq!(recs[0]["status"], "retained");
    assert_eq!(recs[0]["uid_before"], u64::from(uid));
    assert_eq!(recs[0]["uid"], u64::from(uid));
    let (recs, _) = act(&dir, &["chown", "-cJ", spec, "b"]);
    assert!(recs.is_empty());

    // rm: files and directories.
    let (recs, _) = act(&dir, &["rm", "-rJ", "x"]);
    assert_eq!(briefs(&recs), ["rmdir x/y", "rmdir x"]);
    let (recs, _) = act(&dir, &["rm", "-J", "c"]);
    assert_eq!(briefs(&recs), ["remove c"]);
}

#[test]
fn undo_ids_name_the_journal_entry() {
    let dir = action_tree();
    let (recs, _) = act(&dir, &["rm", "-J", "--safe", "a"]);
    assert_eq!(briefs(&recs), ["trash a"]);
    let id = recs[0]["undo_id"].as_u64().unwrap();
    let hist = run_in(&dir, &["history", "-J"], b"");
    let hist = records(&[b"history"], &hist);
    assert!(hist.iter().any(|h| h["id"] == id), "{hist:#?}");
    assert!(run_in(&dir, &["undo"], b"").status.success());
    assert!(dir.join("a").exists());

    // Every record of one cp operand has its entry's id; undo removes the copy.
    let (recs, _) = act(&dir, &["cp", "-rJ", "d", "copy"]);
    let ids: Vec<&Value> = recs.iter().map(|r| &r["undo_id"]).collect();
    assert!(ids.iter().all(|i| i.as_u64().is_some() && *i == ids[0]));
    assert!(run_in(&dir, &["undo"], b"").status.success());
    assert!(!dir.join("copy").exists());
    let (recs, _) = act(&dir, &["mv", "-J", "a", "moved"]);
    assert!(recs[0]["undo_id"].as_u64().is_some());
}

/// The system's GNU coreutils `tool`, if there is one.
fn gnu_tool(tool: &str) -> Option<PathBuf> {
    let path = Path::new("/usr/bin").join(tool);
    let out = Command::new(&path).arg("--version").output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .contains("GNU coreutils")
        .then_some(path)
}

#[test]
fn json_keeps_gnu_stderr_and_writes_records_only() {
    // With -J, standard error is exactly what the tool prints without it,
    // which is GNU's (the gnu_compat suites hold that); here it is also
    // checked against the system's GNU coreutils, when there is one.
    for args in [
        &["cp", "-r", "d", "missing", "e"][..],
        &["cp", "d", "e"],
        &["mv", "missing", "a", "e/nodir/x"],
        &["rm", "-r", "missing", "d", "e/"],
        &["rm", "e", "."],
        &["mkdir", "e", "a/b"],
        &["ln", "-s", "a", "e", "missing"],
        &["ln", "a", "d/f"],
        &["chmod", "u+x", "missing", "a"],
        &["chmod", "-R", "u+x", "d", "nope"],
        &["chown", "", "missing", "a"],
        &["touch", "nodir/x", "a"],
        &["ls", "missing", "a"],
        &["stat", "missing"],
        &["du", "missing"],
        &["wc", "missing", "a"],
        &["md5sum", "missing"],
        &["realpath", "missing/x"],
        &["basename"],
        &["basename", "a", "b", "c"],
        &["dirname"],
        &["mktemp", "missing/XXXXXX"],
        &["mktemp", "XX"],
        &["tee", "missing/x", "a"],
    ] {
        let plain = run_in(&action_tree(), args, b"");
        let json = run_in(&action_tree(), &with(args, "-J"), b"");
        assert_eq!(
            String::from_utf8_lossy(&json.stderr),
            String::from_utf8_lossy(&plain.stderr),
            "{args:?}"
        );
        assert_eq!(json.status.code(), plain.status.code(), "{args:?}");
        if let Some(gnu) = gnu_tool(args[0]) {
            let out = Command::new(gnu)
                .arg0(args[0])
                .args(&args[1..])
                .current_dir(action_tree())
                .env("LC_ALL", "C")
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&json.stderr),
                String::from_utf8_lossy(&out.stderr),
                "{args:?} vs GNU"
            );
            assert_eq!(json.status.code(), out.status.code(), "{args:?} vs GNU");
        }
        // Standard output holds records only: one error record per
        // diagnostic.
        let recs = records(&[args[0].as_bytes()], &json);
        let errors = recs.iter().filter(|r| r.get("error").is_some()).count();
        let diagnostics = String::from_utf8_lossy(&json.stderr)
            .lines()
            .filter(|l| l.starts_with(&format!("{}: ", args[0])))
            .count();
        assert!(diagnostics > 0, "{args:?}");
        assert_eq!(errors, diagnostics, "{args:?}");
    }
    // Nor --debug's text.
    let out = run_in(&action_tree(), &["cp", "-J", "--debug", "a", "b"], b"");
    assert_eq!(briefs(&records(&[b"cp"], &out)), ["copy a b"]);
}

#[test]
fn query_reads_action_and_error_records() {
    let dir = action_tree();
    let (_, cp) = act(&dir, &["cp", "-rJ", "d", "missing", "e"]);
    let copies = run_in(&dir, &["query", "-J", "-w", "action == copy"], &cp.stdout);
    let copies = records(&[b"query"], &copies);
    assert!(copies.len() >= 4, "{copies:#?}");
    assert!(copies.iter().all(|r| r["action"] == "copy"));
    let files = run_in(
        &dir,
        &[
            "query",
            "-J",
            "-w",
            "type=file",
            "-s",
            "bytes",
            "-f",
            "dest,bytes",
        ],
        &cp.stdout,
    );
    let files = records(&[b"query"], &files);
    assert!(
        files
            .windows(2)
            .all(|w| w[0]["bytes"].as_u64() <= w[1]["bytes"].as_u64())
    );
    let errors = run_in(&dir, &["query", "-J", "-w", "errno=2"], &cp.stdout);
    assert_eq!(briefs(&records(&[b"query"], &errors)), ["error missing"]);
    let (_, chmod) = act(&dir, &["chmod", "-RJ", "u+x", "e"]);
    let changed = run_in(&dir, &["query", "-J", "-w", "changed=true"], &chmod.stdout);
    let changed = records(&[b"query"], &changed);
    assert!(!changed.is_empty());
    assert!(changed.iter().all(|r| r["status"] == "changed"));
}

#[test]
fn records_of_mktemp_tee_undo_and_install_links() {
    // mktemp: what it made, in place of its name.
    let dir = action_tree();
    let (recs, out) = act(&dir, &["mktemp", "-J", "-p", ".", "t.XXXX"]);
    assert!(out.status.success());
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0]["action"], "create");
    let made = recs[0]["path"].as_str().unwrap();
    assert!(
        made.starts_with("./t.") && dir.join(made).is_file(),
        "{made}"
    );
    let (recs, _) = act(&dir, &["mktemp", "-dJ", "-p", "."]);
    assert_eq!(recs[0]["action"], "mkdir");
    assert!(dir.join(recs[0]["path"].as_str().unwrap()).is_dir());
    // -u makes nothing: no action.
    let (recs, _) = act(&dir, &["mktemp", "-uJ", "-p", ".", "u.XXXX"]);
    assert_eq!(recs[0]["dry_run"], true);
    assert!(recs[0].get("action").is_none());
    assert!(!dir.join(recs[0]["path"].as_str().unwrap()).exists());
    let (recs, out) = act(&dir, &["mktemp", "-J", "nodir/XXXX"]);
    assert_eq!(briefs(&recs), ["error nodir/XXXX"]);
    assert_eq!(recs[0]["errno"], 2);
    assert_eq!(out.status.code(), Some(1));

    // tee: the data goes to the FILEs only; a record per FILE written.
    let dir = action_tree();
    let out = run_in(&dir, &["tee", "-J", "t1", "t2", "nodir/x"], b"data\n");
    let recs = records(&[b"tee"], &out);
    assert_eq!(briefs(&recs), ["error nodir/x", "write t1", "write t2"]);
    assert_eq!(recs[1]["bytes"], 5);
    assert_eq!(recs[1]["append"], false);
    assert_eq!(fs::read(dir.join("t1")).unwrap(), b"data\n");
    assert_eq!(fs::read(dir.join("t2")).unwrap(), b"data\n");
    assert_eq!(out.stderr, b"tee: nodir/x: No such file or directory\n");
    assert_eq!(out.status.code(), Some(1));
    let out = run_in(&dir, &["tee", "-aJ", "t1"], b"more\n");
    let recs = records(&[b"tee"], &out);
    assert_eq!(recs[0]["append"], true);
    assert_eq!(recs[0]["bytes"], 5);
    assert_eq!(fs::read(dir.join("t1")).unwrap(), b"data\nmore\n");
    // Without -J, standard output gets the data, as in GNU.
    let out = run_in(&dir, &["tee", "t3"], b"x\n");
    assert_eq!(out.stdout, b"x\n");

    // undo: one record per entry, newest first, naming what it put back.
    let dir = action_tree();
    journal_some(&dir);
    let hist = records(&[b"history"], &run_in(&dir, &["history", "-J"], b""));
    let (recs, out) = act(&dir, &["undo", "-J", "-n", "9"]);
    assert!(out.status.success());
    assert_eq!(recs.len(), hist.len());
    let ids: Vec<&Value> = recs.iter().map(|r| &r["undo_id"]).collect();
    let want: Vec<&Value> = hist.iter().rev().map(|h| &h["id"]).collect();
    assert_eq!(ids, want);
    let at = |p: &str| dir.join(p).to_string_lossy().into_owned();
    let undone: Vec<(&str, &str)> = recs
        .iter()
        .map(|r| (r["tool"].as_str().unwrap(), r["undone"].as_str().unwrap()))
        .collect();
    assert_eq!(
        undone,
        [
            ("cp", "copy"),
            ("rm", "trash"),
            ("mv", "move"),
            ("cp", "copy")
        ]
    );
    assert_eq!(recs[0]["removed"], serde_json::json!([at("copy")]));
    assert_eq!(recs[0]["restored"], serde_json::json!([]));
    assert_eq!(recs[1]["restored"], serde_json::json!([at("d/l")]));
    assert_eq!(recs[2]["restored"], serde_json::json!([at("d/sub/g")]));
    assert_eq!(recs[2]["removed"], serde_json::json!([at("e/g")]));
    assert_eq!(recs[3]["restored"], serde_json::json!([at("d/f")]));
    assert_eq!(
        fs::read(dir.join("d/f")).unwrap(),
        "x".repeat(3000).as_bytes()
    );
    assert!(dir.join("d/sub/g").exists() && !dir.join("e/g").exists());
    // An empty journal: no records.
    let (recs, out) = act(&dir, &["undo", "-J"]);
    assert!(recs.is_empty() && out.status.success());
    // A refusal is an error record naming the path.
    let (_, _) = act(&dir, &["rm", "--safe", "a"]);
    fs::write(dir.join("a"), "new\n").unwrap();
    let (recs, out) = act(&dir, &["undo", "-J"]);
    assert_eq!(briefs(&recs), [format!("error {}", at("a"))]);
    assert_eq!(out.status.code(), Some(1));

    // install-links: the directories it made, then each symlink.
    let dir = action_tree();
    let (recs, out) = act(&dir, &["install-links", "-J", "--prefix", "df-", "x/bin"]);
    assert!(out.status.success());
    assert_eq!(briefs(&recs[..2]), ["mkdir x", "mkdir x/bin"]);
    let links: Vec<&Value> = recs[2..].iter().collect();
    assert!(links.len() > 20);
    for l in links {
        assert_eq!(l["action"], "symlink");
        let dest = l["dest"].as_str().unwrap();
        assert!(dest.starts_with("x/bin/df-"), "{dest}");
        assert_eq!(
            fs::read_link(dir.join(dest)).unwrap().to_string_lossy(),
            l["source"].as_str().unwrap()
        );
    }
}
