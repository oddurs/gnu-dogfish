---
id: 38
uid: eef8e6b4-2f2c-4ca6-9b54-7bbec4d7903e
title: "Handle FIFOs in cp (--copy-contents, FIFO destinations)"
type: bug
status: planned
milestone: "v0.3"
depends_on:
- 8
created: 2026-10-29
updated: 2026-10-29
priority: p1
effort: m
area: compat
---

## What happens

Copying to a FIFO destination writes nothing and hangs. `cp -R
--copy-contents` ignores `--copy-contents` and recreates FIFOs instead of
reading them, and `cp -p --copy-contents` from a FIFO does not create the
destination early with 0600 permissions (a permissions race GNU closes).

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/cp/sparse-to-pipe.sh`
- `tests/cp/file-perm-race.sh`
- `tests/cp/parent-perm-race.sh`

## What should happen

FIFO sources and destinations behave as GNU's, including the race-free
permission order. All listed tests pass.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/cp/sparse-to-pipe.sh tests/cp/file-perm-race.sh tests/cp/parent-perm-race.sh"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.
