---
id: 53
uid: 5e4b51a0-002a-4f90-b60b-087731668fa3
title: "shuf: -r, large -i ranges, last -n wins"
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

Missing `-r/--repeat`; `-i1-SIZE_MAX` says `number too large`; `-n0` still opens
its input; a repeated `-n` is an error (0011) — which aborts the test's setup
(the ERROR result).

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/shuf/shuf.sh`

## What should happen

The test passes.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/shuf/shuf.sh"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.
