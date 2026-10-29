//! `dogfish`: the GNU/Dogfish multicall binary.

// Doc comments on clap structs are the `--help` text, where usage syntax such as
// `COMMAND [ARGS]...` is literal; escaping it for rustdoc would show in `--help`.
#![allow(rustdoc::broken_intra_doc_links)]

mod common;
mod hash;
mod meta;
mod tools;

use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use dogfish_core::{
    ColumnAlign, DogfishContext, Table, gnu_io_error,
    io::{install_sigbus_handler, install_sigpipe_handler},
    strip_os_error_suffix,
};

pub const ALL_TOOLS: &[(&str, &str, &str)] = &[
    // Tier 1: File & Directory
    (
        "ls",
        "File & Dir",
        "List directory contents with colors, --tree, and --json",
    ),
    (
        "cp",
        "File & Dir",
        "Copy files/dirs using zero-copy CoW (APFS clonefile)",
    ),
    (
        "mv",
        "File & Dir",
        "Move/rename files (recorded to UndoJournal)",
    ),
    (
        "rm",
        "File & Dir",
        "Remove files/dirs (supports --safe undoable trash)",
    ),
    ("mkdir", "File & Dir", "Create directories (-p, -m, -v)"),
    (
        "ln",
        "File & Dir",
        "Create hard or symbolic links (-s, -f, -v)",
    ),
    (
        "chmod",
        "File & Dir",
        "Change file mode bits (symbolic +x or octal 755, -R)",
    ),
    ("chown", "File & Dir", "Change file owner and group (-R)"),
    (
        "stat",
        "File & Dir",
        "Inspect file metadata with visual card or --json",
    ),
    (
        "touch",
        "File & Dir",
        "Update file access/modification timestamps",
    ),
    (
        "df",
        "File & Dir",
        "Report filesystem disk usage with sparkline bars",
    ),
    (
        "du",
        "File & Dir",
        "Parallel disk usage estimator with sparklines",
    ),
    (
        "realpath",
        "File & Dir",
        "Print resolved canonical physical path (-m, --json)",
    ),
    (
        "basename",
        "File & Dir",
        "Strip directory and suffix from filenames (-a, -s)",
    ),
    (
        "dirname",
        "File & Dir",
        "Strip last component from file name",
    ),
    (
        "mktemp",
        "File & Dir",
        "Create a temporary file or directory safely (-d, -p)",
    ),
    // Tier 2: Text & Streaming
    (
        "cat",
        "Text & Stream",
        "Zero-copy mmap file concatenator (-n, -b, -s, -E, -T)",
    ),
    (
        "tac",
        "Text & Stream",
        "Concatenate and print files in reverse (SIMD memrchr)",
    ),
    (
        "head",
        "Text & Stream",
        "First N lines/bytes or NDJSON objects (auto-table)",
    ),
    (
        "tail",
        "Text & Stream",
        "Last N lines/bytes (-f follow) or NDJSON objects",
    ),
    (
        "wc",
        "Text & Stream",
        "SIMD line/word/char/byte counter (--table, --json)",
    ),
    (
        "sort",
        "Text & Stream",
        "Parallel line & NDJSON sorter (--by <field>, --where)",
    ),
    (
        "uniq",
        "Text & Stream",
        "Deduplicate lines or NDJSON objects (--by <field>)",
    ),
    (
        "cut",
        "Text & Stream",
        "Slice bytes/chars/fields or NDJSON (-f, --where)",
    ),
    (
        "tr",
        "Text & Stream",
        "Translate, squeeze, or delete characters",
    ),
    (
        "paste",
        "Text & Stream",
        "Merge lines of files side-by-side or serially",
    ),
    ("nl", "Text & Stream", "Number lines of files"),
    (
        "shuf",
        "Text & Stream",
        "Fisher-Yates random shuffler for lines, ranges (-i), or NDJSON",
    ),
    (
        "query",
        "Text & Stream",
        "SQL-like NDJSON filter, sort & projection (-w, -s, -f, -n)",
    ),
    // Tier 3: Hashing & Encoding
    (
        "b2sum",
        "Hash & Encode",
        "Parallel BLAKE2b message digest & --check verifier",
    ),
    (
        "sha256sum",
        "Hash & Encode",
        "Parallel SHA-256 message digest & --check verifier",
    ),
    (
        "md5sum",
        "Hash & Encode",
        "Parallel MD5 message digest & --check verifier",
    ),
    ("base64", "Hash & Encode", "Base64 encoder and decoder"),
    (
        "basenc",
        "Hash & Encode",
        "Multi-radix encoder/decoder (base64url, base32, hex, bin)",
    ),
    // Tier 4: Shell & System
    ("echo", "Shell & Sys", "Display a line of text (-n, -e)"),
    ("printf", "Shell & Sys", "Format and print data"),
    (
        "env",
        "Shell & Sys",
        "Inspect (--table, --json) or run in modified environment",
    ),
    (
        "printenv",
        "Shell & Sys",
        "Print values of specified environment variables (--json)",
    ),
    (
        "id",
        "Shell & Sys",
        "Print user and group identity (--json)",
    ),
    (
        "uname",
        "Shell & Sys",
        "Print system kernel/machine information (--json)",
    ),
    ("hostname", "Shell & Sys", "Print system network hostname"),
    (
        "nproc",
        "Shell & Sys",
        "Print number of available processing units (--json)",
    ),
    ("whoami", "Shell & Sys", "Print effective username"),
    (
        "yes",
        "Shell & Sys",
        "64 KiB page-aligned high-throughput repeater",
    ),
    ("true", "Shell & Sys", "Exit with status 0"),
    ("false", "Shell & Sys", "Exit with status 1"),
    (
        "sleep",
        "Shell & Sys",
        "Suspend execution for human duration (500ms, 1m30s)",
    ),
    (
        "timeout",
        "Shell & Sys",
        "Run a command with a human time limit (e.g. timeout 2s cmd)",
    ),
    (
        "watch",
        "Shell & Sys",
        "Execute a program periodically with timing header (-n)",
    ),
    (
        "seq",
        "Shell & Sys",
        "Print fast buffered numeric sequences",
    ),
    (
        "date",
        "Shell & Sys",
        "Format or display dates (-u, -I, -R, -d, --json)",
    ),
    (
        "tee",
        "Shell & Sys",
        "Read from stdin and fan out to stdout + files",
    ),
    // Meta-Tools
    (
        "bench",
        "Meta-Tool",
        "Run head-to-head benchmarks against system utilities",
    ),
    (
        "undo",
        "Meta-Tool",
        "Undo the most recent rm --safe, mv, or cp operation",
    ),
    (
        "history",
        "Meta-Tool",
        "View the reversible file operation journal",
    ),
    (
        "init",
        "Meta-Tool",
        "Generate shell aliases (`eval \"$(dogfish init zsh --safe-rm)\"`)",
    ),
    (
        "install-links",
        "Meta-Tool",
        "Install BusyBox-style symlinks for all 40 utilities",
    ),
];

