![GNU/Dogfish Lockup Logo](./assets/dogfish-logo.png)

[![CI (macOS & Linux)](https://github.com/oddurs/gnu-dogfish/actions/workflows/ci.yml/badge.svg)](https://github.com/oddurs/gnu-dogfish/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/oddurs/gnu-dogfish?color=black&label=release)](https://github.com/oddurs/gnu-dogfish/releases)
[![License: GPL-3.0 / MIT](https://img.shields.io/badge/license-GPLv3%20%2F%20MIT-blue.svg)](./LICENSE)
[![Platform: macOS & Linux](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-success.svg)](https://github.com/oddurs/gnu-dogfish)
[![Utilities: 57](https://img.shields.io/badge/utilities-57%20libre%20tools-orange.svg)](#all-57-libre-utilities)

---

> *"I'd just like to interject for a moment. What you are referring to as 'Coreutils', is in fact, **GNU/Dogfish**, or as I've recently taken to calling it, **GNU plus Dogfish**. Coreutils is not an operating system unto itself, but rather another free component of a fully functioning POSIX system made useful by the GNU/Dogfish SIMD byte kernels, NDJSON object pipelines, and reversible transaction journals comprising a full OS as defined by POSIX."*
> — **St. Ignucius of the Spiny Dogfish**

---

## 📜 The GNU/Dogfish Manifesto

In 1983, Richard Stallman announced the **GNU Project** to give computer users freedom—specifically, the freedom to inspect, modify, and share the foundational utilities of their operating system.

For forty years, `ls`, `cat`, `wc`, `cp`, `df`, and `sort` have stood as the bedrock of free computing. Yet somewhere along the way, userland utilities stagnated in the constraints of 1970s teletypes:
- Piping `ls` into `sort` required fragile whitespace-splitting hacks (`awk`, `cut -d' '`).
- Copying a 50 GiB file meant grinding spinning rust (or NVMe cells) instead of invoking modern Copy-on-Write (`clonefile` / `FICLONE`) filesystem primitives.
- Accidentally typing `rm -rf` meant irreversible catastrophe with no journaled rollback.
- Every command ran in complete silence with zero visibility into kernel strategy, memory bandwidth, or execution latency.

**GNU/Dogfish** is our love letter to the Free Software movement, forged in **libre Rust** (safe Rust everywhere except a small set of audited `libc` FFI calls — see [SECURITY.md](./SECURITY.md)):
1. **Uncompromising User Freedom**: No telemetry servers, no proprietary blobs, no cloud lock-in, ever. When you pass `--stats`, the telemetry belongs **exclusively to you on your local `stderr`**.
2. **Respect for Your Hardware**: Free software should not waste your CPU cycles. Every millisecond saved by SIMD vector scans (`memchr`, `bytecount`), memory-mapped I/O (`mmap`), work-stealing parallelism (`rayon`, `jwalk`), and zero-copy kernel clones (`clonefile`) returns power and time to the user.
3. **Structured Freedom in the Shell**: Text streams are universal, but **JSON Lines (NDJSON)** are self-describing. GNU/Dogfish utilities speak both fluently.

---

## 🗽 The Four Essential Freedoms

GNU/Dogfish is Free Software (*"free" as in freedom, not merely "free beer"—though it is also gratis*):

- **Freedom 0**: The freedom to run `dogfish` for any purpose, on any machine, at memory-bandwidth speeds.
- **Freedom 1**: The freedom to study how `dogfish-core` maps files into virtual memory and change it to suit your computing needs.
- **Freedom 2**: The freedom to redistribute copies of `dogfish` so you can help your neighbor escape slow, non-reversible utilities.
- **Freedom 3**: The freedom to distribute copies of your modified versions to others, advancing the state of the art of Libre Systems Programming.

---

## ⚡ Benchmarks vs. Legacy Coreutils

Every copy of `dogfish` ships with a built-in head-to-head benchmark suite (`dogfish bench`) so you can verify its claims on your own hardware at any time.

> **Read these numbers with care.** The table below is a single run on an Apple Silicon laptop against the macOS system tools (BSD userland, not GNU coreutils), on a warm page cache, with a build tuned for that CPU. Your results will differ by OS, filesystem, CPU, and file size — run `dogfish bench` yourself before relying on any figure.

```text
$ dogfish bench
⚡ dogfish bench: Generating 32 MiB synthetic benchmark corpus...

 BENCHMARK           DOGFISH    SYSTEM        SPEEDUP  RELATIVE    ENGINE STRATEGY           
 ────────────────────────────────────────────────────────────────────────────────────────────
 wc -l (32 MiB)       4.05ms   29.24ms   7.23x faster  [████████]  mmap + SIMD bytecount     
 cp (32 MiB file)     2.95ms    9.81ms   3.32x faster  [█████░░░]  APFS clonefile (CoW)      
 sort -r (32 MiB)    10.20ms   50.37ms   4.94x faster  [████████]  zero-alloc mmap + rayon   
 wc -lwc (32 MiB)     5.34ms   43.60ms   8.16x faster  [████████]  mmap + parallel SIMD scan 
 md5sum (32 MiB)     46.35ms   47.97ms   1.03x faster  [██░░░░░░]  mmap + ASM md-5           
 sha256sum (32 MiB)  17.71ms   74.35ms   4.20x faster  [███████░]  mmap + hardware SHA-256   
 seq 1 1,000,000      8.80ms  114.05ms  12.96x faster  [████████]  itoa + 64KiB BufWriter    
```

---

## 🔥 The 5 Killer Innovations

### 1. Adaptive NDJSON Object Pipelines + `dogfish query` + Auto-Table Rendering
Pipe structured objects between standard Unix commands in `bash`, `zsh`, or `fish`—and filter, sort, or project them using either standard commands (`sort --by`, `cut -f`, `head -n`) or the dedicated **`dogfish query`** SQL-like engine:

```bash
# Single-stage SQL-like query on `ls -J` output:
dogfish ls -J crates/dogfish/src \
  | dogfish query --where "size>10K" --sort size --desc --select name,size,kind,modified_relative

# Or compose standard Unix tools with NDJSON flags:
dogfish ls -J crates/dogfish/src \
  | dogfish sort --where "size>1K" --by size --desc \
  | dogfish cut -f name,size,kind,modified_relative \
  | dogfish head -n 5
```
Output:
```text
 name       size  kind  modified_relative
 ────────────────────────────────────────
 fs_ops.rs  31.2 KiB  file  4m ago           
 text.rs    28.7 KiB  file  2m ago           
 sys.rs     19.4 KiB  file  8m ago           
 meta.rs    12.1 KiB  file  1m ago           
 hash.rs    11.0 KiB  file  7m ago           
```

### 2. Global `--stats` Local Telemetry
Append `--stats` to **any** Dogfish utility to inspect wall-clock execution time, I/O throughput, item counts, and the active kernel/SIMD strategy:

```text
$ dogfish ls --tree -L 2 --stats
.
├── Cargo.lock
├── Cargo.toml
├── PLAN.md
├── README.md
├── assets/
│   └── dogfish-logo.svg
└── crates/
    ├── dogfish/
    └── dogfish-core/
[⚡ dogfish ls 471.83µs │ items: 9 │ engine: recursive visual tree renderer]
```

### 3. Reversible File Operations (`rm --safe`, `mv`, `cp` + `dogfish undo`)
Mistakes shouldn't be fatal. Dogfish maintains a local transaction journal (`~/.dogfish/journal.json` and `~/.dogfish/trash`):

```bash
$ dogfish rm --safe -v secret_notes.txt
trashed 'secret_notes.txt' (undo id #1: run `dogfish undo` to restore)

$ dogfish undo
✔ Undone: [id #1, tool: rm] Restored '/path/to/secret_notes.txt' from trash
```

### 4. Rich Visual Cards & Unicode Sparklines (`df`, `du`, `stat`, `wc --table`)
```text
$ dogfish stat Cargo.toml
  File: Cargo.toml  (regular file)
  Size: 841 B (841 B)   Blocks: 8   Inode: 193703651
Access: -rw-r--r-- (0644)   Owner: oddurs:staff (501:20)   Links: 1
Modify: 2026-10-23 23:47:31 -0400 (15m ago)
Access: 2026-10-23 23:47:44 -0400
Change: 2026-10-23 23:47:31 -0400
```

### 5. Single Multicall Binary + 40 Symlink Personalities
Like BusyBox, `dogfish` compiles into a single stripped, LTO-optimized binary that dispatches either via subcommand (`dogfish ls`) or `argv[0]` symlink (`ls -> dogfish` or `df-ls -> dogfish`).

> **Compatibility note:** Dogfish aims to match GNU behaviour for the flags it implements, and a differential test suite (`crates/dogfish/tests/gnu_compat.rs`) checks this against GNU coreutils on every Linux CI run. It does not implement every GNU flag yet, so review before aliasing it over your system tools.

---

## 🧰 All 57 Libre Utilities

| Category | Utilities | Highlights |
| :--- | :--- | :--- |
| **File & Directory** (16) | `ls`, `cp`, `mv`, `rm`, `mkdir`, `ln`, `chmod`, `chown`, `stat`, `touch`, `df`, `du`, `realpath`, `basename`, `dirname`, `mktemp` | APFS `clonefile` CoW in `cp`; parallel `jwalk` in `du`; `--tree` & `-J` in `ls`; sparkline bars in `df`/`du`; visual card in `stat`; `--safe` trash in `rm`. |
| **Text & Streaming** (13) | `cat`, `tac`, `head`, `tail`, `wc`, `sort`, `uniq`, `cut`, `tr`, `paste`, `nl`, `shuf`, `query` | SIMD `bytecount`/`memchr`; SQL-like NDJSON `query` (`-w`, `-s`, `-f`, `-n`); `--where` & `--by <field>` in `sort`; reverse SIMD `tac`; Fisher-Yates `shuf`. |
| **Hashing & Encoding** (5) | `b2sum`, `sha256sum`, `md5sum`, `base64`, `basenc` | Multi-threaded `rayon` file hashing; `-c` (`--check`) verifier; `base64url`, `base32`, `base16`/`hex`, `base2`/`bin` in `basenc`. |
| **Shell & System** (18) | `echo`, `printf`, `env`, `printenv`, `id`, `uname`, `hostname`, `nproc`, `whoami`, `yes`, `true`, `false`, `sleep`, `timeout`, `watch`, `seq`, `date`, `tee` | 64 KiB page-aligned vector output in `yes`; human durations (`500ms`, `1m30s`) in `sleep`, `timeout`, and `watch`; `--table` and `-J` in `env`. |
| **Meta-Tools** (5) | `bench`, `undo`, `history`, `init`, `install-links` | Head-to-head benchmark suite, undo journal rollback, shell integration generator (`zsh`, `bash`, `fish`), and symlink installer. |

---

## 🚀 Installation & Shell Integration

### Build from Source
```bash
git clone https://github.com/oddurs/gnu-dogfish.git
cd gnu-dogfish
cargo build --release
```

The default build targets your architecture's baseline CPU so binaries are portable. For a build tuned to the machine you're on (and the fastest benchmark numbers), opt in explicitly:

```bash
RUSTFLAGS="-C target-cpu=native" cargo build --release
```

### Option A: Instant Shell Integration (`fish`, `zsh`, `bash`)
```fish
# In Fish (~/.config/fish/config.fish):
/path/to/dogfish/target/release/dogfish init fish --safe-rm | source
```
```bash
# In Zsh (~/.zshrc) or Bash (~/.bashrc):
eval "$(/path/to/dogfish/target/release/dogfish init zsh --safe-rm)"
```

### Option B: BusyBox-Style Symlinks
```bash
# Drop-in symlinks (ls, cp, mv, cat, wc, df, du...):
./target/release/dogfish install-links ~/.local/bin

# Or prefixed symlinks (df-ls, df-cp, df-cat...):
./target/release/dogfish install-links ~/.local/bin --prefix df-
```

---

## ⚖️ License, Copyleft & Trademark Notice

Copyright © 2026 The GNU/Dogfish Contributors.
Distributed as **Free and Open Source Software** under the dual **GPL-3.0-or-later OR MIT** license ([LICENSE](./LICENSE)).

See [CONTRIBUTING.md](./CONTRIBUTING.md), [CHANGELOG.md](./CHANGELOG.md), [SECURITY.md](./SECURITY.md), and [CODE_OF_CONDUCT.md](./CODE_OF_CONDUCT.md) for community and security guidelines.

*Note: GNU/Dogfish is an independent Free Software project inspired by the GNU Project and is not affiliated with or endorsed by the Free Software Foundation.*

*"Share and enjoy! Happy hacking!"* — 🦈🦬
