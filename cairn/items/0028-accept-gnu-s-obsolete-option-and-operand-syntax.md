---
id: 28
uid: 6b613e5b-d858-4b0e-a18f-64d0d3fc451f
title: "Accept GNU's obsolete option and operand syntax"
type: bug
status: doing
assignee: claude
claimed: 2026-10-29
milestone: "v1.0"
depends_on:
- 8
- 11
created: 2026-10-29
updated: 2026-10-29
priority: p2
effort: m
area: compat
---

## What happens

Dogfish rejects the historical forms GNU still accepts: `head -1c`, `-2b`,
`-1k`; `tail +2c`, `-1l`, `-l`, `-b`, `+N`; `uniq -1`, `+1`; `sort +POS -POS`;
and `touch MMDDhhmm[YY] file` under `_POSIX2_VERSION=199209`.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/head/head.pl`
- `tests/tail/tail.pl`
- `tests/uniq/uniq.pl`
- `tests/sort/sort.pl`
- `tests/touch/obsolescent.sh`

## What should happen

Each tool accepts the obsolete forms exactly where GNU does (including the
`_POSIX2_VERSION` rules) and prints GNU's errors for the invalid ones.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/head/head.pl tests/tail/tail.pl tests/uniq/uniq.pl"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.

## 2026-10-29

touch's obsolete `MMDDhhmm[YY]` operand under `_POSIX2_VERSION=199209` is done (tests/touch/obsolescent.sh passes).

## 2026-10-29

head (`-1c`, `-2b`, `-1k`...), tail (`+2c`, `-1l`, `-b`, `+N`, `_POSIX2_VERSION`) and uniq (`-1`, `+1`) obsolete forms are done; head.pl, tail.pl and uniq.pl pass. Remaining: `sort +POS -POS`.
