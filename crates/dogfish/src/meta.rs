use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use dogfish_core::{ColumnAlign, DogfishContext, JournalAction, Table, UndoJournal, sparkline_bar};
use serde::Serialize;

// ---------------------------------------------------------------------------
// 1. undo — Reverses the most recent `rm --safe`, `mv`, or `cp` operation
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "undo",
    about = "Undo the most recent reversible file operation (rm --safe, mv, cp)"
)]
struct UndoArgs {
    /// Number of operations to undo
    #[arg(short = 'n', long = "count", default_value_t = 1)]
    count: usize,
}

pub fn run_undo(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = UndoArgs::try_parse_from(args)?;
    let journal = UndoJournal::open()?;
    let st = ctx.style();

    for _ in 0..parsed.count {
        match journal.undo_last()? {
            Some((entry, summary)) => {
                println!(
                    "{} [id #{}, tool: {}] {}",
                    st.bold_green("✔ Undone:"),
                    entry.id,
                    st.bold_cyan(&entry.tool),
                    summary
                );
                ctx.telemetry.add_items(1);
            }
            None => {
                println!(
                    "{}",
                    st.yellow("No more operations in Dogfish undo journal.")
                );
                break;
            }
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// 2. history — Inspect the Dogfish undo journal
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "history",
    about = "Display the Dogfish reversible operation journal"
)]
struct HistoryArgs {
    #[arg(short = 'J', long = "json")]
    json: bool,
}

pub fn run_history(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = HistoryArgs::try_parse_from(args)?;
    let journal = UndoJournal::open()?;
    let entries = journal.load_entries()?;

    if parsed.json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(0);
    }

    let st = ctx.style();
    if entries.is_empty() {
        println!("{}", st.dim("Undo journal is empty. Use `rm --safe`, `mv`, or `cp` to record reversible actions."));
        return Ok(0);
    }

    let mut tbl = Table::new(vec![
        ("ID", ColumnAlign::Right),
        ("TOOL", ColumnAlign::Left),
        ("DETAILS", ColumnAlign::Left),
    ]);

    for e in entries.iter().rev() {
        let details = match &e.action {
            JournalAction::TrashRemove {
                original_path,
                trash_path: _,
            } => format!("trashed '{}'", original_path),
            JournalAction::Move { src, dst, .. } => format!("'{}' -> '{}'", src, dst),
            JournalAction::CopyCreate {
                src, created_path, ..
            } => {
                format!("copied '{}' -> '{}'", src, created_path)
            }
        };
        tbl.add_row(vec![
            st.bold_cyan(&format!("#{}", e.id)),
            st.bold_yellow(&e.tool),
            details,
        ]);
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    tbl.render(&mut out, st)?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// 3. init — Outputs shell integration script (`eval "$(dogfish init zsh)"`)
// ---------------------------------------------------------------------------
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

// ---------------------------------------------------------------------------
// 4. install-links — Creates BusyBox-style symlinks for all 40 utilities
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "install-links",
    about = "Create symlinks for all Dogfish utilities in TARGET_DIR"
)]
struct InstallLinksArgs {
    /// Directory to create symlinks in (e.g. ./bin or ~/.local/bin)
    target_dir: String,

    /// Optional prefix for symlink names (e.g. `df-` creates `df-ls`, `df-cat`)
    #[arg(long = "prefix", default_value = "")]
    prefix: String,
}

pub fn run_install_links(
    args: Vec<String>,
    tool_names: &[&str],
    ctx: &DogfishContext,
) -> Result<i32> {
    let parsed = InstallLinksArgs::try_parse_from(args)?;
    let target = Path::new(&parsed.target_dir);
    fs::create_dir_all(target)?;
    let exe = std::env::current_exe()?;
    let st = ctx.style();

    let mut count = 0;
    for &tool in tool_names {
        if matches!(
            tool,
            "install-links" | "init" | "bench" | "undo" | "history"
        ) {
            continue;
        }
        let link_name = format!("{}{}", parsed.prefix, tool);
        let link_path = target.join(&link_name);
        if link_path.exists() || link_path.symlink_metadata().is_ok() {
            let _ = fs::remove_file(&link_path);
        }
        symlink(&exe, &link_path)?;
        count += 1;
    }

    ctx.telemetry.add_items(count);
    println!(
        "{} Installed {} utility symlinks in '{}' -> '{}'",
        st.bold_green("✔"),
        st.bold_cyan(&count.to_string()),
        target.display(),
        exe.display()
    );
    Ok(0)
}

// ---------------------------------------------------------------------------
// 4. bench — Head-to-head benchmark suite vs system `/bin/*` & `/usr/bin/*`!
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "bench",
    about = "Run head-to-head benchmarks comparing Dogfish against system utilities"
)]
struct BenchArgs {
    /// Number of iterations per benchmark
    #[arg(short = 'n', long = "iterations", default_value_t = 7)]
    iterations: usize,

