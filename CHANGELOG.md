# Changelog

All notable changes to **GNU/Dogfish** are documented in this file.

## [Unreleased]

This release makes Dogfish safe to use on real data and a drop-in for GNU coreutils 9.4 in scripts. Every tool now prints GNU's exact output, messages and exit codes when output is not a terminal. Remaining differences are listed in [ROADMAP.md](./ROADMAP.md).

### Fixed — data safety
- `cp` onto the same file (`cp a a`, `cp a ./a`, hard links, symlinks) truncated or deleted the source. It is now refused like GNU, and so are the same cases for `mv` and `ln -f`.
- `dogfish undo` of a `cp`/`mv` that overwrote a file deleted the original. Overwritten contents are now kept and restored on undo, and `cp` overwrites in place, as GNU does.
- `undo` never overwrites a path that has since reappeared, moves undone copies to the trash instead of deleting them, and restores trashed symlinks as symlinks.
- The undo journal is append-only NDJSON written under `flock`, so concurrent processes don't lose entries, recording no longer slows down as history grows, and a crash mid-write is repaired. A corrupt journal is reported instead of being silently discarded, and 0.1.0 journals are migrated automatically.
- The trash is bounded. `DOGFISH_JOURNAL_MAX` (500) entries are kept, and dropped entries take their trash payloads with them. `DOGFISH_BACKUP_LIMIT` (1G) caps overwrite backups, never the newest one and never `rm --safe` files. Orphaned trash is swept after 7 days. `dogfish bench` no longer writes to your journal.
- `cp -r`/`mv` refuse to copy or move a directory into itself. `cp -r` copies symlinks as symlinks, and `mv` works across filesystems.
- `rm` refuses `.`/`..` and recursive removal of `/` (`--no-preserve-root` overrides). `rm --safe` refuses directories without `-r`, like plain `rm`.
- `mktemp` used a predictable name and a symlink-following create. It now uses OS randomness and exclusive creation, with `0600`/`0700` applied at creation.
- `touch` no longer truncates existing files.
- `mv s f` and `cp -a`/`cp -d s f`, where `s` is a symlink to `f`, replaced `f` with a symlink to itself. cp and mv now share a port of GNU's `same_file_ok`: they refuse whatever would lose the file, and allow what a backup or another hard link keeps safe (`mv -b h1 h2`, `cp -b f s`, `cp -sf f s`).
- `cp -a --attributes-only` truncated a destination that had other hard links.
- `rm --safe` (and undo) hung on a FIFO when the trash was on another file system: the copy fallback read it. Special files are now recreated, never read.
- `seq` no longer loops forever (release builds) or panics (debug builds) near `i64::MAX`, and keeps full precision above 2^53.
- A file truncated while being read (SIGBUS on mmapped input) now produces a clean error instead of a crash.

### Fixed — GNU compatibility
- `sort`, `ls`, `df` and `du` panicked on every invocation in debug builds, because `-h` clashed with clap's help flag.
- Every tool now parses options with one port of glibc's `getopt_long` instead of clap: GNU's permutation and `POSIXLY_CORRECT`, clustering, unique abbreviations, `option '--s' is ambiguous; possibilities: ...`, last-wins repeats, `--help`/`--version` anywhere, and GNU's usage exit status per tool. Arguments need not be UTF-8.
- `--stats`, `--color` and `--no-color` were removed from every tool's arguments before it saw them, so `echo --color x`, `printf '%s' --stats` or `env cmd --stats` lost them. Each tool now parses them as its own (full spelling only), and `echo`, `printf`, `true`, `false` and `yes` behave exactly as GNU's. Dogfish's own long options no longer change GNU's abbreviations or ambiguity messages.
- Output goes through one standard-output writer with GNU's `close_stdout` semantics: a failed write (`>/dev/full`, a closed stdout) is reported once, as `TOOL: write error: ...`, and fails the tool.
- `ls`, `du`, `df` and `stat` printed decorated tables even when piped. They now produce GNU output when not on a terminal:
  - **`ls`**: every sort and layout mode, `-l` with every `--time-style`, all quoting styles, `-R`, and `--color` with GNU's built-in table and `LS_COLORS`.
  - **`du`**: block-based sizes with hard links counted once, every unit and depth option, GNU's traversal order.
  - **`df`**: FILE operands, `--output`, `-i`, `-T`, `--total`.
  - **`stat`**: the default, terse and `-f` formats, and every format directive.
