use std::fs;
use std::process::Command;

fn dogfish_bin() -> String {
    env!("CARGO_BIN_EXE_dogfish").to_string()
}

#[test]
fn test_ndjson_pipeline_ls_sort_cut_head() {
    let bin = dogfish_bin();
    // 1. Run `dogfish ls -J` on src
    let ls_out = Command::new(&bin)
        .args(["ls", "-J", "src"])
        .output()
        .expect("ls -J failed");
    assert!(ls_out.status.success());

    // 2. Pipe into `dogfish sort --by size --desc -J`
    let mut sort_child = Command::new(&bin)
        .args(["sort", "--by", "size", "--desc", "-J"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("sort failed");
    {
        use std::io::Write;
        sort_child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(&ls_out.stdout)
            .unwrap();
    }
    let sort_out = sort_child.wait_with_output().unwrap();
    assert!(sort_out.status.success());

    // 3. Pipe into `dogfish cut -f name,size -J`
    let mut cut_child = Command::new(&bin)
        .args(["cut", "-f", "name,size", "-J"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("cut failed");
    {
        use std::io::Write;
        cut_child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(&sort_out.stdout)
            .unwrap();
    }
    let cut_out = cut_child.wait_with_output().unwrap();
    assert!(cut_out.status.success());

    let lines: Vec<&str> = std::str::from_utf8(&cut_out.stdout)
        .unwrap()
        .lines()
        .collect();
    assert!(!lines.is_empty());
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert!(first.get("name").is_some());
    assert!(first.get("size").is_some());
    assert!(first.get("mode").is_none()); // Projected away by `cut -f name,size`
}

#[test]
fn test_reversible_rm_safe_and_undo() {
    let bin = dogfish_bin();
    let tmp = tempfile::tempdir().unwrap();
    let home_dir = tmp.path().join("dogfish_home");
    let test_file = tmp.path().join("important.txt");
    fs::write(&test_file, "secret payload 123").unwrap();
    assert!(test_file.exists());

    // Remove with `rm --safe`
    let rm_status = Command::new(&bin)
        .env("DOGFISH_HOME", &home_dir)
        .args(["rm", "--safe", test_file.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(rm_status.success());
    assert!(!test_file.exists());

    // Restore with `dogfish undo`
    let undo_status = Command::new(&bin)
        .env("DOGFISH_HOME", &home_dir)
        .args(["undo"])
        .status()
        .unwrap();
    assert!(undo_status.success());
    assert!(test_file.exists());
    assert_eq!(fs::read_to_string(&test_file).unwrap(), "secret payload 123");
}

#[test]
fn test_hash_and_check_roundtrip() {
    let bin = dogfish_bin();
    let tmp = tempfile::tempdir().unwrap();
    let f1 = tmp.path().join("a.txt");
    let f2 = tmp.path().join("b.txt");
    let sums = tmp.path().join("SHA256SUMS");
    fs::write(&f1, "hello dogfish\n").unwrap();
    fs::write(&f2, "rust coreutils\n").unwrap();

    let out = Command::new(&bin)
        .args(["sha256sum", f1.to_str().unwrap(), f2.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());
    fs::write(&sums, &out.stdout).unwrap();

    let check = Command::new(&bin)
        .args(["sha256sum", "-c", sums.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(check.status.success());
}

#[test]
fn test_base64_and_basenc_roundtrip() {
    let bin = dogfish_bin();
    for flag in ["--base64url", "--base32", "--base16", "--base2msbf"] {
        let mut enc = Command::new(&bin)
            .args(["basenc", flag, "-w", "0"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        {
            use std::io::Write;
            enc.stdin
                .as_mut()
                .unwrap()
                .write_all(b"dogfish-coreutils-2026")
                .unwrap();
        }
        let encoded = enc.wait_with_output().unwrap();
        assert!(encoded.status.success());

        let mut dec = Command::new(&bin)
            .args(["basenc", flag, "-d"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        {
            use std::io::Write;
            dec.stdin
                .as_mut()
                .unwrap()
                .write_all(&encoded.stdout)
                .unwrap();
        }
        let decoded = dec.wait_with_output().unwrap();
        assert!(decoded.status.success());
        assert_eq!(&decoded.stdout, b"dogfish-coreutils-2026");
    }
}

#[test]
fn test_symlink_multicall_dispatch() {
    let bin = dogfish_bin();
    let tmp = tempfile::tempdir().unwrap();
    let status = Command::new(&bin)
        .args(["install-links", tmp.path().to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());

    // Invoke via symlink `wc` directly!
    let wc_link = tmp.path().join("wc");
    assert!(wc_link.exists());
    let out = Command::new(&wc_link)
        .args(["-l", "Cargo.toml"])
        .output()
        .unwrap();
    assert!(out.status.success());
}
