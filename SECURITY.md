# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.1.x   | :white_check_mark: |

## Security Model & Guarantees

**GNU/Dogfish** is written in Rust with memory safety as a core priority:
- Utilities are written in safe Rust, except for a small number of `unsafe` blocks confined to OS/POSIX FFI calls: `mmap` (via `memmap2`), `clonefile` (macOS) and `ioctl(FICLONE)` (Linux), `statvfs` / `getmntinfo`, `getpwnam` / `getgrnam` / `getpwuid` / `getgrgid`, `chown`, `flock` on the undo journal, and `signal(SIGPIPE)`.
- Regular files are read via `mmap`. If another process truncates a file *while* Dogfish is reading it, the kernel raises `SIGBUS`; Dogfish catches it and exits with status 1 and a `tool: input file was truncated while being read` message rather than crashing.
- Destructive operations refuse same-file copies, directory-into-itself copies/moves, `rm` of `.`/`..` and recursive `rm` of `/`; `mktemp` uses OS randomness and exclusive creation. Regression tests live in `crates/dogfish/tests/fs_safety.rs`.
- No network connections or external telemetry calls are ever made by any Dogfish utility.

## Reporting a Vulnerability

If you discover a security issue (such as a path traversal issue in `rm --safe` / `undo`, symlink race condition, or memory-safety bug in an `unsafe` FFI wrapper), please report it via [GitHub Security Advisories](https://github.com/oddurs/gnu-dogfish/security/advisories/new) or by opening a minimal issue.