- The file tools follow GNU option for option: `cp` (`-a`, `-p`, `--preserve`, `-i`, `-u`, `-t`, `-T`, `-L/-P/-H`, `-l`, `-s`, `-b`), `mv`, `rm` (`-i`, `-I`, `-v`), `ln`, `chmod` (gnulib mode parsing), `chown`, `mkdir -m`, `touch` (`-d`, `-t`, `-r`, `-a`, `-m`, `-h`), `realpath` (gnulib canonicalisation), `basename`, `dirname` and `mktemp`.
- The text tools follow GNU too:
  - **`cat`**: `-A`, `-v`, `-E`, `-T`, `-e`, `-t`.
  - **`head`/`tail`**: `-n -N`, `-n +N` and the `-c` forms.
  - **`wc`**: column widths, plus UTF-8-aware `-m` and `-L`.
  - **`sort`**: keys and every ordering option, including `-M`, `-V`, `-R` and `--sort`.
  - **`cut`**: `-c` counts bytes, as in GNU, plus `--complement`.
  - **`tr`**: classes and repeats; options stop at the first set.
  - **`nl`, `uniq`, `tac`, `paste`.**
- `printf`, `echo`, `env` (`-S`, `-C`, signal options, exit 125/126/127), `timeout` (process groups, `-k`, `--preserve-status`), `seq` (`-f`, `-w`, exact big integers), `date` (`-d`, every `+FORMAT` flag, `-I`, `-R`, `--rfc-3339`), `id`, `uname -a`, `tee`, `sleep`, `nproc`.
- Hash tools: `-c` with `--quiet`, `--status`, `--strict`, `--warn` and `--ignore-missing`, BSD `--tag`, `-z`, file-name escaping, and `b2sum -l`. `base64`/`basenc` support all eight encodings and GNU's handling of invalid input.
- Error messages are GNU's. There is no `(os error N)` suffix and no duplicated tool prefix. Unknown options get GNU's wording and per-tool exit status. Every tool answers a lone `--version`.

### Performance
- `sha256sum` now uses `ring`, which carries OpenSSL's SHA-256 assembly, and is about 1.5× faster on CPUs without SHA extensions. It now matches GNU.
- `seq` batches its integer fast path like GNU's `seq_fast`, cutting 10M lines from 178 ms to 79 ms.
- `du` reads directories in parallel and still keeps GNU's output order.
- `cp` opens each file once to attempt FICLONE and fall back to `copy_file_range`. Set `DOGFISH_UNDO=off` to skip undo backups on overwrite.

### Changed
- `ls -T` is now GNU's tab size; the tree view is `ls --tree [-L DEPTH]`.
- Release binaries target the baseline CPU instead of `target-cpu=native`, so they run on any machine of the architecture. Native tuning is opt-in via `RUSTFLAGS`.
- Added `LICENSE-GPL` (the full GPL-3.0 text, previously missing) and `LICENSE-MIT` (previously missing its liability clause). Both crates ship them.
- Minimum supported Rust version is 1.88.

### Infrastructure
- CI: fmt, pinned Clippy (Linux and macOS), rustdoc and `cargo package` checks; tests in debug and release on Linux and macOS; an MSRV job.
- Releases are automated with release-please. Merging the release PR tags the version and publishes Linux and macOS archives with SHA-256 sums.
- Conventional-commit PR titles are enforced; pushed branches open draft PRs automatically; Dependabot patch and minor updates auto-merge behind CI.

### Tests
- 1,800+ differential cases compared byte for byte against GNU coreutils (`tests/gnu_compat*.rs`), plus data-safety (`tests/fs_safety.rs`) and boundary (`tests/edge_cases.rs`) regressions. All suites also pass as an unprivileged user.

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
