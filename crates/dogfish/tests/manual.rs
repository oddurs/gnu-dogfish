//! The generated manual pages and completions (`dogfish --generate`) against
//! the parsers they describe: every fixed set of option arguments they offer
//! is exactly what the tool accepts, as its own "Valid arguments are:"
//! diagnostic lists it; every tool answers `--help` and `--version`; the
//! scripts load in the shells that are installed.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn dogfish(args: &[&str], dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dogfish"))
        .args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .env_remove("POSIXLY_CORRECT")
        .env_remove("VERSION_CONTROL")
        .stdin(Stdio::null())
        .output()
        .expect("run dogfish")
}

fn stdout(args: &[&str]) -> String {
    let out = dogfish(args, Path::new("."));
    assert!(out.status.success(), "dogfish {args:?}: {:?}", out);
    String::from_utf8(out.stdout).expect("UTF-8 output")
}

fn tools() -> Vec<String> {
    stdout(&["--list"]).lines().map(str::to_string).collect()
}

/// `(spelling, argument, words)` for each option of `tool`.
fn options(tool: &str) -> Vec<(String, String, Vec<String>)> {
    stdout(&["--generate", "options", tool])
        .lines()
        .map(|l| {
            let mut f = l.split('\t');
            let spelling = f.next().unwrap_or_default().to_string();
            let arg = f.next().unwrap_or_default().to_string();
            let words = f
                .next()
                .unwrap_or_default()
                .split(',')
                .filter(|w| !w.is_empty())
                .map(str::to_string)
                .collect();
            (spelling, arg, words)
        })
        .collect()
}

/// The words of a gnulib `argmatch_valid` list in `stderr`, if it has one.
/// ls's `--time-style` list also names `[posix-]STYLE` and `+FORMAT`; the
/// completions offer the plain styles.
fn valid_arguments(stderr: &str) -> Option<BTreeSet<String>> {
    let (_, list) = stderr.split_once("Valid arguments are:\n")?;
    Some(
        list.lines()
            .take_while(|l| l.starts_with("  - "))
            .filter(|l| !l[4..].starts_with('+'))
            .flat_map(|l| l[4..].split(", "))
            .map(|w| w.trim_matches(['\'', '`', '\u{2018}', '\u{2019}']))
            .map(|w| w.strip_prefix("[posix-]").unwrap_or(w).to_string())
            .collect(),
    )
}

/// What else a command line needs before the tool checks an option's
/// argument (it checks some only once the option is in use).
fn context(tool: &str, spelling: &str) -> &'static [&'static str] {
    match (tool, spelling) {
        ("ls", "--time-style") => &["-l"],
        ("du", "--time-style") => &["--time"],
        _ => &[],
    }
}

#[test]
fn fixed_option_arguments_are_what_the_parser_accepts() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut checked = 0;
    for tool in tools() {
        for (spelling, _, words) in options(&tool) {
            if words.is_empty() || !spelling.starts_with("--") {
                continue;
            }
            let mut args: Vec<String> = vec![tool.clone()];
            args.extend(context(&tool, &spelling).iter().map(|s| s.to_string()));
            let bogus = format!("{spelling}=@bogus@");
            let mut bad = args.clone();
            bad.push(bogus.clone());
            let bad: Vec<&str> = bad.iter().map(String::as_str).collect();
            let out = dogfish(&bad, dir.path());
            assert!(!out.status.success(), "{tool} {bogus} was accepted");
            let stderr = String::from_utf8_lossy(&out.stderr);
            let declared: BTreeSet<String> = words.iter().cloned().collect();
            match valid_arguments(&stderr) {
                Some(valid) => {
                    assert_eq!(declared, valid, "{tool} {spelling}: manual vs parser");
                }
                // No list in the diagnostic: each word must be accepted.
                None => {
                    for w in &words {
                        let mut good = args.clone();
                        good.push(format!("{spelling}={w}"));
                        good.push("--help".into());
                        let good: Vec<&str> = good.iter().map(String::as_str).collect();
                        let out = dogfish(&good, dir.path());
                        assert!(
                            out.status.success(),
                            "{tool} {spelling}={w}: {}",
                            String::from_utf8_lossy(&out.stderr)
                        );
                    }
                }
            }
            checked += 1;
        }
    }
    assert!(checked > 30, "only {checked} options with fixed arguments");
}

#[test]
fn every_tool_answers_help_and_version() {
    let dir = tempfile::tempdir().expect("temp dir");
    for tool in tools() {
        let opts = options(&tool);
        for spelling in ["--help", "--version"] {
            assert!(
                opts.iter().any(|(s, _, _)| s == spelling),
                "{tool}: no {spelling} in its options"
            );
            let out = dogfish(&[&tool, spelling], dir.path());
            let expected = if tool == "false" { 1 } else { 0 };
            assert_eq!(out.status.code(), Some(expected), "{tool} {spelling}");
            assert!(!out.stdout.is_empty(), "{tool} {spelling}: no output");
        }
    }
}

#[test]
fn every_tool_has_a_manual_page() {
    for tool in tools().iter().map(String::as_str).chain(["dogfish"]) {
        let page = stdout(&["--generate", "man", tool]);
        assert!(page.starts_with(".\\\" Generated by"), "{tool}");
        assert!(
            page.contains(".SH OPTIONS\n") || tool == "dogfish",
            "{tool}"
        );
    }
    let out = dogfish(&["--generate", "man", "nonesuch"], Path::new("."));
    assert_eq!(out.status.code(), Some(2));
}

/// Runs `shell` with `args` if it is installed; `None` if it is not.
fn shell(shell: &str, args: &[&str]) -> Option<Output> {
    Command::new(shell)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()
}

#[test]
fn completion_scripts_load() {
    let dir = tempfile::tempdir().expect("temp dir");
    let write = |name: &str, kind: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, stdout(&["--generate", kind])).expect("write script");
        path.to_string_lossy().into_owned()
    };
    let bash = write("dogfish.bash", "bash");
    let zsh = write("_dogfish", "zsh");
    let fish = write("dogfish.fish", "fish");

    if let Some(out) = shell(
        "bash",
        &["-c", &format!("source '{bash}' && complete -p dogfish")],
    ) {
        assert!(out.status.success(), "{out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "complete -o bashdefault -o default -F _dogfish dogfish\n"
        );
        // Completing an option's fixed words, as bash splits `--color=`.
        let script = format!(
            "source '{bash}'; COMP_WORDS=(dogfish ls --sort = ti); COMP_CWORD=4; \
             _dogfish; echo \"${{COMPREPLY[*]}}\""
        );
        let out = shell("bash", &["-c", &script]).expect("bash ran before");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "time\n");
    }
    for (sh, file) in [("zsh", &zsh), ("fish", &fish)] {
        if let Some(out) = shell(sh, &["-n", file]) {
            assert!(out.status.success(), "{sh} -n: {out:?}");
        }
    }
}
