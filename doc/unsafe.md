# `unsafe` in GNU/Dogfish

Dogfish is safe Rust except where it has to talk to the operating system
through `libc`: system calls and C library functions that the standard library
does not wrap (`statfs`, `getpwnam`, `utimensat`, `copy_file_range`, signal
handling, ...), and one memory map. This file lists every `unsafe` in the
workspace, what it does and why it is sound.

## Rules

- Every `unsafe` block carries a `// SAFETY:` comment saying why it is sound.
  Clippy's `undocumented_unsafe_blocks` lint is denied workspace-wide
  (`[workspace.lints]` in `Cargo.toml`), and so is `unsafe_op_in_unsafe_fn`.
- Every `unsafe` keyword (block, `unsafe fn`, `unsafe extern`, `unsafe impl`,
  `#[unsafe(...)]`) has a row below. `cargo xtask unsafe-audit` (part of
  `cargo xtask ci` and CI's lint job) fails when the source and this list
  disagree. It names each file's `unsafe` sites with their current line
  numbers, so a mismatch is quick to fix.
- Each file has a `### \`path\`` heading and one table row per `unsafe`, in
  source order. The first cell is the item the audit derives: the enclosing
  function for a block (closures count as their enclosing function), the
  function's own name for an `unsafe fn`, and `extern`, `impl`, `trait` or
  `attribute` for the other forms. Line numbers are not recorded here, so
  unrelated edits do not invalidate the list.
- Prefer `std` or an existing wrapper (such as `c_path` and `cvt` in
  `common/copy.rs`) over a new `unsafe` block. Keep each block to the FFI call
  itself and do the pointer-free work outside it.
- The test suites also run under AddressSanitizer (nightly, weekly and on
  every push to `main`: `.github/workflows/supply-chain.yml`).

## Recurring arguments

Most rows reduce to one of these:

- **C path**: the pointer comes from a `CString` (via `c_path` or
  `CString::new`) that lives until the call returns, so it is a valid
  NUL-terminated string; the call reads nothing else through pointers.
- **Descriptor only**: the call takes open file descriptors owned by a live
  `File` (or a raw descriptor this code just opened and closes exactly once)
  and no pointers. A wrong descriptor could only make the call fail.
- **Out-pointer**: the call writes a plain-data C struct (`stat`, `statfs`,
  `statvfs`, `tm`, `utsname`, `sigaction`, `sigset_t`, `cpu_set_t`) that the
  caller zero-initialised with `mem::zeroed()`; all-zero bytes are a valid
  value for each of these types.
- **Static storage**: `getpwnam`/`getpwuid`/`getgrnam`/`getgrgid`/`getmntinfo`
  return null or a pointer into libc storage that stays valid until the next
  call of the same family. The fields are copied out at once, and Dogfish does
  these lookups from a single thread (none of the tools that call them use
  rayon or jwalk).
- **No preconditions**: `geteuid`, `getuid`, `getpid`, `getpgrp`, `sysconf`,
  `umask` cannot fail and take no pointers.
- **Signals**: handlers are `extern "C" fn(c_int)` that only use atomics and
  async-signal-safe calls (`write`, `kill`, `signal`, `setitimer`, `_exit`).
  Signal setup runs before any thread or child exists.

## Accepted risk: memory-mapped input

`FastReader` (`dogfish-core/src/io.rs`) reads regular files through a read-only
`mmap` (`memmap2::Mmap::map`). Rust assumes the bytes behind a `&[u8]` do not
change while it is borrowed, and the kernel does not promise that: another
process can write to or truncate the file while Dogfish reads it.

- A concurrent **write** changes the bytes Dogfish sees, exactly as a `read(2)`
  racing the writer would. Dogfish only scans the bytes (counting, hashing,
  splitting lines); it never relies on two reads of the same byte agreeing to
  stay memory-safe, so the outcome is wrong-but-safe output, the same as GNU's.
- A concurrent **truncate** makes the vanished pages raise `SIGBUS`.
  `install_sigbus_handler` turns that into `tool: input file was truncated
  while being read` and exit status 1 (tested in `tests/edge_cases.rs`).

Every Rust program that maps files it does not control carries this risk;
the speed is the reason it is accepted. Files larger than 16 GiB, empty files
and non-regular files are read with `read(2)` instead.

## Inventory

### `crates/dogfish-core/src/io.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `open` | `Mmap::map` of an input file | Read-only private mapping of a regular file we opened; see "Accepted risk" above. |
| `open` | `madvise(MADV_SEQUENTIAL)` | Pointer and length describe the mapping just created; `madvise` is a hint and never changes the contents. |
| `copy_file_fast` | `clonefile` (macOS) | C paths. |
| `copy_file_fast` | `ioctl(FICLONE)` (Linux) | Descriptor only (both files open for the call). |
| `make_node_like` | `mknod` | C path. |
| `install_sigpipe_handler` | `signal(SIGPIPE, SIG_DFL)` | Restores the default disposition; no handler code runs. |
| `on_sigbus` | `write(2)` of the preformatted message | Async-signal-safe; the message is a `Vec` in a `OnceLock` that is never changed or freed once set, so pointer and length stay valid. |
| `on_sigbus` | `_exit(1)` | Async-signal-safe; never returns. |
| `install_sigbus_handler` | `sigaction(SIGBUS)` | Zeroed `sigaction` (no flags, empty mask); the handler is an `extern "C" fn(c_int)` and the message is set before it is installed. |
| `copy_tree_recreates_a_fifo_instead_of_reading_it` | `mkfifo` (test) | C path. |
| `copy_tree_recreates_fifos_inside_directories` | `mkfifo` (test) | C path. |
| `umask` | `umask` read and restore (test) | No preconditions. |

### `crates/dogfish-core/src/stdout.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `write_fd` | `write(2)` to descriptor 1 | The pointer and length describe the live `data` slice; a bad descriptor only makes the call fail. |

### `crates/dogfish-core/src/journal.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `drop` | `flock(LOCK_UN)` on the journal lock | Descriptor only (owned by the lock guard). |
| `lock` | `flock(LOCK_EX)` on the journal lock | Descriptor only. |

### `crates/dogfish/src/main.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `attribute` | `#[unsafe(no_mangle)]` on `main` | Exports the C entry point under its C name; no other symbol in the binary is called `main` (the Rust one is replaced by `#![no_main]`). |
| `main` | `unsafe extern "C" fn main(argc, argv)` | Called only by the C runtime, which guarantees `argc` entries of `argv` (its documented `# Safety` contract). |
| `main` | `fcntl(F_GETFD)` on descriptors 0 to 2 | Descriptor only; queries, never changes. |
| `main` | `open("/dev/null")` to fill a closed 0, 1 or 2 | C path; the lowest free descriptor is the closed one, as GNU's stdopen does. |
| `main` | read `argv[i]` | `i < argc`, and the C runtime passes `argc` valid pointers. |
| `main` | `CStr::from_ptr(argv[i])` | Checked non-null; argv strings are NUL-terminated and live for the whole process. |

### `crates/dogfish/src/common/backup.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `check_extension` | `pathconf(_PC_NAME_MAX)` | C path. |

### `crates/dogfish/src/common/copy.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `open_path` | `open` with `O_CLOEXEC` | C path; the variadic mode argument is passed as `c_uint`, the promoted type C expects. |
| `open_path` | `File::from_raw_fd` | The descriptor was just returned by `open` and is owned by nothing else. |
| `unlink_path` | `unlinkat` | C path. |
| `link_path` | `linkat` | C paths. |
| `chmod_path` | `chmod` | C path. |
| `fchmod_file` | `fchmod` | Descriptor only. |
| `utimens_path` | `utimensat` | C path; `times` is an array of two `timespec`s, as the call requires. |
| `lchown_ids` | `fchown` | Descriptor only. |
| `lchown_ids` | `lchown` | C path. |
| `mknod_path` | `mknod` | C path. |
| `mkfifo_path` | `mkfifo` | C path. |
| `eaccess_ok` | `faccessat(AT_EACCESS)` | C path. |
| `seek_data` | `lseek(SEEK_DATA/SEEK_HOLE)` | Descriptor only. |
| `lseek` | `lseek` | Descriptor only. |
| `punch_hole` | `fallocate(PUNCH_HOLE)` (Linux) | Descriptor only. |
| `clone_file` | `ioctl(FICLONE)` (Linux) | Descriptor only. |
| `new` | `geteuid` | No preconditions. |
| `copy_internal` | `mkdir` | C path. |
| `copy_reg` | `futimens` | Descriptor only, plus an array of two `timespec`s. |
| `copy_reg` | `close(into_raw_fd())` to see `close` errors | `into_raw_fd` gives up ownership, so the descriptor is closed exactly once. |
| `sparse_copy` | `copy_file_range` (Linux) | Descriptor only; null offset pointers mean "use and advance the file offsets". |
| `try_clonefile` | `clonefile` (macOS) | C paths. |

### `crates/dogfish/src/common/date_format.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `local_time` | `localtime_r` and `tm_zone` | Out-pointer; a non-null `tm_zone` points at libc's zone-name storage and is copied before any other time call. |

### `crates/dogfish/src/common/dirfd.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `stat` | `fstatat`, then `assume_init` | C name; the `stat` out-buffer is read only after the call succeeded and filled it. |
| `open_dir` | `openat(O_DIRECTORY)`, `OwnedFd::from_raw_fd` | C name; the new descriptor is owned by nothing else. |
| `unlink` | `unlinkat` | C name. |
| `chmod` | `fchmodat` | C name. |
| `write_protected` | `faccessat(W_OK)` | C name. |
| `getdents` | `getdents64` (Linux) | Descriptor only; the kernel writes at most the buffer's length into it, and the records are parsed with bounds checks. |
| `read_entries` | `fcntl(F_DUPFD_CLOEXEC)`, `fdopendir`, `close` (not Linux) | Descriptor only; the duplicate is owned by the stream, or closed once if `fdopendir` fails. |
| `read_entries` | `readdir`, errno reset, `CStr::from_ptr(d_name)` (not Linux) | Open stream; a non-null entry is valid until the next `readdir` and its name is copied first. |
| `read_entries` | `closedir` (not Linux) | The stream is open and closed exactly once. |

### `crates/dogfish/src/common/file_info.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `lgetxattr_bytes` | `lgetxattr` | C path and name; the buffer pointer and length describe the same `Vec`. |
| `has_xattrs` | `llistxattr` with a null buffer and size 0 | C path; with size 0 the call writes nothing and returns the list's length. |

### `crates/dogfish/src/common/file_times.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `set_file_times` | `utimensat` | C path; an array of two `timespec`s. |

### `crates/dogfish/src/common/mode.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `process_umask` | `umask(0)` to read the mask | No preconditions. Called before any thread that creates files starts, so the moment with mask 0 is never observed. |
| `process_umask` | `umask(old)` to restore it | No preconditions. |

### `crates/dogfish/src/common/path.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `rename_noreplace` | `renameat2(RENAME_NOREPLACE)` (Linux) | C paths. |
| `rename_noreplace` | `rename` (fallback) | C paths. |

### `crates/dogfish/src/common/sha256.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `blocks` | call of the `#[target_feature(enable = "avx2,bmi1,bmi2")]` `compress` | An `Avx2Sha256` is only made after `is_fastest` detected AVX2, BMI1 and BMI2. |
| `store` | `_mm256_storeu_si256` | The destination is a 32-byte (8 × `u32`) subslice; the store is unaligned. |

### `crates/dogfish/src/common/strftime.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `local_tm` | `localtime_r` | Out-pointer. |
| `tm_zone_name` | `CStr::from_ptr(tm_zone)` | Checked non-null first; `localtime_r` sets it to a static C string. |

### `crates/dogfish/src/common/uname_info.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `get_uname_info` | `uname` | Out-pointer; on success every field is NUL-terminated. |

### `crates/dogfish/src/common/users.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `get_username_by_uid` | `getpwuid` | Static storage; null record and null name are checked. |
| `get_groupname_by_gid` | `getgrgid` | Static storage; null record and null name are checked. |

### `crates/dogfish/src/tools/chown.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `lookup_user` | `getpwnam` | C path (the name). |
| `lookup_user` | read `pw_uid`, `pw_gid` | Static storage; only dereferenced when non-null. |
| `lookup_group` | `getgrnam` | C path (the name). |
| `lookup_group` | read `gr_gid` | Static storage; only dereferenced when non-null. |

### `crates/dogfish/src/tools/cat.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `run_cat` | `mem::zeroed::<stat>()` | Out-pointer (zeroed plain data). |
| `run_cat` | `fstat(1)` | Descriptor only, plus the out-pointer. |

### `crates/dogfish/src/tools/cp.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `re_protect` | `geteuid` | No preconditions. |

### `crates/dogfish/src/tools/date.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `extern` | declares `tzset(void)` | The declaration matches POSIX `void tzset(void)`; the `libc` crate does not bind it on every Unix. |
| `utc_time` | `gmtime_r` | Out-pointer. |
| `make_time` | `timegm` / `mktime` | `tm` is a valid, initialised `struct tm` that the call normalises in place. |
| `run_date` | `env::set_var("TZ", "UTC0")` for `-u` | Unsafe since edition 2024 because another thread could read the environment at the same time; `date` has no other threads. |
| `run_date` | `tzset()` | No arguments; re-reads `TZ` into libc's zone state. |

### `crates/dogfish/src/tools/df.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `read_mount_table` | `getmntinfo` (macOS) | Writes a pointer to libc's mount array into `mntbuf`. |
| `read_mount_table` | `slice::from_raw_parts` over the mount array | Static storage: `count` > 0 entries at a non-null pointer, valid until the next `getmntinfo`. |
| `read_mount_table` | `CStr::from_ptr` on mount names | Called only on the NUL-terminated name arrays of those entries. |
| `query` | `mem::zeroed::<statvfs>()` | Out-pointer (zeroed plain data). |
| `query` | `statvfs` | C path and out-pointer. |

### `crates/dogfish/src/tools/env.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `apply_signal_actions` | `signal`, `sigaddset`, `sigprocmask` for `--default-signal`, `--ignore-signal`, `--block-signal` | Signals: dispositions only (`SIG_DFL`/`SIG_IGN`, no handler code), single-threaded, just before `exec`. |
| `list_signal_handling` | `sigprocmask`, `sigaction` queries for `--list-signal-handling` | Out-pointers; queries only (null "new" arguments). |
| `exec_with_env` | `execve` | The path, argument and environment pointers come from `CString`s and null-terminated pointer arrays that outlive the call. |
| `exec_with_env` | `execve("/bin/sh", ...)` for `ENOEXEC` | As above. |

### `crates/dogfish/src/tools/id.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `lookup_user` | `getpwnam` / `getpwuid` | Static storage; the record is checked non-null and copied out at once (`pw_name` is never null in a found record). |
| `group_list` | `getgroups` | Called with 0 to size the buffer, then with a buffer of that size. |
| `group_list` | `getgrouplist` | The buffer holds `n` entries; the call updates `n`. |
| `group_list` | `getgrouplist` (Apple prototype) | As above; Apple's prototype takes `int` group IDs, which have the same size as `gid_t`. |
| `current` | `getuid`, `geteuid`, `getgid`, `getegid` | No preconditions. |

### `crates/dogfish/src/tools/ln.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `open_dir` | `open(O_DIRECTORY)` | C path. |
| `open_dir` | `close` | The descriptor was opened just above and is closed exactly once. |
| `link_at` | `linkat` | C paths. |

### `crates/dogfish/src/tools/ls/lister.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `read_dir_entries` | `opendir`, `readdir`, `closedir` | C path; each `dirent` is checked non-null and copied before the next `readdir`; the stream is closed once. |

### `crates/dogfish/src/tools/mkdir.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `mkdir_with_umask` | `umask(mask)` before `mkdir` | No preconditions; `mkdir` is single-threaded, so nothing else creates files under the temporary mask. |
| `mkdir_with_umask` | `umask(old)` to restore it | No preconditions. |

### `crates/dogfish/src/tools/nproc.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `online_processors` | `sched_getaffinity` (Linux) | Out-pointer of exactly `size_of::<cpu_set_t>()` bytes. |
| `online_processors` | `sysconf` | No preconditions. |

### `crates/dogfish/src/tools/stat.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `query` | `mem::zeroed::<statfs>()` (Linux) | Out-pointer (zeroed plain data). |
| `query` | `statfs` (Linux) | C path and out-pointer. |
| `query` | `transmute` of `fsid_t` to `[u32; 2]` | `fsid_t` is two 32-bit integers (8 bytes, no padding, any bit pattern valid), as GNU `stat` reads it. |
| `query` | `mem::zeroed::<statvfs>()` (other Unix) | Out-pointer (zeroed plain data). |
| `query` | `statvfs` (other Unix) | C path and out-pointer. |
| `statfs_type_name` | `mem::zeroed::<statfs>()` (macOS) | Out-pointer (zeroed plain data). |
| `statfs_type_name` | `statfs` (macOS) | C path and out-pointer. |
| `statfs_type_name` | `CStr::from_ptr(f_fstypename)` (macOS) | The kernel fills `f_fstypename` with a NUL-terminated name inside the fixed-size array. |

### `crates/dogfish/src/tools/tee.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `run_tee` | `signal(SIGINT/SIGPIPE, SIG_IGN)` for `-i` and `--output-error` | Signals: dispositions only, before any I/O. |

### `crates/dogfish/src/tools/timeout.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `arm_timer` | `setitimer(ITIMER_REAL)` | Reads a fully initialised `itimerval`; async-signal-safe (also called from the handler). |
| `timeout_on_signal` | the signal handler's `kill`, `write`, `signal`, `_exit` | Signals: atomics and async-signal-safe calls only; the verbose message is a `OnceLock` set before the handler is installed. |
| `run_timeout` | `setpgid`, `sigaction`, `signal` setup | Signals: set up before the child exists; zeroed `sigaction` with an `extern "C"` handler. |
| `run_timeout` | `CommandExt::pre_exec` | The closure runs in the forked child and only calls `signal`, which is async-signal-safe. |
| `run_timeout` | `getpid`, `getpgrp` | No preconditions. |
| `run_timeout` | `setrlimit`, `signal`, `sigprocmask`, `raise` to die by the child's signal | Plain calls on this process with initialised arguments. |

### `crates/dogfish/src/tools/wc.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `stdin_stat` | `mem::zeroed::<stat>()` | Out-pointer (zeroed plain data). |
| `stdin_stat` | `fstat(0)` | Descriptor only, plus the out-pointer. |

### `crates/dogfish/src/tools/whoami.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `run_whoami` | `geteuid` | No preconditions. |

### `crates/dogfish/tests/common/mod.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `wait_bounded` | `kill(SIGKILL)` on a hung child (test) | Signals our own child, which has not been reaped, so its pid cannot have been reused. |

### `crates/dogfish/tests/edge_cases.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `sigbus_from_truncated_mmap_is_a_clean_error` | `kill(SIGBUS)` (test) | Signals our own child, which has not been waited for yet. |

### `crates/dogfish/tests/gnu_compat_listing.rs`

| Item | Operation | Why it is sound |
| --- | --- | --- |
| `build_fixture` | `mkfifo` (test) | C path. |
