---
id: 28
uid: 6b613e5b-d858-4b0e-a18f-64d0d3fc451f
title: "Accept GNU's obsolete option and operand syntax"
type: bug
status: done
milestone: "v1.0"
depends_on:
- 8
- 11
created: 2026-10-29
updated: 2026-10-30
closed_at: 2026-10-30
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

- [x] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [x] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.

## 2026-10-29

touch's obsolete `MMDDhhmm[YY]` operand under `_POSIX2_VERSION=199209` is done (tests/touch/obsolescent.sh passes).

## 2026-10-29

head (`-1c`, `-2b`, `-1k`...), tail (`+2c`, `-1l`, `-b`, `+N`, `_POSIX2_VERSION`) and uniq (`-1`, `+1`) obsolete forms are done; head.pl, tail.pl and uniq.pl pass. Remaining: `sort +POS -POS`.

## 2026-10-30

`sort +POS1 [-POS2]` was already ported from sort.c's main and `parse_field_count` (with the `_POSIX2_VERSION` rule: obsolete keys only outside 200112..200809, or when the next argument is `-DIGIT...` and POSIXLY_CORRECT is unset; positions are zero-based, and `-B.0` ends at field B as `-k ...,B`). Added differential cases (`sort_obsolete_keys_*` in gnu_compat_text.rs): sort.pl's obsolete cases under `_POSIX2_VERSION=199209`, the default version, 200112 and the invalid forms. The five listed tests pass and are in the baseline, as does tests/sort/sort-debug-warn.sh (the obsolete-key warning).
