---
id: 32
uid: a7cf6993-0bb0-4c3e-a074-2d15c91b6280
title: "Finish GNU's same-file and backup rules in ln, cp and mv"
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
area: safety
---

## What happens

0025 ported GNU's `same_file_ok` to cp and mv. Still missing:

- `ln -f --backup=simple a b` fails with `File exists` when `b~` is already a
  hard link to `b` (the rename is a no-op; GNU's `force_linkat` unlinks and retries);
- the `backing up 'a' might destroy source; 'a~' not copied` check in cp and mv;
- whatever `cp/same-file.sh` and `cp/backup-1.sh` still show after 0025
  (`mv/hard-4.sh` and `mv/part-symlink.sh` pass since then).

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/ln/backup-1.sh`
- `tests/ln/misc.sh`
- `tests/cp/backup-is-src.sh`
- `tests/mv/backup-is-src.sh`
- `tests/cp/same-file.sh`
- `tests/cp/backup-1.sh`

## What should happen

All listed tests pass; the differential suite covers each case.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/ln/backup-1.sh tests/ln/misc.sh tests/cp/backup-is-src.sh"`, as a non-root user.

## Acceptance criteria

- [x] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [x] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.

## Result

Fixed by porting GNU's copy.c, cp-hash.c, ln.c, force-link.c and backupfile.c into `common/copy.rs` (shared by cp and mv) and ln: ln's `force_linkat`-style unlink-and-retry, the `might destroy source` checks and GNU's remaining same-file decisions. Every listed GNU test passes (util/gnu-test-one.sh) and is in `util/gnu-baseline.json`; 104 new differential cases cover the cp/mv/ln changes, and the undo trash keeps every replaced destination (special files are recreated, never read).
