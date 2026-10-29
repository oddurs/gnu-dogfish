//! `dogfish`: the GNU/Dogfish multicall binary.

// The C `main` below is the entry point: Rust's own start-up code would
// reopen a closed standard output on /dev/null, hiding `>&-` from the tools.
#![cfg_attr(not(test), no_main)]

mod common;
mod tools;

use std::ffi::OsString;
use std::path::Path;

use anyhow::Result;
use dogfish_core::{
    ColumnAlign, DogfishContext, Table, gnu_io_error,
    io::{install_sigbus_handler, install_sigpipe_handler},
    outln, stdout, strip_os_error_suffix,
};

use crate::common::getopt::exit_failure;

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

/// The process entry point, as C sees it. Before anything else, standard
/// input, output and error are made safe: a descriptor the shell closed
/// (`>&-`) is taken by `/dev/null`, so that no file the tool opens becomes
/// its standard output, and a closed standard output makes every write
/// fail, as in GNU.
///
/// # Safety
///
/// Called by the C runtime only, with `argc` valid strings in `argv`.
#[cfg(not(test))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn main(argc: libc::c_int, argv: *const *const libc::c_char) -> libc::c_int {
    use std::os::unix::ffi::OsStringExt;
    for fd in 0..3 {
        // SAFETY: fcntl(F_GETFD) only queries the descriptor.
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
            // SAFETY: a NUL-terminated path; the lowest free descriptor is `fd`.
            unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR) };
            if fd == libc::STDOUT_FILENO {
                stdout::started_closed();
            }
        }
    }
    let args = (0..usize::try_from(argc).unwrap_or(0))
        .filter_map(|i| {
            // SAFETY: the C runtime passes `argc` valid NUL-terminated strings.
            let arg = unsafe { *argv.add(i) };
            (!arg.is_null()).then(|| {
                // SAFETY: a non-null argv entry is a NUL-terminated string
                // that lives for the whole process.
                OsString::from_vec(unsafe { std::ffi::CStr::from_ptr(arg) }.to_bytes().to_vec())
            })
        })
        .collect();
    libc::c_int::from(run(args))
}

/// Runs the multicall binary on `raw_args` (`argv`); returns the exit status.
#[cfg_attr(test, allow(dead_code))]
fn run(raw_args: Vec<OsString>) -> u8 {
    install_sigpipe_handler();

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
            a[0] = OsString::from(&stripped_argv0);
        }
        (stripped_argv0, a)
    } else {
        let sub = raw_args.get(1).map(|s| s.to_string_lossy().into_owned());
        match sub.as_deref() {
            None | Some("-h") | Some("--help") | Some("help") => {
                print_banner_and_help();
                return finish("dogfish", 0);
            }
            Some("-V") | Some("--version") | Some("version") => {
                outln!(
                    "GNU/Dogfish (gnu-dogfish) {} — Libre, Object-Aware Coreutils",
                    env!("CARGO_PKG_VERSION")
                );
                return finish("dogfish", 0);
            }
            Some("--list") | Some("list") => {
                for (name, _, _) in ALL_TOOLS {
                    outln!("{}", name);
                }
                return finish("dogfish", 0);
            }
            Some(sub) => {
                let sub_clean = sub.strip_prefix("df-").unwrap_or(sub).to_string();
                let mut a = Vec::with_capacity(raw_args.len() - 1);
                a.push(OsString::from(&sub_clean));
                a.extend_from_slice(&raw_args[2..]);
                (sub_clean, a)
            }
        }
    };

    install_sigbus_handler(&tool_name);
    let ctx = DogfishContext::new(&tool_name);
    // Each tool parses its own arguments, Dogfish's switches included.
    common::getopt::install_switches(
        common::getopt::switches_for(&tool_name),
        ctx.switches.clone(),
    );
    let res = dispatch(&tool_name, tool_args, &ctx);
    ctx.finish();

    let code = match res {
        Ok(code) => code,
        Err(err) => {
            match err.downcast_ref::<std::io::Error>() {
                // A failed write to standard output is reported as GNU's
                // `close_stdout` does, once.
                Some(io_err) if stdout::is_write_error(io_err) => {
                    stdout::mark_reported();
                    report_write_error(&tool_name, io_err);
                    exit_failure(&tool_name)
                }
                _ => {
                    // GNU style: `tool: message`, with io::Error rendered as bare strerror text.
                    let msg = match err.downcast_ref::<std::io::Error>() {
                        Some(io_err) => gnu_io_error(io_err),
                        None => strip_os_error_suffix(&err.to_string()),
                    };
                    let prefix = format!("{}: ", tool_name);
                    let msg = msg.strip_prefix(&prefix).unwrap_or(&msg);
                    let st = ctx.stderr_style();
                    eprintln!("{}: {}", st.bold(&tool_name), msg);
                    1
                }
            }
        }
    };
    finish(&tool_name, code)
}

