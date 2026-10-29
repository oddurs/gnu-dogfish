//! `cargo xtask docs-gen`: the manual pages and shell completions, generated
//! by `dogfish --generate` from the tools' own option tables, written into a
//! `share/` tree and linted with whichever checkers are installed (groff,
//! mandoc, bash, zsh, fish). `cargo xtask dist` ships the same tree in the
//! release archive.
//!
//! Nothing generated is committed: it is rebuilt from the source each time.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where `docs-gen` writes, under the workspace root.
const OUT: &str = "target/docs-gen";

/// The installed name of each completion script, by `--generate` kind.
const COMPLETIONS: &[(&str, &str)] = &[
    ("bash", "share/bash-completion/completions/dogfish"),
    ("zsh", "share/zsh/site-functions/_dogfish"),
    ("fish", "share/fish/vendor_completions.d/dogfish.fish"),
];

/// Every generated file as `(path under the install prefix, contents)`,
/// sorted by path: `share/man/man1/dogfish.1`, `dogfish-TOOL.1` for each
/// tool, and the completions. `bin` is a dogfish binary that runs here;
/// `epoch` dates the pages (`SOURCE_DATE_EPOCH`).
pub fn generate(bin: &Path, epoch: u64) -> Result<Vec<(String, Vec<u8>)>, String> {
    let run = |args: &[&str]| {
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .env("SOURCE_DATE_EPOCH", epoch.to_string())
            .env("LC_ALL", "C");
        crate::output(&mut cmd)
    };
    let tools = run(&["--list"])?;
    let mut files = Vec::new();
    for tool in std::iter::once("dogfish").chain(tools.lines()) {
        let page = run(&["--generate", "man", tool])?;
        let name = if tool == "dogfish" {
            "dogfish.1".to_string()
        } else {
            format!("dogfish-{tool}.1")
        };
        files.push((format!("share/man/man1/{name}"), page.into_bytes()));
    }
    for (kind, path) in COMPLETIONS {
        files.push((path.to_string(), run(&["--generate", kind])?.into_bytes()));
    }
    files.sort();
    Ok(files)
}

/// Writes `files` under `dir` (emptied first).
fn write_tree(dir: &Path, files: &[(String, Vec<u8>)]) -> Result<(), String> {
    let _ = fs::remove_dir_all(dir);
    for (rel, data) in files {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        fs::write(&path, data).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

/// Whether `program` can be run (`program --version` starts).
fn installed(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Runs a checker; any output on stderr (or stdout, for mandoc) is a
/// failure, as is a non-zero status.
fn lint(what: &str, cmd: &mut Command) -> Result<(), String> {
    let out = cmd
        .output()
        .map_err(|e| format!("cannot run {:?}: {e}", cmd.get_program()))?;
    let noise = [out.stdout, out.stderr].concat();
    if !out.status.success() || !noise.is_empty() {
        return Err(format!(
            "{what}: {}\n{}",
            out.status,
            String::from_utf8_lossy(&noise).trim_end()
        ));
    }
    Ok(())
}

/// Checks the tree at `dir` with each checker that is installed; returns
/// the names of those that were not.
fn lint_tree(dir: &Path, files: &[(String, Vec<u8>)]) -> Result<Vec<&'static str>, String> {
    let mut missing = Vec::new();
    let pages: Vec<PathBuf> = files
        .iter()
        .filter(|(p, _)| p.ends_with(".1"))
        .map(|(p, _)| dir.join(p))
        .collect();
    if installed("groff") {
        for page in &pages {
            lint(
                &format!("groff -man -ww -z {}", page.display()),
                Command::new("groff").args(["-man", "-ww", "-z"]).arg(page),
            )?;
        }
    } else {
        missing.push("groff");
    }
    // mandoc has no --version; -V would do, but is not in older releases.
    if Command::new("mandoc").arg("-h").output().is_ok() {
        lint(
            "mandoc -Tlint -Wwarning",
            Command::new("mandoc")
                .args(["-Tlint", "-Wwarning"])
                .args(&pages),
        )?;
    } else {
        missing.push("mandoc");
    }
    let script = |kind: &str| {
        let rel = COMPLETIONS
            .iter()
            .find(|(k, _)| *k == kind)
            .map_or("", |(_, p)| p);
        dir.join(rel)
    };
    if installed("bash") {
        let bash = script("bash");
        lint("bash -n", Command::new("bash").arg("-n").arg(&bash))?;
        // It loads, and registers the multicall binary.
        let out = crate::output(Command::new("bash").arg("-c").arg(format!(
            "source '{}' && complete -p dogfish",
            bash.display()
        )))?;
        if !out.contains("-F _dogfish dogfish") {
            return Err(format!("bash: sourcing the completion registered {out:?}"));
        }
    } else {
        missing.push("bash");
    }
    for shell in ["zsh", "fish"] {
        if installed(shell) {
            lint(
                &format!("{shell} -n"),
                Command::new(shell).arg("-n").arg(script(shell)),
            )?;
        } else {
            missing.push(shell);
        }
    }
    Ok(missing)
}

/// `cargo xtask docs-gen`: builds dogfish, writes target/docs-gen/share and
/// lints it.
pub fn docs_gen(root: &Path) -> Result<(), String> {
    crate::run(
        Command::new(crate::cargo_bin())
            .args([
                "build",
                "--locked",
                "--package",
                "dogfish",
                "--bin",
                "dogfish",
            ])
            .current_dir(root),
    )?;
    let bin = root.join("target/debug/dogfish");
    let epoch = crate::dist::source_date_epoch(root)?;
    let files = generate(&bin, epoch)?;
    let dir = root.join(OUT);
    write_tree(&dir, &files)?;
    let missing = lint_tree(&dir, &files)?;
    eprintln!(
        "xtask: docs-gen: {} files in {}",
        files.len(),
        dir.join("share").display()
    );
    if !missing.is_empty() {
        eprintln!(
            "xtask: docs-gen: not installed, so not checked with: {}",
            missing.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completions_install_where_the_shells_look() {
        let paths: Vec<&str> = COMPLETIONS.iter().map(|(_, p)| *p).collect();
        assert_eq!(
            paths,
            [
                "share/bash-completion/completions/dogfish",
                "share/zsh/site-functions/_dogfish",
                "share/fish/vendor_completions.d/dogfish.fish",
            ]
        );
    }

    #[test]
    fn lint_fails_on_any_output() {
        assert!(lint("true", &mut Command::new("true")).is_ok());
        assert!(lint("false", &mut Command::new("false")).is_err());
        assert!(lint("echo", Command::new("echo").arg("warning")).is_err());
    }
}
