//! init — Outputs shell integration script (`eval "$(dogfish init zsh)"`)

use anyhow::Result;
use clap::Parser;
use dogfish_core::DogfishContext;

#[derive(Parser)]
#[command(
    name = "init",
    about = "Output shell initialization script (eval \"$(dogfish init zsh)\")"
)]
struct InitArgs {
    /// Target shell: zsh, bash, or fish
    #[arg(default_value = "zsh")]
    shell: String,

    /// Alias `rm` to `dogfish rm --safe` so deletions are undoable by default
    #[arg(long = "safe-rm")]
    safe_rm: bool,
}

pub fn run_init(args: Vec<String>, tool_names: &[&str], _ctx: &DogfishContext) -> Result<i32> {
    let parsed = InitArgs::try_parse_from(args)?;
    let exe = std::env::current_exe()?;
    let exe_str = exe.display();
    let is_fish = parsed.shell.eq_ignore_ascii_case("fish");

    println!("# Dogfish shell integration for {}", parsed.shell);
    if is_fish {
        println!("# Usage in fish: ./target/release/dogfish init fish --safe-rm | source");
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
                println!(
                    "function rm --wraps=rm; \"{}\" rm --safe $argv; end",
                    exe_str
                );
            } else {
                println!(
                    "function {0} --wraps={0}; \"{1}\" {0} $argv; end",
                    tool, exe_str
                );
            }
        }
        println!("function undo; \"{}\" undo $argv; end", exe_str);
        println!("function dogfish; \"{}\" $argv; end", exe_str);
        println!("function gnu-dogfish; \"{}\" $argv; end", exe_str);
        println!("function 'gnu/dogfish'; \"{}\" $argv; end", exe_str);
    } else {
        println!(
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
                println!("alias rm='\"{}\" rm --safe'", exe_str);
            } else {
                println!("alias {}='\"{}\" {}'", tool, exe_str, tool);
            }
        }
        println!("alias undo='\"{}\" undo'", exe_str);
        println!("alias dogfish='\"{}\"'", exe_str);
        println!("alias gnu-dogfish='\"{}\"'", exe_str);
        println!("alias 'gnu/dogfish'='\"{}\"'", exe_str);
    }
    Ok(0)
}