/// Flushes standard output and exits with `code`, or, when output was lost to
/// a write error nobody has reported, reports it and fails (gnulib's
/// `close_stdout`).
fn finish(tool: &str, code: i32) -> u8 {
    let code = match stdout::close() {
        Some(err) => {
            report_write_error(tool, &err);
            exit_failure(tool)
        }
        None => code,
    };
    code as u8
}

/// GNU's diagnostic for output lost to a write error: `tool: write error:
/// No space left on device`. The tools whose output GNU flushes as it goes
/// find the error before exit, when `errno` no longer says why, and print
/// just `write error`.
fn report_write_error(tool: &str, err: &std::io::Error) {
    match tool {
        "wc" | "md5sum" | "sha256sum" | "b2sum" | "du" | "sort" => {
            eprintln!("{tool}: write error")
        }
        _ => eprintln!("{tool}: write error: {}", gnu_io_error(err)),
    }
}

fn is_known_tool(name: &str) -> bool {
    ALL_TOOLS.iter().any(|(n, _, _)| *n == name)
}

fn dispatch(tool: &str, args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
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
        "b2sum" => tools::checksum::run_b2sum(args, ctx),
        "sha256sum" => tools::checksum::run_sha256sum(args, ctx),
        "md5sum" => tools::checksum::run_md5sum(args, ctx),
        "base64" => tools::basenc::run_base64(args, ctx),
        "basenc" => tools::basenc::run_basenc(args, ctx),

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
        "bench" => tools::bench::run_bench(args, ctx),
        "undo" => tools::undo::run_undo(args, ctx),
        "history" => tools::history::run_history(args, ctx),
        "init" => tools::init::run_init(args, &tool_names, ctx),
        "install-links" => tools::install_links::run_install_links(args, &tool_names, ctx),

        other => Err(anyhow::anyhow!(
            "unknown utility '{}'. Run `dogfish --help` to see all 44 utilities.",
            other
        )),
    }
}

fn print_banner_and_help() {
    let ctx = DogfishContext::new("dogfish");
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
    outln!("{}", st.bold_cyan(logo));
    outln!(
        "\n  {} {} — {}",
        st.bold_cyan("🦈 GNU/DOGFISH"),
        st.dim(env!("CARGO_PKG_VERSION")),
        st.bold("Libre, High-Velocity, Object-Aware Coreutils in Rust")
    );
    outln!(
        "  {}",
        st.yellow(
            "Free Software Foundation Spirit • Four Essential Freedoms • Zero Proprietary Bloat"
        )
    );
    outln!(
        "  {}\n",
        st.dim("Usage: gnu/dogfish <UTILITY> [ARGS]... [--stats] [--json|-J] [--color=auto|always|never]")
    );

    outln!("  {}", st.bold_yellow("Killer Features:"));
    outln!(
        "    • {}   `dogfish ls -J | dogfish sort --by size --desc | dogfish head -n 5`",
        st.bold_green("NDJSON Object Piping:")
    );
    outln!(
        "    • {}       Pass `--stats` to any command for microsecond timing & I/O speed",
        st.bold_green("Global Telemetry:")
    );
    outln!(
        "    • {}     `dogfish rm --safe <file>`, `mv`, `cp` -> rollback with `dogfish undo`",
        st.bold_green("Reversible File Ops:")
    );
    outln!(
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

    let _ = tbl.render(&mut stdout::stdout(), st);
    outln!();
}
