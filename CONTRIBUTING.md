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
2. **Before you push, run what CI runs**:
   ```bash
   cargo xtask ci
   ```
   It stops at the first failing step and names it: formatting, Clippy, the
   `unsafe` inventory (`doc/unsafe.md`), the MSRV
   (`rustup toolchain install 1.88`), a macOS type-check from Linux
   (`rustup target add aarch64-apple-darwin`), `cargo test` in debug (overflow
   checks are on) and release, every test binary
   again as `nobody` when you run it as root, docs, and a packaging dry run.
   `cargo xtask` lists the steps; each runs on its own too (`cargo xtask test`).
   CI calls the same steps, so the two cannot drift (CI also runs the tests on macOS).

   On Linux with GNU coreutils installed, `tests/gnu_compat*.rs` also diff Dogfish
   against the system tools; on other platforms those cases are skipped.
   `cargo xtask gnu` runs GNU's own test suite against Dogfish (slow; as a non-root
   user) and fails if a test in `util/gnu-baseline.json` stops passing.

   Outside `ci`, because they need extra tools or take longer (CI runs them in
   `.github/workflows/supply-chain.yml`):
   - `cargo xtask deny`: the dependency policy in `deny.toml` (RustSec
     advisories, licenses, duplicate crates, crates.io only). Needs
     `cargo install --locked cargo-deny@0.20.2`. A new dependency must pass it;
     an advisory is only ignored with a written reason in `deny.toml`.
   - `cargo xtask asan`: the test suites under AddressSanitizer
     (`rustup toolchain install nightly-2026-10-20 --component rust-src`).
   - `cargo xtask repro`: builds the release archive twice, from two copies of
     the tree, and fails unless they are bit-identical. `cargo xtask dist`
     builds one, as the release workflow does.
   - `cargo xtask fuzz [TARGET] [SECONDS]`: the parser fuzz targets in `fuzz/`
     (`cargo install --locked cargo-fuzz`, and the nightly above), 60 s each by
     default; `.github/workflows/fuzz.yml` runs them weekly. A crashing input
     lands in `fuzz/artifacts/TARGET`: fix it and keep it as a regression test.
     The parsers are reached through `crates/dogfish/src/fuzz_api.rs` (the
     `fuzzing` feature).
   - The undo model tests (`tests/undo_model.rs`) run a few hundred random cases
     in `cargo test`; `DOGFISH_PROPTEST_CASES=10000 cargo test --test undo_model`
     runs a long search.
3. **Run the Head-to-Head Benchmark Suite**:
   ```bash
   ./target/release/dogfish bench
   ```
4. **Adding `unsafe`**: avoid it if `std` or an existing helper can do the job.
   Otherwise give the block a `// SAFETY:` comment (Clippy enforces it) and a
   row in [`doc/unsafe.md`](doc/unsafe.md) (`cargo xtask unsafe-audit` enforces
   it).

---

## 🧭 Architecture Overview

The workspace consists of two crates, plus `crates/xtask` (the `cargo xtask` checks):
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

1. **Implement `pub fn run_<tool>(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32>`** in the appropriate module (`fs_ops.rs`, `text.rs`, `hash.rs`, or `sys.rs`).
2. **Parse options with `common::getopt`** (a port of glibc's `getopt_long`: list GNU's options in GNU's `long_options` order, `--help`/`--version` included), write output through `dogfish_core::stdout` (`out!`/`outln!`), and support `-J` / `--json` (`emit_objects_or_table`) whenever the utility outputs structured records.
3. **Record telemetry** via `ctx.telemetry.add_read(...)`, `ctx.telemetry.add_written(...)`, `ctx.telemetry.add_items(...)`, and `ctx.telemetry.set_strategy(...)`.
4. **Register the utility** in `ALL_TOOLS` and `dispatch()` inside `crates/dogfish/src/main.rs`.
5. **Add tests**: integration tests in `crates/dogfish/tests/integration.rs` (and update the tool count in `test_all_57_tools_respond_to_help_cleanly`), GNU differential cases in `tests/gnu_compat.rs`, and — for anything that writes, moves or deletes files — data-safety regressions in `tests/fs_safety.rs`.

---

## 🗺️ Roadmap & Issues

Planned work is tracked with [cairn](https://github.com/oddurs/cairn): one Markdown file per item under `cairn/items`, using the schema in `cairn.toml`. Start with `cairn next --view next`. `AGENTS.md` describes the workflow for people and agents alike. `ROADMAP.md` lists the known differences from GNU, and the generated plan is `cairn/ROADMAP.md`.

## 🔀 Pull Requests & Releases

- **Branches open PRs automatically.** Pushing a branch with new commits opens a *draft* PR against `main` (`.github/workflows/auto-pr.yml`); mark it ready when it is.
- **PR titles are Conventional Commits** (`feat: ...`, `fix(cp): ...`, `perf: ...`, `docs: ...`, `chore: ...`; add `!` for breaking changes). PRs are squash-merged, so the title becomes the commit on `main`, and the **PR Title** check enforces the format.
- **Releases are automated** by [release-please](https://github.com/googleapis/release-please): every merge to `main` updates a `chore: release X.Y.Z` PR that bumps the version in `Cargo.toml`, refreshes `Cargo.lock`, and adds a `CHANGELOG.md` entry from the commit titles (`feat` → minor, `fix`/`perf` → patch while we are pre-1.0). Merging it tags `vX.Y.Z`, publishes the GitHub Release, and attaches Linux and macOS archives with SHA-256 sums.
- **Dependabot** opens weekly dependency and Actions updates (titled `chore(deps): ...` / `ci: ...`). Updates that are semver-compatible by Cargo's rules (patch bumps, and minor bumps of crates at 1.0 or later) are squash-merged once the CI run for that exact commit passes. Everything else waits for a human, including `0.x` minor bumps (breaking in Cargo) and every major bump. The RustCrypto crates (`digest`, `md-5`, `blake2`) are grouped because they only compile together.

### Repository settings these workflows rely on (maintainers)

| Setting | Where | Why |
| :--- | :--- | :--- |
| Allow squash merging (and preferably only squash) | Settings → General → Pull Requests | One Conventional Commit per PR on `main`. |
| Allow auto-merge | Settings → General → Pull Requests | Maintainers can queue merges. The Dependabot workflow waits for CI itself. |
| Branch protection on `main` requiring the CI checks (Lint, Test, MSRV, PR Title) | Settings → Branches | **Required before relying on `--auto`:** without it, auto-merge merges immediately, before CI has run. |
| Allow GitHub Actions to create and approve pull requests | Settings → Actions → General | The default token can open the release and draft PRs. |
| Optional secrets `RELEASE_PLEASE_TOKEN`, `AUTO_PR_TOKEN` (fine-grained PAT or GitHub App token: contents + pull-requests write) | Settings → Secrets and variables → Actions | PRs opened with the default `GITHUB_TOKEN` don't trigger CI; with these secrets, release and auto-opened PRs get checks immediately. |

---

## 📜 License Agreement

By contributing to GNU/Dogfish, you agree that your contributions will be licensed under the repository's dual **GPL-3.0-or-later OR MIT** license so that all users retain the Four Essential Freedoms.
