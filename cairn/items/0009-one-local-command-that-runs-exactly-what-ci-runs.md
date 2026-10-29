---
id: 9
uid: b894cc25-1dda-4b88-b054-b47824adcf78
title: "One local command that runs exactly what CI runs"
type: chore
status: done
assignee: claude
claimed: 2026-10-29
closed_at: 2026-10-29
milestone: "v0.3"
depends_on:
- 8
created: 2026-10-29
updated: 2026-10-29
priority: p1
effort: m
area: testing
---

## Purpose

Most CI failures in the quality sprint were predictable environment
differences, not logic bugs: a Linux-only `clippy --fix` that broke the macOS
build, fixtures that only worked as root, a file name APFS rejects, BSD `yes`
hanging the GNU probe, a newer Clippy on CI, and one push made before the
checks had finished.

## Approach

`cargo xtask ci` (an `xtask` crate in the workspace), stopping at the first failure:

- `cargo fmt --check`; Clippy with the CI-pinned toolchain; `cargo +1.88 check`;
- macOS type-check (`--target aarch64-apple-darwin`) with a stub for `ring`'s C build;
- `cargo test` and `cargo test --release`;
- every test binary again as an unprivileged user, in a fresh target tmp dir;
- `cargo doc` with `-D warnings` and `cargo package --workspace`.

CI calls the same subcommands so the two cannot drift.

## Acceptance criteria

- [x] `cargo xtask ci` exits non-zero on the first failing step and names it.
- [x] CI jobs invoke `cargo xtask` steps rather than duplicating the commands.
- [x] CONTRIBUTING tells contributors to run it before pushing.

## 2026-10-29

Two traps found while building it, both now handled in the step itself rather
than in documentation. cargo's JSON marks a plain binary's *target* as
`"test":true`, so only the `profile` field identifies test binaries. And
`cargo package` verifies against a local registry, and cargo never rebuilds a
registry crate whose version is unchanged, so a reused target directory
verified `dogfish` against a `dogfish-core` built hours earlier; the step
uses a fresh target directory, as a CI runner does.

## Result

`crates/xtask` (std only) with `cargo xtask ci` running fmt, clippy, msrv
(`cargo +1.88 check`), macos (Clippy for `aarch64-apple-darwin` with a stub
`ring`, restoring `Cargo.lock`), test, test-release, unprivileged (as root:
every test binary again as `nobody`, from its package directory), doc and
package, stopping at the first failure with `xtask: FAILED: <step>: <command>`.
`cargo xtask gnu` runs GNU's suite. Every CI job step is now a `cargo xtask`
step; CONTRIBUTING and AGENTS.md point at it. A full local run takes about
1.5 minutes warm.
