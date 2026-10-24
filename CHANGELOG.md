# Changelog

All notable changes to **GNU/Dogfish** are documented in this file.

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
- **39 Automated Test Suites**: Unit, integration, adversarial, Unicode, and differential tests running on Linux and macOS CI.
