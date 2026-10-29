---
id: 52
uid: 118ab315-e9e8-4a6c-abd8-3d01a0275370
title: "tac: -b, -r and -s; read errors on closed stdin"
type: bug
status: backlog
milestone: "v1.0"
depends_on:
- 8
created: 2026-10-29
updated: 2026-10-29
priority: p2
effort: m
area: compat
---

## What happens

`-b/--before`, `-r/--regex` and `-s/--separator` are missing. `tac - - <&-`
exits 0; GNU reports the read error and exits 1.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/tac/tac.pl`
- `tests/tac/tac-2-nonseekable.sh`

## What should happen

Both tests pass.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/tac/tac.pl tests/tac/tac-2-nonseekable.sh"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.
