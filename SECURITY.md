# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.1.x   | :white_check_mark: |

## Security Model & Guarantees

**GNU/Dogfish** is written in Rust with memory safety as a core priority:
- Utilities are written in safe Rust, except for `unsafe` blocks confined to OS/POSIX FFI calls (`statfs`, `utimensat`, `copy_file_range`, `getpwnam`, signal handling, ...) and one read-only `mmap`. [`doc/unsafe.md`](doc/unsafe.md) lists every one with why it is sound; Clippy requires a `// SAFETY:` comment on each, and `cargo xtask unsafe-audit` fails CI when the list and the source disagree. The test suites also run under AddressSanitizer.
- Regular files are read via `mmap`. If another process truncates a file *while* Dogfish is reading it, the kernel raises `SIGBUS`; Dogfish catches it and exits with status 1 and a `tool: input file was truncated while being read` message rather than crashing.
- Destructive operations refuse same-file copies, directory-into-itself copies/moves, `rm` of `.`/`..` and recursive `rm` of `/`; `mktemp` uses OS randomness and exclusive creation. Regression tests live in `crates/dogfish/tests/fs_safety.rs`.
- No network connections or external telemetry calls are ever made by any Dogfish utility.

## Supply Chain

- **Dependencies** are checked by [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) on every pull request and weekly ([`deny.toml`](deny.toml)): a crate with a [RustSec](https://rustsec.org/) advisory (vulnerable, unsound, unmaintained or yanked), a license outside the allow-list, a second major version of a crate, or a source other than crates.io fails CI.
- **Release builds are reproducible**: two builds of the same tag with the same toolchain produce a bit-identical binary; CI checks this on every push to `main`.
- **Release artifacts** come with `SHA256SUMS`, a CycloneDX SBOM per archive, and Sigstore-signed GitHub artifact attestations (SLSA build provenance, and an SBOM attestation per archive).
- **Workflows** pin every action to a full commit SHA, give each job only the token permissions it needs, and never check out pull request code in a `pull_request_target` workflow.

See [`doc/verify.md`](doc/verify.md) for how to check a release: checksum, `gh attestation verify`, and rebuilding it yourself.

## Reporting a Vulnerability

If you discover a security issue (such as a path traversal issue in `rm --safe` / `undo`, symlink race condition, or memory-safety bug in an `unsafe` FFI wrapper), please report it via [GitHub Security Advisories](https://github.com/oddurs/gnu-dogfish/security/advisories/new) or by opening a minimal issue.
