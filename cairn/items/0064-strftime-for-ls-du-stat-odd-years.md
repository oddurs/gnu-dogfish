---
id: 64
uid: 8905dfbc-f171-4f2a-b8df-7ce794668114
title: "common/strftime: GNU's results for negative and huge years (ls, du, stat)"
type: bug
status: done
milestone: "v0.3"
created: 2026-10-30
updated: 2026-10-30
closed_at: 2026-10-30
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

- [x] Differential cases for ls, du and stat with negative and very large years match GNU.

## Notes

- 2026-10-30: common/strftime.rs now renders through common/date_format.rs, so date, ls, du and stat share one nstrftime. Beyond %C/%y/%g and %c/%x, the old copy did not pad %Y/%G/%F to four digits (`ls -l` showed year 1 as "1" where GNU shows "0001") and did not know the + flag. Differential runs of the shared formatter against GNU date also fixed E/O modifiers (gnulib hands `%O` numbers and `%EC`/`%Ey`/`%EY` to glibc's strftime; Dogfish now calls it the same way), the padding of unknown conversions, "%5%d", colons before something other than z, and flags on %F and %D. The cases (`odd_years::*` and `date_modifiers_*` in gnu_compat_dates.rs) put the files on /dev/shm when the target file system clamps times to 1901..2446 (ext4 with 128-byte inodes).
- Left: GNU date widens `%-N` to the clock resolution (`%9N`); Dogfish does not (ROADMAP.md).
