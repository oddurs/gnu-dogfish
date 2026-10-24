# Dogfish: Modern, High-Performance Coreutils in Rust

**Dogfish** is a modernized, high-performance reimagining of GNU coreutils written in Rust. Rather than carrying 40 years of obscure legacy flags, Dogfish keeps the essential GNU flags muscle memory relies on while introducing **5 killer innovations** that POSIX text-only utilities could never do:

1. **Adaptive Dual-Mode Pipelines (NDJSON Object Streams + Auto-Table Rendering)**:
   - Utilities emit and consume structured **JSON Lines (NDJSON)** (`--json` / `-J`), allowing object-aware pipelines inside standard shells (`zsh`/`bash`):
     `dogfish ls -J | dogfish sort --by size --desc | dogfish head -n 5`
   - When the final stage of an NDJSON pipeline outputs to an interactive TTY, Dogfish automatically renders a sleek, color-coded table!
2. **Global `--stats` Telemetry & Auto-Threshold Progress HUD**:
   - Pass `--stats` to any utility to print a microsecond-accurate telemetry footer on `stderr` (`[dogfish: 0.84ms | 512 MiB (609 GiB/s) | kernel: APFS clonefile]`).
   - Long-running operations (`cp`, `mv`, `rm`, `du`, `*sum`, `cat`) support live throughput & progress HUDs (`--progress` / `-P` or auto-triggered).
3. **Hardware-Sympathetic "Speed Demon" Engine**:
   - **Zero-Copy CoW Clones**: `cp` uses macOS APFS `clonefile` and Linux `FICLONE`/`copy_file_range` for instant, 0-byte overhead copies.
   - **SIMD & `mmap`**: `wc`, `cat`, `head`, `tail`, `cut`, `tr`, `nl` use `mmap` + `memchr`/`bytecount` SIMD vector scans.
   - **Work-Stealing Parallelism**: `du`, `rm -r`, `cp -r`, `chmod -R`, and `*sum` scale across all CPU cores via `rayon` / `jwalk`.
4. **Journaled Safe Operations & `dogfish undo`**:
   - `rm` (with `--safe` / trash staging) and `mv`/`cp` record actions to `~/.local/share/dogfish/journal.json` (or `.dogfish-trash`), allowing instant recovery via **`dogfish undo`** and **`dogfish history`**.
5. **Rich Visual Cards, Sparklines & Built-In `dogfish bench`**:
   - **`df` & `du`**: Inline Unicode bar charts (`[████████░░] 81%`) alongside human units.
   - **`stat`**: Visual inspection card with side-by-side symbolic (`rwxr-xr-x`) + octal (`0755`) permissions, relative timestamps (`14m ago`), and file timeline.
   - **`ls`**: Built-in `--tree` view, smart grid layout, type badges, and `--json` mode.
   - **`dogfish bench`**: Built-in head-to-head benchmark suite comparing `dogfish` directly against system `/bin/*` or GNU coreutils with a live speedup table!

---

## 1. Architecture & Workspace Layout

Dogfish uses a modular **Cargo Workspace** where utilities live in domain crates (or standalone binary wrappers) unified under the **`dogfish` multicall binary** (supporting both `dogfish <tool>` subcommands and `argv[0]` symlink dispatch).

```text
dogfish/
├── Cargo.toml                 # Workspace root (shared dependencies, LTO release profile)
├── PLAN.md                    # This architecture & roadmap document
├── crates/
│   ├── dogfish-core/          # Core engine: NDJSON stream, --stats telemetry, CoW I/O, undo journal, table/sparkline UI
│   ├── dogfish/               # Multicall binary + `bench`, `undo`, `history`, `install-links`, and all 40 utilities
└── tests/                     # Integration & head-to-head test suite
```

---

## 2. Utility Scope (40 Core Utilities + 3 Meta-Tools)

| Tier | Utilities | Key Modern Enhancements & Optimizations |
| :--- | :--- | :--- |
| **Meta-Tools** (3) | `bench`, `undo`, `history` | Head-to-head benchmark vs system coreutils (`dogfish bench`); instant rollback of safe `rm`/`mv`/`cp` (`dogfish undo`). |
| **1. File & Directory** (12) | `ls`, `cp`, `mv`, `rm`, `mkdir`, `ln`, `chmod`, `chown`, `stat`, `touch`, `df`, `du` | APFS `clonefile` / Linux CoW in `cp`; parallel `du`/`rm`; `--json` & `--tree` in `ls`/`du`; Unicode usage bars in `df`/`du`; visual card in `stat`; undoable `rm`. |
| **2. Text & Streaming** (10) | `cat`, `head`, `tail`, `wc`, `sort`, `uniq`, `cut`, `tr`, `paste`, `nl` | NDJSON field-aware `sort --by <field>`, `cut --field <field>`, `uniq --by <field>`, `head`/`tail` auto-table rendering; SIMD `wc` with sparkline breakdown. |
| **3. Hashing & Encoding** (5) | `b2sum`, `sha256sum`, `md5sum`, `base64`, `basenc` | Rayon multi-threaded file hashing; `--check` (`-c`) with colorized verification summary; `--json` output; hex/base32/base64url/z85 in `basenc`. |
| **4. Shell & System** (13) | `echo`, `printf`, `env`, `id`, `uname`, `whoami`, `yes`, `true`, `false`, `sleep`, `seq`, `date`, `tee` | Page-aligned vector I/O in `yes`; human durations in `sleep` (`1m 30s`, `500ms`); relative/ISO formats in `date`; `--json` in `env`, `id`, `uname`, `date`. |

---

## 3. Execution Phases

- [x] **Phase 0: Architecture & Design Plan**
- [ ] **Phase 1: Workspace Foundation & `dogfish-core`**
  - Global `--stats` interceptor, `SIGPIPE` handler, TTY/Color manager, NDJSON reader/writer with auto-table renderer, Unicode sparkline/bar widgets, CoW `clonefile` I/O wrapper, and `UndoJournal`.
- [ ] **Phase 2: Tier 4 — Shell & System Utilities (13 tools)**
  - `true`, `false`, `yes`, `echo`, `printf`, `sleep`, `seq`, `whoami`, `uname`, `id`, `env`, `date`, `tee`.
- [ ] **Phase 3: Tier 3 — Hashing & Encoding Utilities (5 tools)**
  - `b2sum`, `sha256sum`, `md5sum`, `base64`, `basenc`.
- [ ] **Phase 4: Tier 2 — Text & Streaming Utilities with NDJSON Object Piping (10 tools)**
  - `cat`, `nl`, `head`, `tail`, `wc`, `cut`, `tr`, `paste`, `uniq`, `sort`.
- [ ] **Phase 5: Tier 1 — File & Directory Utilities with Visual Cards & Undo (12 tools)**
  - `mkdir`, `touch`, `ln`, `chmod`, `chown`, `stat`, `rm`, `cp`, `mv`, `df`, `du`, `ls`.
- [ ] **Phase 6: Meta-Tools (`dogfish bench`, `dogfish undo`, `dogfish history`) & Verification**
  - Head-to-head benchmark suite, comprehensive integration tests, and standalone binary wrappers.
