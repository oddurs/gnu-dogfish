---
id: 54
uid: e383a4b1-9dea-4a73-baa4-e2d7327e12de
title: "printf: reject field widths and precisions above INT_MAX"
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

`printf '%.*d' 2147483648 1` pads 2 GiB (the test times out); GNU says
`invalid precision` / `invalid field width` and exits 1.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/printf/printf-cov.pl`
- `tests/printf/printf.sh`

## What should happen

Both tests pass.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/printf/printf-cov.pl tests/printf/printf.sh"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.
