---
id: 4
uid: 9bdc65a3-12ab-4f90-9306-3fbb51e7b0ce
title: "Restore main: digest and blake2 0.11 bumps broke the build"
type: bug
status: done
milestone: "v0.2"
created: 2026-10-29
updated: 2026-10-29
closed_at: 2026-10-29
priority: p0
effort: s
area: release
---

## What happens

Four Dependabot PRs (#7 base64 0.23, #8 jwalk 0.9, #9 blake2 0.11, #10
digest 0.11) auto-merged within minutes without waiting for CI. digest 0.11
and blake2 0.11 do not compile against md-5 0.10 and the `digest`-trait
adapter in `hash.rs`, so `main` stopped building.

Two defects in the auto-merge workflow let them through: it trusted
Dependabot's `semver-minor` label, although Cargo treats a `0.x` minor bump
as breaking; and `gh pr merge --auto` merges immediately when no branch
protection rule requires checks.

## What should happen

`main` always builds. Only updates that are compatible by Cargo's rules merge
without a human, and only after CI on that exact commit passes.

## Reproduction

1. `git checkout f14660d && cargo build --locked` → unresolved `blake2::Blake2bVar`, `digest::VariableOutput`.

## Acceptance criteria

- [x] A regression check reproduces the failure and passes with the fix.
- [x] #11 merged: digest/blake2 reverted, base64 0.23 and jwalk 0.9 kept after the full suite passed.
- [x] The auto-merge rule refuses 0.x minor bumps and waits for CI on the head commit.
- [x] Dependabot PR titles pass the Conventional Commit check.

## Result

Fixed in #11 (squash `2b41f62`). digest 0.11 and blake2 0.11 were reverted;
base64 0.23 and jwalk 0.9 passed fmt, Clippy and all 1,915 tests and stayed.
The regression check is the build itself: `cargo build --locked` fails on
`f14660d` and passes on `2b41f62`, and CI now gates every merge on it.

The auto-merge workflow now decides compatibility by Cargo's rules (patch,
or minor at >=1.0), checked against the four bumps that slipped through
(all refused) and other cases, and it merges only after the CI run for the
PR's exact head commit passes, so it no longer depends on branch protection.
Dependabot titles are `chore(deps): ...` / `ci: ...`, which the PR Title
check accepts. Upgrading the RustCrypto crates is 0015.