fn main() -> ExitCode {
    install_sigpipe_handler();
    let raw_args: Vec<String> = std::env::args().collect();

    // Determine if invoked via symlink (e.g. `ls`, `df-ls`) or as `dogfish <subcommand>`
    let argv0_name = raw_args
        .first()
        .and_then(|s| Path::new(s).file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "dogfish".to_string());

    let stripped_argv0 = argv0_name
        .strip_prefix("df-")
        .unwrap_or(&argv0_name)
        .to_string();

    let (tool_name, tool_args) = if !matches!(stripped_argv0.as_str(), "dogfish" | "gnu-dogfish")
        && is_known_tool(&stripped_argv0)
    {
        let mut a = raw_args;
        if !a.is_empty() {
            a[0] = stripped_argv0.clone();
        }
        (stripped_argv0, a)
    } else {
        match raw_args.get(1).map(|s| s.as_str()) {
            None | Some("-h") | Some("--help") | Some("help") => {
                print_banner_and_help();
                return ExitCode::SUCCESS;
            }
            Some("-V") | Some("--version") | Some("version") => {
                println!(
                    "GNU/Dogfish (gnu-dogfish) {} — Libre, Object-Aware Coreutils",
                    env!("CARGO_PKG_VERSION")
                );
                return ExitCode::SUCCESS;
            }
            Some("--list") | Some("list") => {
                for (name, _, _) in ALL_TOOLS {
                    println!("{}", name);
                }
                return ExitCode::SUCCESS;
            }
            Some(sub) => {
                let sub_clean = sub.strip_prefix("df-").unwrap_or(sub).to_string();
                let mut a = Vec::with_capacity(raw_args.len() - 1);
                a.push(sub_clean.clone());
                a.extend_from_slice(&raw_args[2..]);
                (sub_clean, a)
            }
        }
    };

    install_sigbus_handler(&tool_name);
    let (ctx, filtered_args) = DogfishContext::from_raw_args(&tool_name, tool_args);
    // Like every GNU utility (scripts probe `tool --version`), a lone `--version`
    // prints the version instead of being parsed as an operand or rejected.
    if filtered_args.len() == 2 && filtered_args[1] == "--version" {
        println!("{} (GNU/Dogfish) {}", tool_name, env!("CARGO_PKG_VERSION"));
        // GNU `false` fails even when asked for its version.
        return if tool_name == "false" {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        };
    }
    let res = dispatch(&tool_name, filtered_args, &ctx);
    ctx.finish();

    match res {
        Ok(code) => ExitCode::from(code as u8),
        Err(err) => {
            if let Some(clap_err) = err.downcast_ref::<clap::Error>() {
                use clap::error::ErrorKind;
                if matches!(
                    clap_err.kind(),
                    ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
                ) {
                    let _ = clap_err.print();
                    return ExitCode::SUCCESS;
                }
                // Usage errors read like glibc getopt's, then GNU's "Try" hint.
                match gnu_usage_message(clap_err) {
                    Some(msg) => eprintln!(
                        "{tool_name}: {msg}\nTry '{tool_name} --help' for more information."
                    ),
                    None => {
                        let _ = clap_err.print();
                    }
                }
                return ExitCode::from(usage_error_status(&tool_name));
            }
            // GNU style: `tool: message`, with io::Error rendered as bare strerror text.
            let msg = match err.downcast_ref::<std::io::Error>() {
                Some(io_err) => gnu_io_error(io_err),
                None => strip_os_error_suffix(&err.to_string()),
            };
            let prefix = format!("{}: ", tool_name);
            let msg = msg.strip_prefix(&prefix).unwrap_or(&msg);
            let st = ctx.stderr_style();
            eprintln!("{}: {}", st.bold(&tool_name), msg);
            ExitCode::from(1)
        }
    }
}

