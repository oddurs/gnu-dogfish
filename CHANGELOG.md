# Changelog

All notable changes to **GNU/Dogfish** are documented in this file.

## [Unreleased]

### Fixed — data safety
- `cp` onto the same file (`cp a a`, `cp a ./a`, hard links, symlinks) truncated or deleted the source; it is now refused like GNU. Same for `mv` and `ln -f`.
- `dogfish undo` of a `cp`/`mv` that overwrote an existing file deleted the original; overwritten targets are now backed up to the trash and restored on undo.
- `undo` never overwrites a path that has since been recreated, moves undone copies to trash instead of deleting them, and restores trashed symlinks as symlinks.
- The undo journal is written atomically under `flock`, so concurrent `dogfish` processes no longer lose entries, and a corrupt journal is reported instead of silently discarded.
- `cp -r`/`mv` refuse to copy or move a directory into itself; `cp -r` copies symlinks as symlinks; `mv` works across filesystems.
- `rm` refuses `.`/`..` and recursive removal of `/` (`--no-preserve-root` to override); `rm --safe` no longer trashes directories without `-r`.
- `mktemp` uses OS randomness and exclusive creation with `0600`/`0700` set atomically (previously a predictable name and a symlink-following create).
- `touch` never truncates an existing file.
- `seq` no longer loops forever (release) or panics (debug) near `i64::MAX`, and keeps full precision for integers above 2^53.

### Fixed — correctness & compatibility
- `sort`, `ls`, `df` and `du` panicked on every invocation in debug builds (`-h` clashed with clap's help flag).
- GNU-compatible behaviour for `cat` (`-A -v -E -T -e -t`), `head`/`tail` (`-n -N`, `-n +N`, `-c` forms), `wc` column widths, `sort` keys and `-n`/`-g`/`-h` semantics, `nl`, `uniq`, `cut`, `tr`, `tac`, `echo -e`, `printf`, `base64`, and GNU-style error messages (no `(os error N)` suffix, no duplicated tool prefix).

### Changed
- Release binaries target the baseline CPU instead of `target-cpu=native`, so they run on any machine of the architecture; native tuning is opt-in via `RUSTFLAGS`.
- `LICENSE-GPL` (full GPL-3.0 text) and `LICENSE-MIT` (previously missing its liability clause) are now included.
- CI runs `cargo fmt --check`, `clippy -D warnings`, and the test suite in both debug and release.

### Added
- `tests/gnu_compat.rs`: 264 differential cases checked byte-for-byte against GNU coreutils on Linux.
- `tests/fs_safety.rs` and `tests/edge_cases.rs`: data-safety and numeric boundary regressions.

## [0.1.0] - 2026-10-24

### Added
- **57 Libre Utilities + Meta-Tools** in a single LTO-optimized multicall binary (`dogfish`):
  - **File & Directory (16)**: `ls` (`--tree`, `-J`), `cp` (CoW), `mv`, `rm` (`--safe`), `mkdir`, `ln`, `chmod`, `chown`, `stat` (visual card), `touch`, `df` (sparklines), `du` (parallel `jwalk`), `realpath`, `basename`, `dirname`, `mktemp`.
  - **Text & Streaming (13)**: `cat`, `tac`, `head`, `tail`, `wc` (`--table`, `-J`), `sort` (`--by`, `--where`), `uniq` (`--by`), `cut` (`-f`, `--where`), `tr`, `paste`, `nl`, `shuf`, `query` (alias `filter`).
  - **Hashing & Encoding (5)**: `b2sum`, `sha256sum`, `md5sum` (with `-c` check), `base64`, `basenc` (`base64url`, `base32`, `base32hex`, `base16`, `base2msbf`).
  - **Shell & System (18)**: `echo`, `printf`, `env` (`--table`), `printenv`, `id`, `uname`, `hostname`, `nproc`, `whoami`, `yes`, `true`, `false`, `sleep`, `timeout`, `watch`, `seq`, `date`, `tee`.
  - **Meta-Tools (5)**: `bench`, `undo`, `history`, `init` (`zsh`, `bash`, `fish`), `install-links`.
- **Adaptive NDJSON Object Pipelines**: Stream JSON Lines between utilities and auto-render aligned terminal tables on interactive TTYs.
- **SQL-Like `dogfish query` Engine**: Filter (`--where "size>10K"`), sort (`--sort size --desc`), project (`--select name,size`), and limit (`-n 5`) NDJSON streams.
- **Global `--stats` Telemetry**: Microsecond timing, throughput, and engine strategy footer on `stderr`.
- **Reversible File Operations (`UndoJournal`)**: Stage deleted files with `rm --safe` and record `mv`/`cp` transactions for instant rollback via `dogfish undo`.
- **Cross-Platform Performance Engine**:
  - macOS APFS `clonefile` and Linux `FICLONE` zero-copy Copy-on-Write in `cp`.
  - Zero-allocation `mmap` + `MADV_SEQUENTIAL` + Rayon parallel slice sorting in `sort` (~5x faster than system `sort`).
  - Parallel 256 KiB chunked SIMD scanning in `wc` (~8x faster than system `wc`).
  - Hardware SHA-256 / MD5 assembly intrinsics (`sha2/asm`, `md-5/asm`) and `itoa` integer streaming in `seq` (~13x faster than system `seq`).
- **39 Automated Tests**: Unit and integration tests running on Linux and macOS CI.
