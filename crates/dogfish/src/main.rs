mod fs_ops;
mod hash;
mod meta;
mod sys;
mod text;

use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use dogfish_core::{
    ColumnAlign, DogfishContext, Table,
    io::install_sigpipe_handler,
};

pub const ALL_TOOLS: &[(&str, &str, &str)] = &[
    // Tier 1: File & Directory
    ("ls", "File & Dir", "List directory contents with colors, --tree, and --json"),
    ("cp", "File & Dir", "Copy files/dirs using zero-copy CoW (APFS clonefile)"),
    ("mv", "File & Dir", "Move/rename files (recorded to UndoJournal)"),
    ("rm", "File & Dir", "Remove files/dirs (supports --safe undoable trash)"),
    ("mkdir", "File & Dir", "Create directories (-p, -m, -v)"),
    ("ln", "File & Dir", "Create hard or symbolic links (-s, -f, -v)"),
    ("chmod", "File & Dir", "Change file mode bits (symbolic +x or octal 755, -R)"),
    ("chown", "File & Dir", "Change file owner and group (-R)"),
    ("stat", "File & Dir", "Inspect file metadata with visual card or --json"),
    ("touch", "File & Dir", "Update file access/modification timestamps"),
    ("df", "File & Dir", "Report filesystem disk usage with sparkline bars"),
    ("du", "File & Dir", "Parallel disk usage estimator with sparklines"),
    // Tier 2: Text & Streaming
    ("cat", "Text & Stream", "Zero-copy mmap file concatenator (-n, -b, -s, -E, -T)"),
    ("head", "Text & Stream", "First N lines/bytes or NDJSON objects (auto-table)"),
    ("tail", "Text & Stream", "Last N lines/bytes (-f follow) or NDJSON objects"),
    ("wc", "Text & Stream", "SIMD line/word/char/byte counter (--table, --json)"),
    ("sort", "Text & Stream", "Parallel line & NDJSON field sorter (--by <field>)"),
    ("uniq", "Text & Stream", "Deduplicate lines or NDJSON objects (--by <field>)"),
    ("cut", "Text & Stream", "Slice bytes/chars/fields or project NDJSON fields (-f)"),
    ("tr", "Text & Stream", "Translate, squeeze, or delete characters"),
    ("paste", "Text & Stream", "Merge lines of files side-by-side or serially"),
    ("nl", "Text & Stream", "Number lines of files"),
    // Tier 3: Hashing & Encoding
    ("b2sum", "Hash & Encode", "Parallel BLAKE2b message digest & --check verifier"),
    ("sha256sum", "Hash & Encode", "Parallel SHA-256 message digest & --check verifier"),
    ("md5sum", "Hash & Encode", "Parallel MD5 message digest & --check verifier"),
    ("base64", "Hash & Encode", "Base64 encoder and decoder"),
    ("basenc", "Hash & Encode", "Multi-radix encoder/decoder (base64url, base32, hex, bin)"),
    // Tier 4: Shell & System
    ("echo", "Shell & Sys", "Display a line of text (-n, -e)"),
    ("printf", "Shell & Sys", "Format and print data"),
    ("env", "Shell & Sys", "Inspect (--table, --json) or run in modified environment"),
    ("id", "Shell & Sys", "Print user and group identity (--json)"),
    ("uname", "Shell & Sys", "Print system kernel/machine information (--json)"),
    ("whoami", "Shell & Sys", "Print effective username"),
    ("yes", "Shell & Sys", "64 KiB page-aligned high-throughput repeater"),
    ("true", "Shell & Sys", "Exit with status 0"),
    ("false", "Shell & Sys", "Exit with status 1"),
    ("sleep", "Shell & Sys", "Suspend execution for human duration (500ms, 1m30s)"),
    ("seq", "Shell & Sys", "Print fast buffered numeric sequences"),
    ("date", "Shell & Sys", "Format or display dates (-u, -I, -R, -d, --json)"),
    ("tee", "Shell & Sys", "Read from stdin and fan out to stdout + files"),
    // Meta-Tools
    ("bench", "Meta-Tool", "Run head-to-head benchmarks against system utilities"),
    ("undo", "Meta-Tool", "Undo the most recent rm --safe, mv, or cp operation"),
    ("history", "Meta-Tool", "View the reversible file operation journal"),
    ("init", "Meta-Tool", "Generate shell aliases (`eval \"$(dogfish init zsh --safe-rm)\"`)"),
    ("install-links", "Meta-Tool", "Install BusyBox-style symlinks for all 40 utilities"),
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

    let (tool_name, tool_args) = if stripped_argv0 != "dogfish" && is_known_tool(&stripped_argv0) {
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
                println!("dogfish {} (modern Rust coreutils)", env!("CARGO_PKG_VERSION"));
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

    let (ctx, filtered_args) = DogfishContext::from_raw_args(&tool_name, tool_args);
    let res = dispatch(&tool_name, filtered_args, &ctx);
    ctx.finish();

    match res {
        Ok(code) => ExitCode::from(code as u8),
        Err(err) => {
            // If clap printed --help or --version, exit 0
            if let Some(clap_err) = err.downcast_ref::<clap::Error>() {
                let _ = clap_err.print();
                return match clap_err.kind() {
                    clap::error::ErrorKind::DisplayHelp
                    | clap::error::ErrorKind::DisplayVersion => ExitCode::SUCCESS,
                    _ => ExitCode::from(1),
                };
            }
            let st = ctx.stderr_style();
            eprintln!("{} {}: {}", st.bold_red("error:"), st.bold(&tool_name), err);
            ExitCode::from(1)
        }
    }
}

fn is_known_tool(name: &str) -> bool {
    ALL_TOOLS.iter().any(|(n, _, _)| *n == name)
}

fn dispatch(tool: &str, args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let tool_names: Vec<&str> = ALL_TOOLS.iter().map(|(n, _, _)| *n).collect();
    match tool {
        // Tier 1: File & Directory
        "ls" => fs_ops::run_ls(args, ctx),
        "cp" => fs_ops::run_cp(args, ctx),
        "mv" => fs_ops::run_mv(args, ctx),
        "rm" => fs_ops::run_rm(args, ctx),
        "mkdir" => fs_ops::run_mkdir(args, ctx),
        "ln" => fs_ops::run_ln(args, ctx),
        "chmod" => fs_ops::run_chmod(args, ctx),
        "chown" => fs_ops::run_chown(args, ctx),
        "stat" => fs_ops::run_stat(args, ctx),
        "touch" => fs_ops::run_touch(args, ctx),
        "df" => fs_ops::run_df(args, ctx),
        "du" => fs_ops::run_du(args, ctx),

        // Tier 2: Text & Streaming
        "cat" => text::run_cat(args, ctx),
        "head" => text::run_head(args, ctx),
        "tail" => text::run_tail(args, ctx),
        "wc" => text::run_wc(args, ctx),
        "sort" => text::run_sort(args, ctx),
        "uniq" => text::run_uniq(args, ctx),
        "cut" => text::run_cut(args, ctx),
        "tr" => text::run_tr(args, ctx),
        "paste" => text::run_paste(args, ctx),
        "nl" => text::run_nl(args, ctx),

        // Tier 3: Hashing & Encoding
        "b2sum" => hash::run_b2sum(args, ctx),
        "sha256sum" => hash::run_sha256sum(args, ctx),
        "md5sum" => hash::run_md5sum(args, ctx),
        "base64" => hash::run_base64(args, ctx),
        "basenc" => hash::run_basenc(args, ctx),

        // Tier 4: Shell & System
        "echo" => sys::run_echo(args, ctx),
        "printf" => sys::run_printf(args, ctx),
        "env" => sys::run_env(args, ctx),
        "id" => sys::run_id(args, ctx),
        "uname" => sys::run_uname(args, ctx),
        "whoami" => sys::run_whoami(args, ctx),
        "yes" => sys::run_yes(args, ctx),
        "true" => sys::run_true(args, ctx),
        "false" => sys::run_false(args, ctx),
        "sleep" => sys::run_sleep(args, ctx),
        "seq" => sys::run_seq(args, ctx),
        "date" => sys::run_date(args, ctx),
        "tee" => sys::run_tee(args, ctx),

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
        st.yellow("Free Software Foundation Spirit • Four Essential Freedoms • Zero Proprietary Bloat")
    );
    println!(
        "  {}\n",
        st.dim("Usage: dogfish <UTILITY> [ARGS]... [--stats] [--json|-J] [--color=auto|always|never]")
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
        tbl.add_row(vec![
            st.bold_cyan(name),
            st.yellow(cat),
            desc.to_string(),
        ]);
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = tbl.render(&mut out, st);
    println!();
}