/// Exit status GNU uses for command-line usage errors: most tools exit 1, but
/// the ones whose own failures are reported with 1 reserve a distinct status.
fn usage_error_status(tool: &str) -> u8 {
    match tool {
        "ls" | "sort" | "printenv" => 2,
        "env" | "timeout" => 125,
        _ => 1,
    }
}

/// Translates a clap parse error into glibc `getopt_long` wording
/// (`invalid option -- 'x'`, `unrecognized option '--xyz'`, ...), or `None`
/// for kinds GNU has no generic message for.
fn gnu_usage_message(err: &clap::Error) -> Option<String> {
    use clap::error::{ContextKind, ContextValue, ErrorKind};
    let context = |kind| match err.get(kind) {
        Some(ContextValue::String(s)) => Some(s.as_str()),
        _ => None,
    };
    // clap names an option as it is declared (`--wrap <COLS>`); keep the flag.
    let declared =
        || context(ContextKind::InvalidArg).map(|a| a.split([' ', '=']).next().unwrap_or(a));
    match err.kind() {
        ErrorKind::UnknownArgument => {
            let arg = context(ContextKind::InvalidArg)?;
            Some(if arg.starts_with("--") {
                // clap drops any `=VALUE`; GNU echoes the argument as typed.
                let typed = std::env::args()
                    .find(|a| a.strip_prefix(arg).is_some_and(|v| v.starts_with('=')))
                    .unwrap_or_else(|| arg.to_string());
                format!("unrecognized option '{typed}'")
            } else if let Some(short) = arg.strip_prefix('-').and_then(|s| s.chars().next()) {
                format!("invalid option -- '{short}'")
            } else {
                format!("extra operand '{arg}'")
            })
        }
        ErrorKind::InvalidValue if context(ContextKind::InvalidValue) == Some("") => {
            // A missing option argument: GNU names the option the way it was typed,
            // which clap does not record, so look at the last option-like argument.
            let typed = std::env::args().rev().find(|a| a.starts_with('-'));
            Some(match typed {
                Some(t) if !t.starts_with("--") => {
                    let short = t.chars().last().unwrap_or('?');
                    format!("option requires an argument -- '{short}'")
                }
                _ => format!("option '{}' requires an argument", declared()?),
            })
        }
        ErrorKind::InvalidValue | ErrorKind::ValueValidation => Some(format!(
            "invalid argument '{}' for '{}'",
            context(ContextKind::InvalidValue)?,
            declared()?
        )),
        ErrorKind::TooManyValues => {
            let arg = declared()?;
            Some(if arg.starts_with('-') {
                format!("option '{arg}' doesn't allow an argument")
            } else {
                format!("extra operand '{}'", context(ContextKind::InvalidValue)?)
            })
        }
        ErrorKind::MissingRequiredArgument => Some("missing operand".to_string()),
        _ => None,
    }
}

