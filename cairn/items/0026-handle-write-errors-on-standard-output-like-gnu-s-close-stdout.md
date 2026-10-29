---
id: 26
uid: 9921a997-5415-4b3f-a23f-e0deac847e4f
title: "Handle write errors on standard output like GNU's close_stdout"
type: bug
status: planned
milestone: "v0.3"
depends_on:
- 8
- 11
created: 2026-10-29
updated: 2026-10-29
priority: p1
effort: m
area: compat
---

## What happens

Writing to a full or closed standard output goes wrong four ways:

- `println!` panics on ENOSPC (`true --version >/dev/full` aborts, exit 134);
- a closed stdout (`printf foo >&-`, `cp --verbose >&-`, `mktemp >&-`) exits 0,
  and `mktemp` leaves its file behind;
- the message is `tool: No space left on device` where GNU prints
  `tool: write error: No space left on device` (or `standard output: ...` for `yes`);
- `paste /dev/zero >/dev/full` and `head -n -N` from an endless pipe never stop.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/misc/false-status.sh`
- `tests/misc/close-stdout.sh`
- `tests/misc/yes.sh`
- `tests/misc/write-errors.sh`
- `tests/head/head-write-error.sh`

## What should happen

Every write to stdout goes through one writer that reports the first error
with GNU's wording and exit status, and every tool flushes and checks it at
exit, as gnulib's `close_stdout` does. Tools that loop stop on the first
write error.

The error path belongs in the `gnu::error` part of the compatibility layer (0011).

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/misc/false-status.sh tests/misc/close-stdout.sh tests/misc/yes.sh"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.
