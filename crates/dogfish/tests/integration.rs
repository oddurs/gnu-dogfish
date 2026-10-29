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

    for flag in [
        "--base64",
        "--base64url",
        "--base32",
        "--base32hex",
        "--base16",
        "--base2msbf",
    ] {
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
    assert_eq!(stdout_str(&nl_out), "   1: alpha\n      \n   2: beta\n");
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
    let p_out = run(&[
        "paste",
        "-d",
        ":",
        f1.to_str().unwrap(),
        f2.to_str().unwrap(),
    ]);
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
    assert!(
        run(&["mkdir", "-p", nested.to_str().unwrap()])
            .status
            .success()
    );
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
    assert!(
        run(&["chmod", "+x", file_p.to_str().unwrap()])
            .status
            .success()
    );
    assert_ne!(
        fs::metadata(&file_p).unwrap().permissions().mode() & 0o111,
        0
    );

    assert!(
        run(&["chmod", "640", file_p.to_str().unwrap()])
            .status
            .success()
    );
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

// ===========================================================================
// SPRINT 2: ADVERSARIAL, DIFFERENTIAL & EDGE-CASE TESTS
// ===========================================================================

#[test]
fn test_differential_vs_system_utilities() {
    let tmp = tempfile::tempdir().unwrap();
    let sample = tmp.path().join("sample.txt");
    let text = "The GNU/Dogfish Project 1983-2026\nFree as in Free Swimming!\n\tTabbed\tLine\tHere\n1234567890\n";
    fs::write(&sample, text).unwrap();
    let s_path = sample.to_str().unwrap();

    // 1. Compare `wc -l`, `wc -w`, `wc -c` against `/usr/bin/wc`
    for flag in ["-l", "-w", "-c"] {
        let df = run(&["wc", flag, s_path]);
        let sys = Command::new("/usr/bin/wc")
            .args([flag, s_path])
            .output()
            .unwrap();
        let df_num: u64 = stdout_str(&df)
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let sys_num: u64 = String::from_utf8_lossy(&sys.stdout)
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(df_num, sys_num, "wc {} mismatch vs /usr/bin/wc", flag);
    }

    // 2. Compare `base64 -w 0` against `/usr/bin/base64`
    let df_b64 = run(&["base64", "-w", "0", s_path]);
    let sys_b64 = if cfg!(target_os = "macos") {
        Command::new("/usr/bin/base64")
            .args(["-i", s_path])
            .output()
            .unwrap()
    } else {
        Command::new("/usr/bin/base64")
            .args(["-w", "0", s_path])
            .output()
            .unwrap()
    };
    assert_eq!(
        stdout_str(&df_b64).trim(),
        String::from_utf8_lossy(&sys_b64.stdout)
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
    );

    // 3. Compare `md5sum` against `/sbin/md5 -q` (macOS) or `/usr/bin/md5sum` (Linux)
    if std::path::Path::new("/sbin/md5").exists() {
        let df_md5 = run(&["md5sum", s_path]);
        let sys_md5 = Command::new("/sbin/md5")
            .args(["-q", s_path])
            .output()
            .unwrap();
        let df_hex = stdout_str(&df_md5)
            .split_whitespace()
            .next()
            .unwrap()
            .to_string();
        let sys_hex = String::from_utf8_lossy(&sys_md5.stdout).trim().to_string();
        assert_eq!(df_hex, sys_hex);
    } else if std::path::Path::new("/usr/bin/md5sum").exists() {
        let df_md5 = run(&["md5sum", s_path]);
        let sys_md5 = Command::new("/usr/bin/md5sum")
            .args([s_path])
            .output()
            .unwrap();
        let df_hex = stdout_str(&df_md5)
            .split_whitespace()
            .next()
            .unwrap()
            .to_string();
        let sys_hex = String::from_utf8_lossy(&sys_md5.stdout)
            .split_whitespace()
            .next()
            .unwrap()
            .to_string();
        assert_eq!(df_hex, sys_hex);
    }

    // 4. Compare `sha256sum` against `/usr/bin/shasum -a 256` or `/usr/bin/sha256sum`
    if std::path::Path::new("/usr/bin/shasum").exists() {
        let df_sha = run(&["sha256sum", s_path]);
        let sys_sha = Command::new("/usr/bin/shasum")
            .args(["-a", "256", s_path])
            .output()
            .unwrap();
        assert_eq!(
            stdout_str(&df_sha).trim(),
            String::from_utf8_lossy(&sys_sha.stdout).trim()
        );
    } else if std::path::Path::new("/usr/bin/sha256sum").exists() {
        let df_sha = run(&["sha256sum", s_path]);
        let sys_sha = Command::new("/usr/bin/sha256sum")
            .args([s_path])
            .output()
            .unwrap();
        assert_eq!(
            stdout_str(&df_sha).trim(),
            String::from_utf8_lossy(&sys_sha.stdout).trim()
        );
    }
}

#[test]
fn test_empty_files_and_no_trailing_newline_edge_cases() {
    let tmp = tempfile::tempdir().unwrap();
    let empty_f = tmp.path().join("empty.txt");
    let no_nl_f = tmp.path().join("no_nl.txt");
    fs::write(&empty_f, b"").unwrap();
    fs::write(&no_nl_f, b"single line without newline").unwrap();

    let e_str = empty_f.to_str().unwrap();
    let n_str = no_nl_f.to_str().unwrap();

    // Empty file checks
    assert_eq!(stdout_str(&run(&["cat", e_str])), "");
    assert_eq!(stdout_str(&run(&["tac", e_str])), "");
    assert_eq!(stdout_str(&run(&["head", e_str])), "");
    assert_eq!(stdout_str(&run(&["tail", e_str])), "");
    assert_eq!(stdout_str(&run(&["sort", e_str])), "");
    assert_eq!(stdout_str(&run(&["uniq", e_str])), "");
    assert_eq!(stdout_str(&run(&["nl", e_str])), "");

    let wc_empty: serde_json::Value =
        serde_json::from_slice(&run(&["wc", "-J", e_str]).stdout).unwrap();
    assert_eq!(wc_empty["lines"].as_u64(), Some(0));
    assert_eq!(wc_empty["words"].as_u64(), Some(0));
    assert_eq!(wc_empty["bytes"].as_u64(), Some(0));

    // SHA-256 of empty file must be e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
    let sha_empty = stdout_str(&run(&["sha256sum", e_str]));
    assert!(
        sha_empty.starts_with("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
    );

    // No trailing newline checks
    let wc_no_nl: serde_json::Value =
        serde_json::from_slice(&run(&["wc", "-J", n_str]).stdout).unwrap();
    assert_eq!(wc_no_nl["lines"].as_u64(), Some(0));
    assert_eq!(wc_no_nl["words"].as_u64(), Some(4));
    assert_eq!(wc_no_nl["bytes"].as_u64(), Some(27));
    assert_eq!(wc_no_nl["max_line_length"].as_u64(), Some(27));

    assert_eq!(
        stdout_str(&run(&["head", "-n", "1", n_str])),
        "single line without newline"
    );
    assert_eq!(
        stdout_str(&run(&["tail", "-n", "1", n_str])),
        "single line without newline"
    );
}

#[test]
fn test_large_multi_chunk_tail_and_head() {
    // Generate >200 KiB (>3 x 64 KiB chunks) to exercise multi-chunk reverse SeekFrom::End in `tail`
    let tmp = tempfile::tempdir().unwrap();
    let big = tmp.path().join("big.log");
    let mut content = String::with_capacity(250_000);
    for i in 1..=5000 {
        content.push_str(&format!(
            "2026-10-24T00:00:00Z event_index={:05} payload=0123456789abcdef\n",
            i
        ));
    }
    fs::write(&big, &content).unwrap();
    let b_str = big.to_str().unwrap();

    // Verify `tail -n 2500` spans multiple 64 KiB chunks accurately
    let t_out = run(&["tail", "-n", "2500", b_str]);
    let t_text = stdout_str(&t_out);
    let t_lines: Vec<&str> = t_text.lines().collect();
    assert_eq!(t_lines.len(), 2500);
    assert!(t_lines[0].contains("event_index=02501"));
    assert!(t_lines[2499].contains("event_index=05000"));

    // Verify `head -n 2500` spans multiple 64 KiB chunks accurately
    let h_out = run(&["head", "-n", "2500", b_str]);
    let h_text = stdout_str(&h_out);
    let h_lines: Vec<&str> = h_text.lines().collect();
    assert_eq!(h_lines.len(), 2500);
    assert!(h_lines[0].contains("event_index=00001"));
    assert!(h_lines[2499].contains("event_index=02500"));
}

#[test]
fn test_unicode_multibyte_cut_wc_and_tr() {
    let unicode_text = "🦈🦀🦬\nαβγδε\n";
    let wc_j: serde_json::Value =
        serde_json::from_slice(&run_stdin(&["wc", "-J"], unicode_text.as_bytes()).stdout).unwrap();
    assert_eq!(wc_j["lines"].as_u64(), Some(2));
    assert_eq!(wc_j["chars"].as_u64(), Some(10)); // 3 emojis + \n + 5 greek + \n = 10 chars
    assert_eq!(wc_j["bytes"].as_u64(), Some(24)); // 12 + 1 + 10 + 1 = 24 bytes

    // Like GNU coreutils, `cut -c` selects bytes, so it can split a character.
    let cut_c = run_stdin(&["cut", "-c", "2-3"], unicode_text.as_bytes());
    assert_eq!(cut_c.stdout, b"\x9f\xa6\n\xb1\xce\n");
    let cut_c = run_stdin(&["cut", "-c", "5-12"], unicode_text.as_bytes());
    assert_eq!(stdout_str(&cut_c), "🦀🦬\nγδε\n");
}

#[test]
fn test_chmod_complex_symbolic_clauses_and_recursive() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("chmod_tree");
    let sub = dir.join("sub");
    fs::create_dir_all(&sub).unwrap();
    let f = sub.join("file.txt");
    fs::write(&f, "test").unwrap();

    // Set initial mode 0600
    assert!(run(&["chmod", "600", f.to_str().unwrap()]).status.success());
    // Apply compound symbolic mode: u+x,g+r,o+r -> 0744
    assert!(
        run(&["chmod", "u+x,g+r,o+r", f.to_str().unwrap()])
            .status
            .success()
    );
    assert_eq!(
        fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o744
    );

    // Apply =rw to all: a=rw -> 0666
    assert!(
        run(&["chmod", "a=rw", f.to_str().unwrap()])
            .status
            .success()
    );
    assert_eq!(
        fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o666
    );

    // Apply go-w -> 0644
    assert!(
        run(&["chmod", "go-w", f.to_str().unwrap()])
            .status
            .success()
    );
    assert_eq!(
        fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o644
    );

    // Recursive chmod -R 755 on directory
    assert!(
        run(&["chmod", "-R", "755", dir.to_str().unwrap()])
            .status
            .success()
    );
    assert_eq!(
        fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
fn test_error_handling_and_exit_codes() {
    // Non-existent files should return non-zero exit codes
    assert!(
        !run(&["cat", "/nonexistent/dogfish_missing_file"])
            .status
            .success()
    );
    assert!(
        !run(&["ls", "/nonexistent/dogfish_missing_dir"])
            .status
            .success()
    );
    assert!(
        !run(&["stat", "/nonexistent/dogfish_missing_file"])
            .status
            .success()
    );
    assert!(
        !run(&["rm", "/nonexistent/dogfish_missing_file"])
            .status
            .success()
    );
    // But `rm -f` on nonexistent file must succeed (exit 0)
    assert!(
        run(&["rm", "-f", "/nonexistent/dogfish_missing_file"])
            .status
            .success()
    );

    // `seq` with 0 step must fail
    assert!(!run(&["seq", "1", "0", "10"]).status.success());

    // Unknown subcommand must fail with exit 1
    assert!(!run(&["definitely_not_a_utility"]).status.success());
}

#[test]
fn test_global_stats_and_color_flags_on_every_category() {
    // `--stats` should emit `[⚡ gnu/dogfish ...]` on stderr without polluting stdout
    let out = run(&["echo", "--stats", "--no-color", "clean_stdout"]);
    assert!(out.status.success());
    assert_eq!(stdout_str(&out), "clean_stdout\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("gnu/dogfish echo"));
}

#[test]
fn test_all_57_tools_respond_to_help_cleanly() {
    let list_out = run(&["--list"]);
    assert!(list_out.status.success());
    let tools: Vec<String> = stdout_str(&list_out)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    assert_eq!(tools.len(), 57);

    for tool in &tools {
        if matches!(
            tool.as_str(),
            "true" | "false" | "echo" | "printf" | "hostname"
        ) {
            continue;
        }
        let h_out = run(&[tool, "--help"]);
        assert!(
            h_out.status.success(),
            "tool '{}' failed on --help with status {:?}",
            tool,
            h_out.status
        );
        assert!(
            !stdout_str(&h_out).is_empty(),
            "tool '{}' produced empty --help output",
            tool
        );
    }
}