fn is_known_tool(name: &str) -> bool {
    ALL_TOOLS.iter().any(|(n, _, _)| *n == name)
}

fn dispatch(tool: &str, args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let tool_names: Vec<&str> = ALL_TOOLS.iter().map(|(n, _, _)| *n).collect();
    match tool {
        // Tier 1: File & Directory
        "ls" => tools::ls::run_ls(args, ctx),
        "cp" => tools::cp::run_cp(args, ctx),
        "mv" => tools::mv::run_mv(args, ctx),
        "rm" => tools::rm::run_rm(args, ctx),
        "mkdir" => tools::mkdir::run_mkdir(args, ctx),
        "ln" => tools::ln::run_ln(args, ctx),
        "chmod" => tools::chmod::run_chmod(args, ctx),
        "chown" => tools::chown::run_chown(args, ctx),
        "stat" => tools::stat::run_stat(args, ctx),
        "touch" => tools::touch::run_touch(args, ctx),
        "df" => tools::df::run_df(args, ctx),
        "du" => tools::du::run_du(args, ctx),
        "realpath" => tools::realpath::run_realpath(args, ctx),
        "basename" => tools::basename::run_basename(args, ctx),
        "dirname" => tools::dirname::run_dirname(args, ctx),
        "mktemp" => tools::mktemp::run_mktemp(args, ctx),

        // Tier 2: Text & Streaming
        "cat" => tools::cat::run_cat(args, ctx),
        "tac" => tools::tac::run_tac(args, ctx),
        "head" => tools::head::run_head(args, ctx),
        "tail" => tools::tail::run_tail(args, ctx),
        "wc" => tools::wc::run_wc(args, ctx),
        "sort" => tools::sort::run_sort(args, ctx),
        "uniq" => tools::uniq::run_uniq(args, ctx),
        "cut" => tools::cut::run_cut(args, ctx),
        "tr" => tools::tr::run_tr(args, ctx),
        "paste" => tools::paste::run_paste(args, ctx),
        "nl" => tools::nl::run_nl(args, ctx),
        "shuf" => tools::shuf::run_shuf(args, ctx),
        "query" | "filter" => tools::query::run_query(args, ctx),

        // Tier 3: Hashing & Encoding
        "b2sum" => hash::run_b2sum(args, ctx),
        "sha256sum" => hash::run_sha256sum(args, ctx),
        "md5sum" => hash::run_md5sum(args, ctx),
        "base64" => hash::run_base64(args, ctx),
        "basenc" => hash::run_basenc(args, ctx),

        // Tier 4: Shell & System
        "echo" => tools::echo::run_echo(args, ctx),
        "printf" => tools::printf::run_printf(args, ctx),
        "env" => tools::env::run_env(args, ctx),
        "printenv" => tools::printenv::run_printenv(args, ctx),
        "id" => tools::id::run_id(args, ctx),
        "uname" => tools::uname::run_uname(args, ctx),
        "hostname" => tools::hostname::run_hostname(args, ctx),
        "nproc" => tools::nproc::run_nproc(args, ctx),
        "whoami" => tools::whoami::run_whoami(args, ctx),
        "yes" => tools::yes::run_yes(args, ctx),
        "true" => tools::true_false::run_true(args, ctx),
        "false" => tools::true_false::run_false(args, ctx),
        "sleep" => tools::sleep::run_sleep(args, ctx),
        "timeout" => tools::timeout::run_timeout(args, ctx),
        "watch" => tools::watch::run_watch(args, ctx),
        "seq" => tools::seq::run_seq(args, ctx),
        "date" => tools::date::run_date(args, ctx),
        "tee" => tools::tee::run_tee(args, ctx),

        // Meta-Tools
        "bench" => meta::run_bench(args, ctx),
        "undo" => meta::run_undo(args, ctx),
        "history" => meta::run_history(args, ctx),
        "init" => meta::run_init(args, &tool_names, ctx),
        "install-links" => meta::run_install_links(args, &tool_names, ctx),

        other => Err(anyhow::anyhow!(
            "unknown utility '{}'. Run `dogfish --help` to see all 44 utilities.",
            other
        )),
    }
}

