---
id: 64
uid: 8905dfbc-f171-4f2a-b8df-7ce794668114
title: "common/strftime: GNU's results for negative and huge years (ls, du, stat)"
type: bug
status: planned
milestone: "v0.3"
created: 2026-10-30
updated: 2026-10-30
priority: p3
effort: s
area: compat
---

## What happens

`common/strftime.rs`, the formatter behind `ls --time-style=+FMT`, `du --time-style` and `stat`'s `%w`-style output, is separate from `date`'s and probably has the bugs the date fuzzer found there (0019): `%C`/`%y`/`%g` round negative years down where gnulib truncates toward zero, and `%c`/`%x` follow gnulib instead of glibc (unpadded `%Y` that wraps past INT_MAX).

## What should happen

Both formatters agree with GNU for every year; ideally they are one formatter.

## Reproduction

1. `touch -d @-12219292800000 f` (a negative year), then compare `ls -l --time-style=+%C.%y.%g f` and `du --time --time-style=+%C.%y f` with GNU's.

## Acceptance criteria

- [ ] Differential cases for ls, du and stat with negative and very large years match GNU.