    /// Size of generated benchmark payload in MiB
    #[arg(short = 's', long = "size-mb", default_value_t = 32)]
    size_mb: usize,

    /// Emit JSON benchmark results
    #[arg(short = 'J', long = "json")]
    json: bool,
}

#[derive(Serialize)]
struct BenchRow {
    benchmark: String,
    dogfish_ms: f64,
    system_ms: f64,
    speedup: f64,
    strategy: String,
}

pub fn run_bench(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = BenchArgs::try_parse_from(args)?;
    let st = ctx.style();
    let exe = std::env::current_exe()?;

    let tmp_dir = tempfile::tempdir()?;
    let bench_home = tmp_dir.path().join("dogfish_home");
    let payload_path = tmp_dir.path().join("bench_payload.txt");
    let copy_dogfish_dst = tmp_dir.path().join("copy_df.dat");
    let copy_sys_dst = tmp_dir.path().join("copy_sys.dat");

    // Generate realistic multi-megabyte log payload
    let total_bytes = parsed.size_mb * 1024 * 1024;
    if !parsed.json {
        eprintln!(
            "{} Generating {} MiB synthetic benchmark corpus in {}...",
            st.bold_cyan("⚡ dogfish bench:"),
            parsed.size_mb,
            tmp_dir.path().display()
        );
    }

    {
        let mut f = io::BufWriter::with_capacity(64 * 1024, fs::File::create(&payload_path)?);
        let sample_line = b"2026-10-23T23:45:00Z INFO  [dogfish::core::engine] request_id=94812 latency_us=42 bytes=8192 status=200\n";
        let mut written = 0;
        while written < total_bytes {
            f.write_all(sample_line)?;
            written += sample_line.len();
        }
        f.flush()?;
    }

    let p_str = payload_path.to_string_lossy().into_owned();
    let cp_df_str = copy_dogfish_dst.to_string_lossy().into_owned();
    let cp_sys_str = copy_sys_dst.to_string_lossy().into_owned();

    let mut results = Vec::new();

    // 1. wc -l (SIMD bytecount vs /usr/bin/wc -l)
    if Path::new("/usr/bin/wc").exists() {
        let df_dur = time_cmd(&bench_home, &exe, &["wc", "-l", &p_str], parsed.iterations);
        let sys_dur = time_cmd(
            &bench_home,
            Path::new("/usr/bin/wc"),
            &["-l", &p_str],
            parsed.iterations,
        );
        results.push(make_bench_row(
            &format!("wc -l ({} MiB)", parsed.size_mb),
            df_dur,
            sys_dur,
            "mmap + SIMD bytecount",
        ));
    }

    // 2. cp (macOS APFS clonefile CoW / Linux FICLONE CoW vs /bin/cp)
    if Path::new("/bin/cp").exists() {
        let df_dur = time_cmd(
            &bench_home,
            &exe,
            &["cp", &p_str, &cp_df_str],
            parsed.iterations,
        );
        let sys_dur = time_cmd(
            &bench_home,
            Path::new("/bin/cp"),
            &[&p_str, &cp_sys_str],
            parsed.iterations,
        );
        let cp_strat = if cfg!(target_os = "macos") {
            "APFS clonefile (CoW)"
        } else {
            "FICLONE / buffered copy"
        };
        results.push(make_bench_row(
            &format!("cp ({} MiB file)", parsed.size_mb),
            df_dur,
            sys_dur,
            cp_strat,
        ));
    }

    // 3. sort -n (rayon par_sort_by vs /usr/bin/sort -n)
    if Path::new("/usr/bin/sort").exists() {
        let df_dur = time_cmd(&bench_home, &exe, &["sort", "-r", &p_str], 3);
        let sys_dur = time_cmd(&bench_home, Path::new("/usr/bin/sort"), &["-r", &p_str], 3);
        results.push(make_bench_row(
            &format!("sort -r ({} MiB)", parsed.size_mb),
            df_dur,
            sys_dur,
            "rayon par_sort_by",
        ));
    }

    // 4. wc (full lines + words + bytes vs /usr/bin/wc)
    if Path::new("/usr/bin/wc").exists() {
        let df_dur = time_cmd(&bench_home, &exe, &["wc", &p_str], parsed.iterations);
        let sys_dur = time_cmd(
            &bench_home,
            Path::new("/usr/bin/wc"),
            &[&p_str],
            parsed.iterations,
        );
        results.push(make_bench_row(
            &format!("wc -lwc ({} MiB)", parsed.size_mb),
            df_dur,
            sys_dur,
            "mmap + single-pass scanner",
        ));
    }

    // 5. md5sum / md5
    let sys_md5 = if Path::new("/sbin/md5").exists() {
        Some((Path::new("/sbin/md5"), vec!["-q", &p_str]))
    } else if Path::new("/usr/bin/md5sum").exists() {
        Some((Path::new("/usr/bin/md5sum"), vec![p_str.as_str()]))
    } else {
        None
    };
    if let Some((sys_bin, sys_args)) = sys_md5 {
        let df_dur = time_cmd(&bench_home, &exe, &["md5sum", &p_str], parsed.iterations);
        let sys_dur = time_cmd(&bench_home, sys_bin, &sys_args, parsed.iterations);
        results.push(make_bench_row(
            &format!("md5sum ({} MiB)", parsed.size_mb),
            df_dur,
            sys_dur,
            "mmap + ASM md-5",
        ));
    }

    // 6. sha256sum vs /usr/bin/shasum -a 256 or /usr/bin/sha256sum
    let sys_sha = if Path::new("/usr/bin/sha256sum").exists() {
        Some((Path::new("/usr/bin/sha256sum"), vec![p_str.as_str()]))
    } else if Path::new("/usr/bin/shasum").exists() {
        Some((Path::new("/usr/bin/shasum"), vec!["-a", "256", &p_str]))
    } else {
        None
    };
    if let Some((sys_bin, sys_args)) = sys_sha {
        let df_dur = time_cmd(&bench_home, &exe, &["sha256sum", &p_str], parsed.iterations);
        let sys_dur = time_cmd(&bench_home, sys_bin, &sys_args, parsed.iterations);
        results.push(make_bench_row(
            &format!("sha256sum ({} MiB)", parsed.size_mb),
            df_dur,
            sys_dur,
            "mmap + ring SHA-256 (OpenSSL asm)",
        ));
    }

    // 7. seq 1 1000000 vs /usr/bin/seq 1 1000000
    if Path::new("/usr/bin/seq").exists() {
        let df_dur = time_cmd(
            &bench_home,
            &exe,
            &["seq", "1", "1000000"],
            parsed.iterations,
        );
        let sys_dur = time_cmd(
            &bench_home,
            Path::new("/usr/bin/seq"),
            &["1", "1000000"],
            parsed.iterations,
        );
        results.push(make_bench_row(
            "seq 1 1,000,000",
            df_dur,
            sys_dur,
            "itoa + 64KiB BufWriter",
        ));
    }

    if parsed.json {
        println!("{}", serde_json::to_string_pretty(&results)?);
        return Ok(0);
    }

    let mut tbl = Table::new(vec![
        ("BENCHMARK", ColumnAlign::Left),
        ("DOGFISH", ColumnAlign::Right),
        ("SYSTEM", ColumnAlign::Right),
        ("SPEEDUP", ColumnAlign::Right),
        ("RELATIVE", ColumnAlign::Left),
        ("ENGINE STRATEGY", ColumnAlign::Left),
    ]);

    for r in &results {
        let speedup_str = if r.speedup >= 1.0 {
            st.bold_green(&format!("{:.2}x faster", r.speedup))
        } else {
            st.yellow(&format!("{:.2}x", r.speedup))
        };
        let bar_ratio = (r.speedup / 5.0).clamp(0.1, 1.0);
        tbl.add_row(vec![
            st.bold(&r.benchmark),
            st.bold_cyan(&format!("{:.2}ms", r.dogfish_ms)),
            st.dim(&format!("{:.2}ms", r.system_ms)),
            speedup_str,
            sparkline_bar(bar_ratio, 8, st),
            st.yellow(&r.strategy),
        ]);
    }

    println!();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    tbl.render(&mut out, st)?;
    println!();
    Ok(0)
}

/// Best-of-N wall time. `home` isolates `DOGFISH_HOME` so benchmarking `cp` never
/// writes to the user's real undo journal or trash.
fn time_cmd(home: &Path, bin: &Path, args: &[&str], iterations: usize) -> Duration {
    let mut best = Duration::MAX;
    for _ in 0..iterations {
        let start = Instant::now();
        let _ = Command::new(bin)
            .args(args)
            .env("DOGFISH_HOME", home)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let el = start.elapsed();
        if el < best {
            best = el;
        }
    }
    best
}

fn make_bench_row(name: &str, df: Duration, sys: Duration, strategy: &str) -> BenchRow {
    let df_ms = df.as_secs_f64() * 1000.0;
    let sys_ms = sys.as_secs_f64() * 1000.0;
    let speedup = if df_ms > 0.0 { sys_ms / df_ms } else { 1.0 };
    BenchRow {
        benchmark: name.to_string(),
        dogfish_ms: (df_ms * 100.0).round() / 100.0,
        system_ms: (sys_ms * 100.0).round() / 100.0,
        speedup: (speedup * 100.0).round() / 100.0,
        strategy: strategy.to_string(),
    }
}
