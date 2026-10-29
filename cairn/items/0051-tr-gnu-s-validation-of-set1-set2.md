---
id: 51
uid: 5cfbea6d-6af5-48b7-a09c-c91c5e36de00
title: "tr: GNU's validation of SET1/SET2"
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

`[:*3]` is mis-parsed; `[b*08]` is not rejected (`invalid repeat count`); no
`unescaped backslash at end of string` warning; `-cs '[:upper:]' 'X[Y*]'` is
not rejected; `[==]` and `[::]` lack GNU's messages; a SET2 ending in a class
when SET1 is longer is not rejected.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/tr/tr.pl`
- `tests/tr/tr-case-class.sh`

## What should happen

Both tests pass.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/tr/tr.pl tests/tr/tr-case-class.sh"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.
