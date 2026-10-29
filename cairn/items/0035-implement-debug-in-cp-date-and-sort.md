---
id: 35
uid: 33581729-a7b4-45aa-88fe-f2eb63d7d8ff
title: "Implement --debug in cp, date and sort"
type: bug
status: backlog
milestone: "v1.0"
depends_on:
- 8
created: 2026-10-29
updated: 2026-10-29
priority: p3
effort: m
area: compat
---

## What happens

`--debug` is accepted but prints nothing: cp should report its copy strategy
(`copy offload`, `reflink`, `sparse detection`), date its parse, sort the key
underlining and its warnings.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/cp/debug.sh`
- `tests/date/date-debug.sh`
- `tests/sort/sort-debug-keys.sh`
- `tests/sort/sort-debug-warn.sh`

## What should happen

The three `--debug` outputs match GNU's.

date's part needs the full date grammar (0019).

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/cp/debug.sh tests/date/date-debug.sh tests/sort/sort-debug-keys.sh"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.