fn print_banner_and_help() {
    let (ctx, _) = DogfishContext::from_raw_args("dogfish", vec![]);
    let st = ctx.style();
    let logo = r#"
             .              /\
           .' \            /  \     "I'd just like to interject for a moment.
          /   |   _..---.._ \  \     What you're referring to as Coreutils,
     |\  /    | .`  o   o  `.\  |    is in fact, GNU/Dogfish!"
     | \/     |/      _      \| /
     \        ;   _.-' '-._   ;/           __n__n__
      '.       \    / / \ \  /          .------`-\00/-'
        `._     `--' /   \_\           /  ##  ## (oo)
           `-.___..-'   / /           / \## __   ./
                       /_/               |//YY \|/"#;
    println!("{}", st.bold_cyan(logo));
    println!(
        "\n  {} {} — {}",
        st.bold_cyan("🦈 GNU/DOGFISH"),
        st.dim(env!("CARGO_PKG_VERSION")),
        st.bold("Libre, High-Velocity, Object-Aware Coreutils in Rust")
    );
    println!(
        "  {}",
        st.yellow(
            "Free Software Foundation Spirit • Four Essential Freedoms • Zero Proprietary Bloat"
        )
    );
    println!(
        "  {}\n",
        st.dim("Usage: gnu/dogfish <UTILITY> [ARGS]... [--stats] [--json|-J] [--color=auto|always|never]")
    );

    println!("  {}", st.bold_yellow("Killer Features:"));
    println!(
        "    • {}   `dogfish ls -J | dogfish sort --by size --desc | dogfish head -n 5`",
        st.bold_green("NDJSON Object Piping:")
    );
    println!(
        "    • {}       Pass `--stats` to any command for microsecond timing & I/O speed",
        st.bold_green("Global Telemetry:")
    );
    println!(
        "    • {}     `dogfish rm --safe <file>`, `mv`, `cp` -> rollback with `dogfish undo`",
        st.bold_green("Reversible File Ops:")
    );
    println!(
        "    • {}        Run `dogfish bench` to race Dogfish against system `/bin/*` utilities\n",
        st.bold_green("Live Benchmarks:")
    );

    let mut tbl = Table::new(vec![
        ("UTILITY", ColumnAlign::Left),
        ("CATEGORY", ColumnAlign::Left),
        ("DESCRIPTION", ColumnAlign::Left),
    ]);

    for (name, cat, desc) in ALL_TOOLS {
        tbl.add_row(vec![st.bold_cyan(name), st.yellow(cat), desc.to_string()]);
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = tbl.render(&mut out, st);
    println!();
}
