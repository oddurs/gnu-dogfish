//! bench — Head-to-head benchmark suite vs system `/bin/*` & `/usr/bin/*`!

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use std::ffi::OsString;

use anyhow::Result;
use dogfish_core::{ColumnAlign, DogfishContext, Table, outln, sparkline_bar, stdout};
use serde::Serialize;

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, count_arg, getopt_error, lossy, print_help, print_version, usage_error,
};
use crate::common::quote::quote_locale;

const BENCH_HELP: &str = "\
Run head-to-head benchmarks comparing Dogfish against system utilities

Usage: bench [OPTIONS]

Options:
  -n, --iterations <ITERATIONS>  Number of iterations per benchmark [default: 7]
  -s, --size-mb <SIZE_MB>        Size of generated benchmark payload in MiB [default: 32]
  -J, --json                     Emit JSON benchmark results
  -h, --help                     Print help
";

struct BenchArgs {
    iterations: usize,
    size_mb: usize,
    json: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BenchOpt {
    Iterations,
    SizeMb,
    Json,
    Help,
    Version,
}

const SPECS: &[OptSpec<BenchOpt>] = &[
    OptSpec::both(b'n', "iterations", HasArg::Required, BenchOpt::Iterations),
    OptSpec::both(b's', "size-mb", HasArg::Required, BenchOpt::SizeMb),
    OptSpec::both(b'J', "json", HasArg::No, BenchOpt::Json),
    OptSpec::both(b'h', "help", HasArg::No, BenchOpt::Help),
    OptSpec::long("version", HasArg::No, BenchOpt::Version),
];

/// Parses the command line; `Err` is the exit status after `--help`,
/// `--version` or a usage diagnostic.
fn parse_bench_args(args: Vec<OsString>) -> std::result::Result<BenchArgs, i32> {
    let mut p = BenchArgs {
        iterations: 7,
        size_mb: 32,
        json: false,
    };
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    while let Some(opt) = opts.next() {
        let (o, v) = opt.map_err(|e| getopt_error("bench", &e))?;
        let v = v.as_deref().map(lossy).unwrap_or_default();
        match o {
            BenchOpt::Iterations => p.iterations = count_arg("bench", "--iterations", &v)?,
            BenchOpt::SizeMb => p.size_mb = count_arg("bench", "--size-mb", &v)?,
            BenchOpt::Json => p.json = true,
            BenchOpt::Help => return Err(print_help(BENCH_HELP)),
            BenchOpt::Version => return Err(print_version("bench")),
        }
    }
    if let Some(extra) = opts.operands().first() {
        return Err(usage_error(
            "bench",
            format!("extra operand {}", quote_locale(extra)),
        ));
    }
    Ok(p)
}

#[derive(Serialize)]
struct BenchRow {
    benchmark: String,
    dogfish_ms: f64,
    system_ms: f64,
    speedup: f64,
    strategy: String,
}

pub fn run_bench(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_bench_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
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
        outln!("{}", serde_json::to_string_pretty(&results)?);
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

    outln!();
    let mut out = stdout::stdout();
    tbl.render(&mut out, st)?;
    outln!();
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

/// bench's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("bench", BENCH_HELP, SPECS)
}
