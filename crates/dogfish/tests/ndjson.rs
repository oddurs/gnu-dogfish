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
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

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
            "bench",
            vec![c(&["bench", "-J", "-n", "1", "--size-mb", "1"])],
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
            let out = run_os(fixture(), &os_args(&args), b"");
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
        let produced = run_os(fixture(), &os_args(args), b"");
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
