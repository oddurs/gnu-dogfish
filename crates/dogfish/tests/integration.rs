use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output, Stdio};

fn bin() -> String {
    env!("CARGO_BIN_EXE_dogfish").to_string()
}

fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("failed to execute dogfish")
}

fn run_stdin(args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(bin())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn dogfish");
    child.stdin.as_mut().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn stdout_str(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

// ===========================================================================
// TIER 4: SHELL & SYSTEM UTILITIES (18 tools)
// ===========================================================================

#[test]
fn test_true_and_false_exit_codes() {
    assert!(run(&["true"]).status.success());
    assert_eq!(run(&["false"]).status.code(), Some(1));
}

#[test]
fn test_echo_flags_and_escapes() {
    let out = run(&["echo", "hello", "dogfish"]);
    assert_eq!(stdout_str(&out), "hello dogfish\n");

    let out_n = run(&["echo", "-n", "no-newline"]);
    assert_eq!(stdout_str(&out_n), "no-newline");

    let out_e = run(&["echo", "-e", "line1\\nline2\\t\\x41"]);
    assert_eq!(stdout_str(&out_e), "line1\nline2\tA\n");

    let out_c = run(&["echo", "-e", "stop\\chere"]);
    assert_eq!(stdout_str(&out_c), "stop");
}

#[test]
fn test_printf_formatting_and_repeating_spec() {
    let out = run(&["printf", "%s=%04d (0x%x)\n", "id", "42", "255"]);
    assert_eq!(stdout_str(&out), "id=0042 (0xff)\n");

    let out_float = run(&["printf", "%.2f", "3.14159"]);
    assert_eq!(stdout_str(&out_float), "3.14");

    // Format string re-applied to remaining args
    let out_rep = run(&["printf", "[%s]", "a", "b", "c"]);
    assert_eq!(stdout_str(&out_rep), "[a][b][c]");
}

#[test]
fn test_seq_integer_float_padded_and_json() {
    let out = run(&["seq", "1", "2", "7"]);
    assert_eq!(stdout_str(&out), "1\n3\n5\n7\n");

    let out_w = run(&["seq", "-w", "-s", ",", "8", "10"]);
    assert_eq!(stdout_str(&out_w), "08,09,10\n");

    let out_desc = run(&["seq", "5", "-2", "1"]);
    assert_eq!(stdout_str(&out_desc), "5\n3\n1\n");

    let out_json = run(&["seq", "-J", "1", "3"]);
    let arr: Vec<i64> = serde_json::from_slice(&out_json.stdout).unwrap();
    assert_eq!(arr, vec![1, 2, 3]);
}

#[test]
fn test_sleep_human_durations() {
    let out = run(&["sleep", "5ms"]);
    assert!(out.status.success());
}

#[test]
fn test_whoami_id_uname_hostname_nproc() {
    let who = run(&["whoami"]);
    assert!(who.status.success());
    assert!(!stdout_str(&who).trim().is_empty());

    let who_j = run(&["whoami", "-J"]);
    let v: serde_json::Value = serde_json::from_slice(&who_j.stdout).unwrap();
    assert!(v.get("uid").is_some());

    let id_u = run(&["id", "-u"]);
    assert!(stdout_str(&id_u).trim().parse::<u32>().is_ok());

    let id_j = run(&["id", "-J"]);
    let id_v: serde_json::Value = serde_json::from_slice(&id_j.stdout).unwrap();
    assert!(id_v.get("groups").unwrap().is_array());

    let un = run(&["uname", "-s", "-m"]);
    assert!(un.status.success());
    assert!(stdout_str(&un).split_whitespace().count() >= 2);

    let hn = run(&["hostname"]);
    assert!(hn.status.success());
    assert!(!stdout_str(&hn).trim().is_empty());

    let np = run(&["nproc", "-J"]);
    let np_v: serde_json::Value = serde_json::from_slice(&np.stdout).unwrap();
    assert!(np_v["available_cpus"].as_u64().unwrap() >= 1);
}

#[test]
fn test_env_and_printenv() {
    let out = run(&["env", "-i", "FOO=bar", "BAZ=qux"]);
    assert_eq!(stdout_str(&out), "BAZ=qux\nFOO=bar\n");

    let p_out = Command::new(bin())
        .env("DOGFISH_SECRET_VAR", "4242")
        .args(["printenv", "DOGFISH_SECRET_VAR"])
        .output()
        .unwrap();
    assert_eq!(stdout_str(&p_out), "4242\n");
}

#[test]
fn test_date_epoch_iso_and_json() {
    let out = run(&["date", "-u", "-d", "@0", "+%Y-%m-%d %H:%M:%S"]);
    assert_eq!(stdout_str(&out).trim(), "1970-01-01 00:00:00");

    let out_j = run(&["date", "-d", "@1700000000", "-J"]);
    let v: serde_json::Value = serde_json::from_slice(&out_j.stdout).unwrap();
    assert_eq!(v["unix"].as_i64(), Some(1700000000));
}

#[test]
fn test_tee_multi_file_and_append() {
    let tmp = tempfile::tempdir().unwrap();
    let f1 = tmp.path().join("t1.txt");
    let f2 = tmp.path().join("t2.txt");

    let out = run_stdin(
        &["tee", f1.to_str().unwrap(), f2.to_str().unwrap()],
        b"first\n",
    );
    assert_eq!(stdout_str(&out), "first\n");
    assert_eq!(fs::read_to_string(&f1).unwrap(), "first\n");
    assert_eq!(fs::read_to_string(&f2).unwrap(), "first\n");

    let _ = run_stdin(&["tee", "-a", f1.to_str().unwrap()], b"second\n");
    assert_eq!(fs::read_to_string(&f1).unwrap(), "first\nsecond\n");
}

#[test]
fn test_timeout_and_watch() {
    // Command finishes within timeout -> exit 0
    let ok = run(&["timeout", "2s", bin().as_str(), "true"]);
    assert_eq!(ok.status.code(), Some(0));

    // Command exceeds timeout -> exit 124
    let killed = run(&["timeout", "30ms", bin().as_str(), "sleep", "2s"]);
    assert_eq!(killed.status.code(), Some(124));

    // Watch with -c 1 iteration
    let w = run(&["watch", "-n", "5ms", "-c", "1", "echo watched_ok"]);
    assert!(w.status.success());
    assert!(stdout_str(&w).contains("watched_ok"));
}

#[test]
fn test_yes_piped_to_head_sigpipe_safety() {
    let mut yes_child = Command::new(bin())
        .args(["yes", "libre"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let yes_stdout = yes_child.stdout.take().unwrap();

    let head_out = Command::new(bin())
        .args(["head", "-n", "4"])
        .stdin(yes_stdout)
        .output()
        .unwrap();
    let _ = yes_child.wait();
    assert_eq!(stdout_str(&head_out), "libre\nlibre\nlibre\nlibre\n");
}

// ===========================================================================
// TIER 3: HASHING & ENCODING UTILITIES (5 tools)
// ===========================================================================

#[test]
fn test_b2sum_sha256sum_md5sum_and_check_verifier() {
    let tmp = tempfile::tempdir().unwrap();
    let f1 = tmp.path().join("alpha.txt");
    let f2 = tmp.path().join("beta.txt");
    fs::write(&f1, "hello dogfish\n").unwrap();
    fs::write(&f2, "rust coreutils\n").unwrap();

    for tool in ["b2sum", "sha256sum", "md5sum"] {
        let sums_path = tmp.path().join(format!("{}.sums", tool));
        let out = run(&[tool, f1.to_str().unwrap(), f2.to_str().unwrap()]);
        assert!(out.status.success());
        fs::write(&sums_path, &out.stdout).unwrap();

        let check = run(&[tool, "-c", sums_path.to_str().unwrap()]);
        assert!(check.status.success());
        assert!(stdout_str(&check).contains("OK"));

        // BSD --tag format roundtrip
        let tag_out = run(&[tool, "--tag", f1.to_str().unwrap()]);
        fs::write(&sums_path, &tag_out.stdout).unwrap();
        let tag_check = run(&[tool, "-c", sums_path.to_str().unwrap()]);
        assert!(tag_check.status.success());
    }

    // Corrupt file -> verify check returns exit code 1
    let sums_path = tmp.path().join("sha256sum.sums");
    let out = run(&["sha256sum", f1.to_str().unwrap()]);
    fs::write(&sums_path, &out.stdout).unwrap();
    fs::write(&f1, "tampered content\n").unwrap();
    let bad_check = run(&["sha256sum", "-c", sums_path.to_str().unwrap()]);
    assert_eq!(bad_check.status.code(), Some(1));
}

#[test]
fn test_base64_and_basenc_all_radixes() {
    let payload = b"The quick brown dogfish jumps over the lazy proprietary OS!";

    let b64_enc = run_stdin(&["base64", "-w", "20"], payload);
    assert!(b64_enc.status.success());
    let b64_dec = run_stdin(&["base64", "-d"], &b64_enc.stdout);
    assert_eq!(b64_dec.stdout, payload);

    for flag in ["--base64", "--base64url", "--base32", "--base32hex", "--base16", "--base2msbf"] {
        let enc = run_stdin(&["basenc", flag, "-w", "0"], payload);
        assert!(enc.status.success());
        let dec = run_stdin(&["basenc", flag, "-d"], &enc.stdout);
        assert!(dec.status.success());
        assert_eq!(dec.stdout, payload, "failed roundtrip for {}", flag);
    }
}

// ===========================================================================
// TIER 2: TEXT & STREAMING UTILITIES + NDJSON PIPELINES (13 tools)
// ===========================================================================

#[test]
fn test_cat_flags_and_nl() {
    let input = b"first\n\n\nsecond\titem\n";
    let out = run_stdin(&["cat", "-b", "-s", "-E", "-T"], input);
    let s = stdout_str(&out);
    assert!(s.contains("     1\tfirst$"));
    assert!(s.contains("     2\tsecond^Iitem$"));

    let nl_out = run_stdin(&["nl", "-w", "4", "-s", ": "], b"alpha\n\nbeta\n");
    assert_eq!(stdout_str(&nl_out), "   1: alpha\n\n   2: beta\n");
}

#[test]
fn test_tac_and_head_tail_lines_and_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("lines.txt");
    let content: String = (1..=100).map(|i| format!("line-{:03}\n", i)).collect();
    fs::write(&p, &content).unwrap();

    let h = run(&["head", "-n", "3", p.to_str().unwrap()]);
    assert_eq!(stdout_str(&h), "line-001\nline-002\nline-003\n");

    let h_bytes = run(&["head", "-c", "8", p.to_str().unwrap()]);
    assert_eq!(stdout_str(&h_bytes), "line-001");

    let t = run(&["tail", "-n", "3", p.to_str().unwrap()]);
    assert_eq!(stdout_str(&t), "line-098\nline-099\nline-100\n");

    let t_bytes = run(&["tail", "-c", "9", p.to_str().unwrap()]);
    assert_eq!(stdout_str(&t_bytes), "line-100\n");

    let tac_out = run_stdin(&["tac"], b"1\n2\n3\n");
    assert_eq!(stdout_str(&tac_out), "3\n2\n1\n");
}

#[test]
fn test_wc_simd_counts_json_and_table() {
    let input = b"one two three\nfour five\n";
    let j_out = run_stdin(&["wc", "-J"], input);
    let obj: serde_json::Value = serde_json::from_slice(&j_out.stdout).unwrap();
    assert_eq!(obj["lines"].as_u64(), Some(2));
    assert_eq!(obj["words"].as_u64(), Some(5));
    assert_eq!(obj["bytes"].as_u64(), Some(24));
    assert_eq!(obj["max_line_length"].as_u64(), Some(13));

    let tbl_out = run_stdin(&["wc", "--table"], input);
    assert!(tbl_out.status.success());
    assert!(stdout_str(&tbl_out).contains("PROPORTION"));
}

#[test]
fn test_sort_text_numeric_human_key_and_uniq() {
    let num_out = run_stdin(&["sort", "-n"], b"100\n2\n25\n1\n");
    assert_eq!(stdout_str(&num_out), "1\n2\n25\n100\n");

    let hum_out = run_stdin(&["sort", "-h", "-r"], b"10K\n5G\n200M\n500B\n");
    assert_eq!(stdout_str(&hum_out), "5G\n200M\n10K\n500B\n");

    let key_out = run_stdin(&["sort", "-t", ":", "-k", "2", "-n"], b"b:20\na:5\nc:10\n");
    assert_eq!(stdout_str(&key_out), "a:5\nc:10\nb:20\n");

    let u_out = run_stdin(&["uniq", "-c"], b"x\nx\nx\ny\nz\nz\n");
    assert_eq!(stdout_str(&u_out), "      3 x\n      1 y\n      2 z\n");

    let d_out = run_stdin(&["uniq", "-d"], b"x\nx\ny\nz\nz\n");
    assert_eq!(stdout_str(&d_out), "x\nz\n");
}

#[test]
fn test_cut_tr_paste_shuf() {
    // cut fields & ranges
    let c_out = run_stdin(
        &["cut", "-d", ",", "-f", "1,3", "--output-delimiter", "|"],
        b"a,b,c,d\n1,2,3,4\n",
    );
    assert_eq!(stdout_str(&c_out), "a|c\n1|3\n");

    // tr character classes, delete & squeeze
    let tr_up = run_stdin(&["tr", "[:lower:]", "[:upper:]"], b"dogfish\n");
    assert_eq!(stdout_str(&tr_up), "DOGFISH\n");

    let tr_sq = run_stdin(&["tr", "-s", " "], b"a    b   c\n");
    assert_eq!(stdout_str(&tr_sq), "a b c\n");

    let tr_del = run_stdin(&["tr", "-d", "0-9"], b"ab12cd34\n");
    assert_eq!(stdout_str(&tr_del), "abcd\n");

    // paste parallel & serial
    let tmp = tempfile::tempdir().unwrap();
    let f1 = tmp.path().join("p1");
    let f2 = tmp.path().join("p2");
    fs::write(&f1, "a\nb\n").unwrap();
    fs::write(&f2, "1\n2\n").unwrap();
    let p_out = run(&["paste", "-d", ":", f1.to_str().unwrap(), f2.to_str().unwrap()]);
    assert_eq!(stdout_str(&p_out), "a:1\nb:2\n");

    // shuf range
    let shuf_out = run(&["shuf", "-i", "1-10", "-n", "5"]);
    assert_eq!(stdout_str(&shuf_out).lines().count(), 5);
}

#[test]
fn test_ndjson_pipeline_and_query_engine() {
    let ls_out = run(&["ls", "-J", "src"]);
    assert!(ls_out.status.success());

    let q_out = run_stdin(
        &[
            "query",
            "-w",
            "size>1K",
            "-w",
            "kind=file",
            "-s",
            "size",
            "-d",
            "-f",
            "name,size",
            "-n",
            "3",
            "-J",
        ],
        &ls_out.stdout,
    );
    assert!(q_out.status.success());
    let rows: Vec<serde_json::Value> = stdout_str(&q_out)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rows.len(), 3);
    assert!(rows[0]["size"].as_u64().unwrap() >= rows[1]["size"].as_u64().unwrap());
    assert!(rows[0].get("mode").is_none());
}

// ===========================================================================
// TIER 1: FILE & DIRECTORY UTILITIES + REVERSIBLE UNDO JOURNAL (16 tools)
// ===========================================================================

#[test]
fn test_mkdir_touch_ln_chmod_stat_realpath_basename_dirname_mktemp() {
    let tmp = tempfile::tempdir().unwrap();
    let nested = tmp.path().join("a/b/c");

    // mkdir -p
    assert!(run(&["mkdir", "-p", nested.to_str().unwrap()]).status.success());
    assert!(nested.is_dir());

    // touch & ln -s
    let file_p = nested.join("script.sh");
    assert!(run(&["touch", file_p.to_str().unwrap()]).status.success());
    assert!(file_p.exists());

    let link_p = tmp.path().join("link.sh");
    assert!(
        run(&[
            "ln",
            "-s",
            file_p.to_str().unwrap(),
            link_p.to_str().unwrap()
        ])
        .status
        .success()
    );
    assert!(link_p.symlink_metadata().unwrap().file_type().is_symlink());

    // chmod symbolic (+x) and octal (640)
    assert!(run(&["chmod", "+x", file_p.to_str().unwrap()]).status.success());
    assert_ne!(fs::metadata(&file_p).unwrap().permissions().mode() & 0o111, 0);

    assert!(run(&["chmod", "640", file_p.to_str().unwrap()]).status.success());
    assert_eq!(
        fs::metadata(&file_p).unwrap().permissions().mode() & 0o777,
        0o640
    );

    // stat -c and -J
    let st_fmt = run(&["stat", "-c", "%a %F", file_p.to_str().unwrap()]);
    assert_eq!(stdout_str(&st_fmt).trim(), "0640 regular file");

    // realpath resolves symlink to canonical file
    let rp = run(&["realpath", link_p.to_str().unwrap()]);
    assert_eq!(
        fs::canonicalize(stdout_str(&rp).trim()).unwrap(),
        fs::canonicalize(&file_p).unwrap()
    );

    // basename & dirname
    assert_eq!(
        stdout_str(&run(&["basename", "/foo/bar/baz.txt", ".txt"])).trim(),
        "baz"
    );
    assert_eq!(
        stdout_str(&run(&["dirname", "/foo/bar/baz.txt"])).trim(),
        "/foo/bar"
    );

    // mktemp -d
    let mkt = run(&["mktemp", "-d", "-p", tmp.path().to_str().unwrap()]);
    let created_dir = stdout_str(&mkt).trim().to_string();
    assert!(std::path::Path::new(&created_dir).is_dir());
}

#[test]
fn test_cp_cow_mv_rm_safe_and_multi_step_undo() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("df_home");
    let orig = tmp.path().join("orig.txt");
    let copied = tmp.path().join("copied.txt");
    let moved = tmp.path().join("moved.txt");
    fs::write(&orig, "libre data 12345").unwrap();

    // 1. `cp` (records CopyCreate in journal)
    let st1 = Command::new(bin())
        .env("DOGFISH_HOME", &home)
        .args(["cp", orig.to_str().unwrap(), copied.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(st1.success());
    assert_eq!(fs::read_to_string(&copied).unwrap(), "libre data 12345");

    // 2. `mv` (records Move in journal)
    let st2 = Command::new(bin())
        .env("DOGFISH_HOME", &home)
        .args(["mv", copied.to_str().unwrap(), moved.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(st2.success());
    assert!(!copied.exists());
    assert!(moved.exists());

    // 3. `rm --safe` (records TrashRemove in journal)
    let st3 = Command::new(bin())
        .env("DOGFISH_HOME", &home)
        .args(["rm", "--safe", moved.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(st3.success());
    assert!(!moved.exists());

    // Check `history -J` has 3 entries
    let hist = Command::new(bin())
        .env("DOGFISH_HOME", &home)
        .args(["history", "-J"])
        .output()
        .unwrap();
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&hist.stdout).unwrap();
    assert_eq!(entries.len(), 3);

    // Undo #3 (`rm --safe`) -> `moved.txt` is back!
    Command::new(bin())
        .env("DOGFISH_HOME", &home)
        .args(["undo"])
        .status()
        .unwrap();
    assert!(moved.exists());

    // Undo #2 (`mv`) -> `copied.txt` is back and `moved.txt` is gone!
    Command::new(bin())
        .env("DOGFISH_HOME", &home)
        .args(["undo"])
        .status()
        .unwrap();
    assert!(copied.exists());
    assert!(!moved.exists());

    // Undo #1 (`cp`) -> `copied.txt` is removed!
    Command::new(bin())
        .env("DOGFISH_HOME", &home)
        .args(["undo"])
        .status()
        .unwrap();
    assert!(!copied.exists());
    assert!(orig.exists());
}

#[test]
fn test_df_du_ls_tree_and_install_links() {
    let df_out = run(&["df", "-J"]);
    assert!(df_out.status.success());

    let du_out = run(&["du", "-s", "-J", "src"]);
    let du_obj: serde_json::Value = serde_json::from_slice(&du_out.stdout).unwrap();
    assert!(du_obj["size"].as_u64().unwrap() > 0);

    let tree_out = run(&["ls", "--tree", "-L", "2", "src"]);
    assert!(stdout_str(&tree_out).contains("main.rs"));

    // Symlink installation and argv[0] dispatch
    let tmp = tempfile::tempdir().unwrap();
    assert!(
        run(&["install-links", tmp.path().to_str().unwrap()])
            .status
            .success()
    );
    let linked_basename = tmp.path().join("basename");
    let b_out = Command::new(&linked_basename)
        .args(["/a/b/dogfish"])
        .output()
        .unwrap();
    assert_eq!(stdout_str(&b_out).trim(), "dogfish");
}
