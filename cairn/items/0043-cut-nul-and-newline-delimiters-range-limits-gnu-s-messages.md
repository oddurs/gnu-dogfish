---
id: 43
uid: 4a59a3c4-f9b5-4ffd-a209-ccdbd0339795
title: "cut: NUL and newline delimiters, range limits, GNU's messages"
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

Beyond `-z` (0029), abbreviations (0011) and non-UTF-8 arguments (0011):
`-d ''` (NUL) and `-d '\n'` are wrong; `-b UINTMAX_MAX` is accepted (GNU: too
large); messages differ (`invalid field range`, `invalid byte or character
range`, `an input delimiter may be specified only when operating on fields`).

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/cut/cut.pl`
- `tests/cut/cut-huge-range.sh`

## What should happen

Both tests pass.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/cut/cut.pl tests/cut/cut-huge-range.sh"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.
