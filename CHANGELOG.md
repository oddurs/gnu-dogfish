# Changelog

All notable changes to **GNU/Dogfish** are documented in this file.

## [1.0.0](https://github.com/oddurs/gnu-dogfish/compare/v0.1.0...v1.0.0) (2026-10-30)


### ⚠ BREAKING CHANGES

* the -J output of these tools changed (before -> after):
    - ls: {name,path,kind,size,mode,mode_octal,user,group,modified_unix,
      modified_relative} -> {path,name,type,size,blocks,mode,mode_symbolic,
      nlink,uid,user,gid,group,inode,dev,mtime,atime,ctime,btime (+_ns),
      target}
    - stat: {name,file_type,size,size_human,links,device,mode_octal,accessed,
      modified,changed,modified_relative,symlink_target,...} -> the ls fields
      with path, plus io_block
    - df: {filesystem,fs_type,total_bytes,used_bytes,available_bytes,
      use_percent (float),mount_point} -> {filesystem,fs_type,mount,path,size,
      used,available,use_percent (GNU's, integer),inodes,inodes_used,
      inodes_available,inodes_use_percent}; --total row has "total":true
    - du: {path,size,size_human,files} -> {path,size|inodes,files,mtime?}
    - wc, md5sum/sha256sum/b2sum: `file` -> `path`; --check records
      {file,status:"OK"|...} in a pretty array -> one {path,status:"ok"|
      "failed"|"unreadable",expected,actual} per line
    - nproc: {available_cpus,total_cpus} -> {processors,installed}
    - env: {key,value} -> {name,value}; printenv: a pretty object -> one
      {name,value} record per variable
    - realpath: {input,realpath} -> {path,resolved}
    - whoami: {uid,username} -> {uid,user}
    - id: pretty {uid(effective),user,gid,group,groups:[[gid,name]]} -> one
      line {uid,user,gid,group (real),euid,egid,groups:[{gid,name}]}
    - uname: pretty object -> one line, same fields
    - date: pretty {unix,unix_ms,utc,local,format} -> {time,time_ns,
      utc_offset,zone,formatted}
    - history: pretty array of journal entries -> one {id,time,time_ns,tool,
      action:"copy"|"move"|"trash",source,dest,backup,path,trash,...} per entry
    - seq: [1,2,3] -> {"value":1} per line
    - bench: pretty array with *_ms -> {benchmark,dogfish_duration_ns,
      system_duration_ns,speedup,strategy} per line
    - consumers: on a terminal, -J now writes NDJSON (was a pretty array)
* the -J output of every tool changes format (see doc/ndjson.md). ls, stat, df, du, wc, md5sum/sha256sum/b2sum, nproc, env, printenv, realpath, whoami, id, uname, date, history, seq and bench now emit one record per line with the shared field names; pretty-printed JSON and top-level arrays are gone. Without -J nothing changes.

### Features

* -J action records for file-changing tools, and error records everywhere ([#44](https://github.com/oddurs/gnu-dogfish/issues/44)) ([3c87e34](https://github.com/oddurs/gnu-dogfish/commit/3c87e349bfbfacc9384ab3f49604f6a17f5b65f1))
* **bench:** head-to-head benchmarks against GNU coreutils ([#23](https://github.com/oddurs/gnu-dogfish/issues/23)) ([14b9cec](https://github.com/oddurs/gnu-dogfish/commit/14b9cece0a53d310b32e1839d09ea0d6de9b0205))
* data safety, GNU compatibility, automated releases, stricter CI ([#1](https://github.com/oddurs/gnu-dogfish/issues/1)) ([53427e4](https://github.com/oddurs/gnu-dogfish/commit/53427e4c3fa7604069535159a5d9d38cd50598dc))
* **ls,du,rm:** --dired, --hyperlink, du --time/--files0-from, unlimited depth ([#32](https://github.com/oddurs/gnu-dogfish/issues/32)) ([07e587d](https://github.com/oddurs/gnu-dogfish/commit/07e587dc13a4a40041601ce8d5441bc5e5e415a8))
* **manual:** man pages and bash/zsh/fish completions from the option tables ([#34](https://github.com/oddurs/gnu-dogfish/issues/34)) ([498bc06](https://github.com/oddurs/gnu-dogfish/commit/498bc0601d3bda0c33e3a124f86854ab33071a1f))
* **ndjson:** -J for basename, dirname, hostname; action records for mktemp, tee, undo, install-links ([#46](https://github.com/oddurs/gnu-dogfish/issues/46)) ([d1bd94a](https://github.com/oddurs/gnu-dogfish/commit/d1bd94af2bbf2209c21f712c96029a7133e19615))
* one getopt_long parser and one stdout writer for every tool ([#24](https://github.com/oddurs/gnu-dogfish/issues/24)) ([83fb34b](https://github.com/oddurs/gnu-dogfish/commit/83fb34bb68e0bca6d83e3a43d30b29b938937a6d))
* one NDJSON record contract for every -J producer, with versioned schemas ([#42](https://github.com/oddurs/gnu-dogfish/issues/42)) ([ad06c75](https://github.com/oddurs/gnu-dogfish/commit/ad06c758935d44d8c7b0b40a98b13fdb9e11a1fe))
* **seq,printf:** long double arithmetic and formatting as GNU's on x86-64 ([#50](https://github.com/oddurs/gnu-dogfish/issues/50)) ([896d9b7](https://github.com/oddurs/gnu-dogfish/commit/896d9b74f1c09550f26d4b11bb7efa62e15da5f7))
* **sort,cp:** GNU merge and external sort, long-double -g, key checks, --debug ([#38](https://github.com/oddurs/gnu-dogfish/issues/38)) ([389a08b](https://github.com/oddurs/gnu-dogfish/commit/389a08bb6cb4a00ad3128d1213f48d1b3df3cc0b))
* **tail:** port GNU's follow modes (-f, -F, --pid, --retry, inotify) ([#30](https://github.com/oddurs/gnu-dogfish/issues/30)) ([7a8667f](https://github.com/oddurs/gnu-dogfish/commit/7a8667f1be0cf039c7b9c935765ea23c01977f39))
* **testing:** run GNU coreutils' own test suite; track the roadmap with cairn ([#12](https://github.com/oddurs/gnu-dogfish/issues/12)) ([891b5a8](https://github.com/oddurs/gnu-dogfish/commit/891b5a8f9a97238b24838302ed7853518c698e57))
* **testing:** run single GNU tests locally in seconds ([#15](https://github.com/oddurs/gnu-dogfish/issues/15)) ([c3e5b35](https://github.com/oddurs/gnu-dogfish/commit/c3e5b35600a1ba4d4b9231aa128bf065e907de33))
* **xtask:** cargo xtask ci runs exactly what CI runs ([#14](https://github.com/oddurs/gnu-dogfish/issues/14)) ([08bbc63](https://github.com/oddurs/gnu-dogfish/commit/08bbc63c3ea52fc603d54416615c8ca52d59d673))


### Bug Fixes

* **chmod,chown:** walk -R trees by descriptor at any depth ([#35](https://github.com/oddurs/gnu-dogfish/issues/35)) ([b4d519f](https://github.com/oddurs/gnu-dogfish/commit/b4d519ff406181409ce0b0677f0eda66d19eeb6a))
* **chown,chmod:** --preserve-root checks every directory the walk enters ([#28](https://github.com/oddurs/gnu-dogfish/issues/28)) ([80e2ab3](https://github.com/oddurs/gnu-dogfish/commit/80e2ab3846e5c469791f5908d7c5bee3d813a03e))
* **chown:** detect directory cycles under -L as GNU's fts does ([#41](https://github.com/oddurs/gnu-dogfish/issues/41)) ([d8675c8](https://github.com/oddurs/gnu-dogfish/commit/d8675c8bc8090bab361e4f0977f8fe21955cb733))
* **core:** never read FIFOs or devices when moving across file systems ([#21](https://github.com/oddurs/gnu-dogfish/issues/21)) ([66c7976](https://github.com/oddurs/gnu-dogfish/commit/66c7976a67c063d03caf12e0f53d5d111d053b50))
* **cp,mv,ln:** port GNU copy.c, ln.c and backupfile ([#22](https://github.com/oddurs/gnu-dogfish/issues/22)) ([359939e](https://github.com/oddurs/gnu-dogfish/commit/359939efbec9f416d3f6d3445c3fb7dd98cbdd96))
* **date,touch,sys:** port gnulib parse_datetime; GNU 9.4 edge cases in system tools ([#26](https://github.com/oddurs/gnu-dogfish/issues/26)) ([c8caa81](https://github.com/oddurs/gnu-dogfish/commit/c8caa81907a637c5be3266fd85fcd99bb982e18e))
* **date:** four divergences found by an hour of differential fuzzing against GNU date ([#45](https://github.com/oddurs/gnu-dogfish/issues/45)) ([ee6777f](https://github.com/oddurs/gnu-dogfish/commit/ee6777f4a0a8ebe005a9a6aa2fb66e70eb5473fc))
* **deps:** revert breaking digest/blake2 bumps; gate Dependabot merges on CI ([#11](https://github.com/oddurs/gnu-dogfish/issues/11)) ([2b41f62](https://github.com/oddurs/gnu-dogfish/commit/2b41f62d324a4011d0bc6e908d4de525623badac))
* **ls,du,stat,tac:** one nstrftime for every tool; --color order; tac errors ([#51](https://github.com/oddurs/gnu-dogfish/issues/51)) ([f45ce3c](https://github.com/oddurs/gnu-dogfish/commit/f45ce3cb5ce1eda0a97906d94d04aafa646074ec))
* **mv,cp,undo:** keep hard links across file systems; same-file check through symlinked directories ([#40](https://github.com/oddurs/gnu-dogfish/issues/40)) ([2e273c0](https://github.com/oddurs/gnu-dogfish/commit/2e273c0e321be69f713a8d83d10b6e0f379cdd4b))
* **mv,undo:** record a move by where it went when the rename moves its path ([#43](https://github.com/oddurs/gnu-dogfish/issues/43)) ([5687a08](https://github.com/oddurs/gnu-dogfish/commit/5687a08f08cfa19ab2d8a17c396e966ebfa327ae))
* open and name non-UTF-8 files in the text tools ([#49](https://github.com/oddurs/gnu-dogfish/issues/49)) ([f3adca2](https://github.com/oddurs/gnu-dogfish/commit/f3adca27b0ff4f67a7e0f3479cf41b83383bbc10))
* **stdout:** report write errors exactly as GNU does (EPIPE, ENOSPC, EBADF) ([#37](https://github.com/oddurs/gnu-dogfish/issues/37)) ([6848b05](https://github.com/oddurs/gnu-dogfish/commit/6848b056968aa2e05d2dc6d650a1f31642bd9e17))
* **text:** GNU behaviour and speed for cat, head, tail, wc, sort, uniq, cut, tr, tac, printf, echo, paste, mktemp, nl, shuf ([#29](https://github.com/oddurs/gnu-dogfish/issues/29)) ([21b2bb2](https://github.com/oddurs/gnu-dogfish/commit/21b2bb249467850147c9a8d502ebf56e0f449da7))
* **undo:** restore hard links that cp --preserve=links replaced ([#52](https://github.com/oddurs/gnu-dogfish/issues/52)) ([a720f70](https://github.com/oddurs/gnu-dogfish/commit/a720f70d49cd06e71f76bf8c6a5ebe34cda96629))


### Performance Improvements

* **cp:** copy files into new directories on worker threads ([#57](https://github.com/oddurs/gnu-dogfish/issues/57)) ([d108df7](https://github.com/oddurs/gnu-dogfish/commit/d108df71446b34f6616a87831cf2ca77c2db4609))
* cut per-invocation startup cost ([#31](https://github.com/oddurs/gnu-dogfish/issues/31)) ([79affe3](https://github.com/oddurs/gnu-dogfish/commit/79affe3d3556203eb8d8b97cb11546aef094f162))
* faster base64/basenc, seq, rm, chmod, du, ls and sha256sum ([#27](https://github.com/oddurs/gnu-dogfish/issues/27)) ([59defc4](https://github.com/oddurs/gnu-dogfish/commit/59defc43abe99d487a220d603269f5f651e5739b))
* sha256sum and ls past GNU, startup closer; static unwinder back on the binary ([#47](https://github.com/oddurs/gnu-dogfish/issues/47)) ([05c4d7d](https://github.com/oddurs/gnu-dogfish/commit/05c4d7d6dedf60d20c213f348e9816e2094e9f9f))

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
- `cp`, `mv`, `ln`, `mkdir` and `mktemp` accept file names that are not valid UTF-8, byte for byte as GNU does, and the undo journal records them losslessly (as `{"path_bytes": BASE64}`; journals with plain string paths still read).
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
