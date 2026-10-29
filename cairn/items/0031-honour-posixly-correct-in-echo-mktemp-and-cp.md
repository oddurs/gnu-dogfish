---
id: 31
uid: 88e91031-5221-4544-b0fe-dccb48b5e493
title: "Honour POSIXLY_CORRECT in echo, mktemp and cp"
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

With `POSIXLY_CORRECT` set, GNU `echo` treats `-nE`, `-E -n` and `--version` as
text, `mktemp` stops parsing options at the first operand, and `cp` writes
through a dangling destination symlink. Dogfish ignores the variable.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/misc/echo.sh`
- `tests/misc/mktemp.pl`
- `tests/cp/thru-dangling.sh`

## What should happen

The three behaviours match GNU with and without `POSIXLY_CORRECT`.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/misc/echo.sh tests/misc/mktemp.pl tests/cp/thru-dangling.sh"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.
