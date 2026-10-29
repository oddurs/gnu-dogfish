---
id: 38
uid: eef8e6b4-2f2c-4ca6-9b54-7bbec4d7903e
title: "Handle FIFOs in cp (--copy-contents, FIFO destinations)"
type: bug
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

- [x] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [x] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.

## Result

Fixed by porting GNU's copy.c, cp-hash.c, ln.c, force-link.c and backupfile.c into `common/copy.rs` (shared by cp and mv) and ln: FIFO destinations and `--copy-contents` behave as GNU's, with the race-free permission order. Every listed GNU test passes (util/gnu-test-one.sh) and is in `util/gnu-baseline.json`; 104 new differential cases cover the cp/mv/ln changes, and the undo trash keeps every replaced destination (special files are recreated, never read).
