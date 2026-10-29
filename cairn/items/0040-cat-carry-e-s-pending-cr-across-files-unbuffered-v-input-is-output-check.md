---
id: 40
uid: ada47e88-e3e1-488f-8f78-29b4c56438fb
title: "cat: carry -E's pending CR across files, unbuffered -v, input-is-output check"
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

- `-E` loses a pending `\r` at a file boundary (`1\r` then `\n...` must print `1^M$`).
- `cat -v` buffers when input would block; GNU writes each chunk.
- `cat out >>out` succeeds; GNU refuses with `input file is output file`.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/cat/cat-E.sh`
- `tests/cat/cat-buf.sh`
- `tests/cat/cat-self.sh`

## What should happen

All three tests pass.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/cat/cat-E.sh tests/cat/cat-buf.sh tests/cat/cat-self.sh"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.
