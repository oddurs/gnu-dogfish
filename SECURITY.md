# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.1.x   | :white_check_mark: |

## Security Model & Guarantees

**GNU/Dogfish** is written in Rust with memory safety as a core priority:
- The vast majority of utilities operate in 100% safe Rust.
- `unsafe` blocks are strictly confined to audited OS/POSIX FFI boundaries (`libc::mmap` / `madvise`, `libc::clonefile` on macOS, `libc::ioctl(FICLONE)` on Linux, `libc::statvfs` / `getmntinfo`, and `getpwuid` / `getgrgid`).
- No network connections or external telemetry calls are ever made by any Dogfish utility.

## Reporting a Vulnerability

If you discover a security issue (such as a path traversal issue in `rm --safe` / `undo`, symlink race condition, or memory-safety bug in an `unsafe` FFI wrapper), please report it via [GitHub Security Advisories](https://github.com/oddurs/gnu-dogfish/security/advisories/new) or by opening a minimal issue.
