# Contributing to GNU/Dogfish 🦈🦬

Thank you for your interest in contributing to **GNU/Dogfish**! Every improvement—whether a new utility, a SIMD fast-path, a bug fix, or better documentation—advances the state of libre systems software.

---

## 🛠️ Development Quickstart

1. **Clone & Build**:
   ```bash
   git clone https://github.com/oddurs/gnu-dogfish.git
   cd gnu-dogfish
   cargo build --release
   ```
2. **Run the Test Suite (39 Suites)**:
   ```bash
   cargo test --release
   ```
3. **Run the Head-to-Head Benchmark Suite**:
   ```bash
   ./target/release/dogfish bench
   ```
4. **Lint & Format**:
   ```bash
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets
   ```

---

## 🧭 Architecture Overview

The workspace consists of two crates:
- **`crates/dogfish-core`**: Shared infrastructure used by all utilities:
  - `context.rs`: `DogfishContext`, TTY/color policy, and `--stats` telemetry interceptor.
  - `io.rs`: `FastReader` (`mmap` + `MADV_SEQUENTIAL` + 64 KiB `BufReader` fallback), `copy_file_fast` (macOS APFS `clonefile` & Linux `FICLONE` CoW), and `SIGPIPE`/`BrokenPipe` handlers.
  - `table.rs`: Adaptive NDJSON object reader/writer, terminal table renderer, and `--where` expression evaluator (`eval_json_where`).
  - `journal.rs`: `UndoJournal` transaction log and trash staging for `rm --safe`, `mv`, and `cp`.
  - `format.rs`: Human byte/duration parsers, POSIX `rwxr-xr-x` formatter, and Unicode sparkline bars.
- **`crates/dogfish`**: The multicall binary (`dogfish`) and utility implementations:
  - `fs_ops.rs`: Tier 1 — File & Directory utilities (`ls`, `cp`, `mv`, `rm`, `stat`, `df`, `du`, etc.)
  - `text.rs`: Tier 2 — Text & Streaming utilities (`cat`, `tac`, `head`, `tail`, `wc`, `sort`, `uniq`, `cut`, `tr`, `paste`, `nl`, `shuf`, `query`)
  - `hash.rs`: Tier 3 — Hashing & Encoding utilities (`b2sum`, `sha256sum`, `md5sum`, `base64`, `basenc`)
  - `sys.rs`: Tier 4 — Shell & System utilities (`echo`, `printf`, `env`, `printenv`, `id`, `uname`, `hostname`, `nproc`, `whoami`, `yes`, `true`, `false`, `sleep`, `timeout`, `watch`, `seq`, `date`, `tee`)
  - `meta.rs`: Meta-tools (`bench`, `undo`, `history`, `init`, `install-links`)

---

## ➕ How to Add a New Utility

1. **Implement `pub fn run_<tool>(args: Vec<String>, ctx: &DogfishContext) -> Result<i32>`** in the appropriate module (`fs_ops.rs`, `text.rs`, `hash.rs`, or `sys.rs`).
2. **Use `clap::Parser`** for flags, and support `-J` / `--json` (`emit_objects_or_table`) whenever the utility outputs structured records.
3. **Record telemetry** via `ctx.telemetry.add_read(...)`, `ctx.telemetry.add_written(...)`, `ctx.telemetry.add_items(...)`, and `ctx.telemetry.set_strategy(...)`.
4. **Register the utility** in `ALL_TOOLS` and `dispatch()` inside `crates/dogfish/src/main.rs`.
5. **Add integration tests** in `crates/dogfish/tests/integration.rs` (and update the tool count in `test_all_57_tools_respond_to_help_cleanly`).

---

## 📜 License Agreement

By contributing to GNU/Dogfish, you agree that your contributions will be licensed under the repository's dual **GPL-3.0-or-later OR MIT** license so that all users retain the Four Essential Freedoms.
