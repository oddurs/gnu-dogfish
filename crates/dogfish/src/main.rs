//! `dogfish`: the GNU/Dogfish multicall binary.

// The C `main` below is the entry point: Rust's own start-up code would
// reopen a closed standard output on /dev/null, hiding `>&-` from the tools.
#![cfg_attr(not(test), no_main)]

mod common;
mod manual;
mod tool_list;
mod tools;

use std::ffi::OsString;
use std::path::Path;

use anyhow::Result;
use dogfish_core::{
    ColumnAlign, DogfishContext, Table, gnu_io_error, io::install_sigbus_handler, outln, stdout,
    strip_os_error_suffix,
};

use crate::common::getopt::exit_failure;

pub use tool_list::ALL_TOOLS;

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
    let argc = usize::try_from(argc).unwrap_or(0);
    let arg = |i: usize| -> Option<&[u8]> {
        if i >= argc {
            return None;
        }
        // SAFETY: the C runtime passes `argc` valid pointers; `i < argc`.
        let p = unsafe { *argv.add(i) };
        // SAFETY: a non-null argv entry is a NUL-terminated string that
        // lives for the whole process.
        (!p.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(p) }.to_bytes())
    };
    // `true` and `false` are called in loops; settle them before anything
    // allocates (the first allocation alone costs three system calls).
    if let Some(code) = trivial_exit_status(argc, arg(0), arg(1), || {
        std::env::var_os("DOGFISH_STATS").is_some()
    }) {
        return code;
    }
    for fd in 0..3 {
        // SAFETY: fcntl(F_GETFD) only queries the descriptor.
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
            // A closed standard input is held by a write-only /dev/null, so
            // reading it fails with EBADF as GNU's reads of `<&-` do.
            let mode = if fd == libc::STDIN_FILENO {
                common::input::stdin_started_closed();
                libc::O_WRONLY
            } else {
                libc::O_RDWR
            };
            // Close-on-exec: a program run by this one (`timeout`, `env`, ...)
            // gets the descriptor closed, as it was given to us.
            // SAFETY: a NUL-terminated path; the lowest free descriptor is `fd`.
            unsafe { libc::open(c"/dev/null".as_ptr(), mode | libc::O_CLOEXEC) };
            if fd == libc::STDOUT_FILENO {
                stdout::started_closed();
            }
        }
    }
    let args = (0..argc)
        .filter_map(|i| arg(i).map(|a| OsString::from_vec(a.to_vec())))
        .collect();
    libc::c_int::from(run(args))
}

/// The exit status of a `true` or `false` that has nothing to do but exit:
/// GNU's ignore every argument unless there is exactly one (`--help` or
/// `--version`). `None` when the full start-up is needed: another tool, a
/// lone argument, or `DOGFISH_STATS` asking for the `--stats` footer.
/// `argv0` and `argv1` are the first two arguments (`argc` of them in all).
#[cfg_attr(test, allow(dead_code))]
fn trivial_exit_status(
    argc: usize,
    argv0: Option<&[u8]>,
    argv1: Option<&[u8]>,
    stats_requested: impl FnOnce() -> bool,
) -> Option<libc::c_int> {
    let argv0 = argv0?;
    let name = argv0.rsplit(|&b| b == b'/').next().unwrap_or(argv0);
    let name = name.strip_prefix(b"df-").unwrap_or(name);
    // As `dogfish true ...`, the tool's own arguments start one later.
    let (tool, tool_argc) = match name {
        b"dogfish" | b"gnu-dogfish" => {
            let sub = argv1?;
            (sub.strip_prefix(b"df-").unwrap_or(sub), argc - 1)
        }
        _ => (name, argc),
    };
    let code = match tool {
        b"true" => 0,
        b"false" => 1,
        _ => return None,
    };
    (tool_argc != 2 && !stats_requested()).then_some(code)
}

/// Runs the multicall binary on `raw_args` (`argv`); returns the exit status.
#[cfg_attr(test, allow(dead_code))]
fn run(raw_args: Vec<OsString>) -> u8 {
    // SIGPIPE keeps the disposition it was given (the C `main` skips the
    // standard library's, which would ignore it), as in GNU: by default a
    // write to a closed pipe ends the process, and with `trap '' PIPE` it
    // fails with EPIPE.

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
            // Manual pages and completion scripts, from the tools' own
            // option tables (`cargo xtask docs-gen` writes them all).
            Some("--generate") => {
                let code = manual::generate(&raw_args[2..]);
                return finish("dogfish", code);
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

#[cfg(test)]
mod tests {
    use super::trivial_exit_status;

    fn status(args: &[&str], stats: bool) -> Option<libc::c_int> {
        let arg = |i: usize| args.get(i).map(|a| a.as_bytes());
        trivial_exit_status(args.len(), arg(0), arg(1), || stats)
    }

    #[test]
    fn true_and_false_exit_without_start_up() {
        assert_eq!(status(&["true"], false), Some(0));
        assert_eq!(status(&["/bin/df-false"], false), Some(1));
        assert_eq!(status(&["true", "a", "b"], false), Some(0));
        assert_eq!(status(&["dogfish", "false"], false), Some(1));
        assert_eq!(
            status(&["/x/gnu-dogfish", "df-true", "x", "y"], false),
            Some(0)
        );
    }

    #[test]
    fn a_lone_argument_or_stats_takes_the_full_path() {
        // `true --help` and `true --version` print; one argument may be either.
        assert_eq!(status(&["true", "--help"], false), None);
        assert_eq!(status(&["dogfish", "true", "--version"], false), None);
        // DOGFISH_STATS asks for the --stats footer.
        assert_eq!(status(&["true"], true), None);
        assert_eq!(status(&["echo"], false), None);
        assert_eq!(status(&["dogfish"], false), None);
        assert_eq!(status(&["true/"], false), None);
        assert_eq!(status(&[], false), None);
    }
}
