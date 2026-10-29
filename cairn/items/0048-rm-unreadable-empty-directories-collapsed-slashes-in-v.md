---
id: 48
uid: 054d61a4-eea6-4446-bcb3-42e34c1637e4
title: "rm: unreadable empty directories, collapsed slashes in -v"
type: bug
status: backlog
milestone: "v1.0"
depends_on:
- 8
created: 2026-10-29
updated: 2026-10-29
priority: p2
effort: s
area: compat
---

## What happens

`rm -rf` on an empty directory with mode 0100 fails as non-root; GNU falls
back to `rmdir` when `opendir` gives EACCES. `rm -rv a///` echoes `a///x`;
GNU prints `a/x` and `a/`.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/rm/unreadable.pl`
- `tests/rm/v-slash.sh`

## What should happen

Both tests pass (as non-root, as the suite runs).

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/rm/unreadable.pl tests/rm/v-slash.sh"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.
