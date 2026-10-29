//! init — Outputs shell integration script (`eval "$(dogfish init zsh)"`)

use std::ffi::OsString;

use anyhow::Result;
use dogfish_core::{DogfishContext, outln};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy_all, print_help, print_version, usage_error,
};
use crate::common::quote::quote_locale;

const INIT_HELP: &str = "\
Output shell initialization script (eval \"$(dogfish init zsh)\")

Usage: init [OPTIONS] [SHELL]

Arguments:
  [SHELL]  Target shell: zsh, bash, or fish [default: zsh]

Options:
      --safe-rm  Alias `rm` to `dogfish rm --safe` so deletions are undoable by default
  -h, --help     Print help
";

struct InitArgs {
    shell: String,
    safe_rm: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InitOpt {
    SafeRm,
    Help,
    Version,
}

const SPECS: &[OptSpec<InitOpt>] = &[
    OptSpec::long("safe-rm", HasArg::No, InitOpt::SafeRm),
    OptSpec::both(b'h', "help", HasArg::No, InitOpt::Help),
    OptSpec::long("version", HasArg::No, InitOpt::Version),
];

/// Parses the command line; `Err` is the exit status after `--help`,
/// `--version` or a usage diagnostic.
fn parse_init_args(args: Vec<OsString>) -> std::result::Result<InitArgs, i32> {
    let mut p = InitArgs {
        shell: "zsh".to_string(),
        safe_rm: false,
    };
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    while let Some(opt) = opts.next() {
        match opt.map_err(|e| getopt_error("init", &e))?.0 {
            InitOpt::SafeRm => p.safe_rm = true,
            InitOpt::Help => return Err(print_help(INIT_HELP)),
            InitOpt::Version => return Err(print_version("init")),
        }
    }
    let mut operands = lossy_all(opts.operands()).into_iter();
    if let Some(shell) = operands.next() {
        p.shell = shell;
    }
    if let Some(extra) = operands.next() {
        return Err(usage_error(
            "init",
            format!("extra operand {}", quote_locale(&extra)),
        ));
    }
    Ok(p)
}

pub fn run_init(args: Vec<OsString>, tool_names: &[&str], _ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_init_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let exe = std::env::current_exe()?;
    let exe_str = exe.display();
    let is_fish = parsed.shell.eq_ignore_ascii_case("fish");

    outln!("# Dogfish shell integration for {}", parsed.shell);
    if is_fish {
        outln!("# Usage in fish: ./target/release/dogfish init fish --safe-rm | source");
        for &tool in tool_names {
            // In fish, `true`, `false`, `echo`, and `printf` are core builtins that shouldn't be clobbered
            // unless desired, and `alias` syntax or `function --wraps` is used.
            if matches!(
                tool,
                "install-links" | "init" | "bench" | "undo" | "history" | "true" | "false"
            ) {
                continue;
            }
            if tool == "rm" && parsed.safe_rm {
                outln!(
                    "function rm --wraps=rm; \"{}\" rm --safe $argv; end",
                    exe_str
                );
            } else {
                outln!(
                    "function {0} --wraps={0}; \"{1}\" {0} $argv; end",
                    tool,
                    exe_str
                );
            }
        }
        outln!("function undo; \"{}\" undo $argv; end", exe_str);
        outln!("function dogfish; \"{}\" $argv; end", exe_str);
        outln!("function gnu-dogfish; \"{}\" $argv; end", exe_str);
        outln!("function 'gnu/dogfish'; \"{}\" $argv; end", exe_str);
    } else {
        outln!(
            "# Usage in zsh/bash: eval \"$(./target/release/dogfish init {} --safe-rm)\"",
            parsed.shell
        );
        for &tool in tool_names {
            if matches!(
                tool,
                "install-links" | "init" | "bench" | "undo" | "history" | "true" | "false"
            ) {
                continue;
            }
            if tool == "rm" && parsed.safe_rm {
                outln!("alias rm='\"{}\" rm --safe'", exe_str);
            } else {
                outln!("alias {}='\"{}\" {}'", tool, exe_str, tool);
            }
        }
        outln!("alias undo='\"{}\" undo'", exe_str);
        outln!("alias dogfish='\"{}\"'", exe_str);
        outln!("alias gnu-dogfish='\"{}\"'", exe_str);
        outln!("alias 'gnu/dogfish'='\"{}\"'", exe_str);
    }
    Ok(0)
}

/// init's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("init", INIT_HELP, SPECS)
}
