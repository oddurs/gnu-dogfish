---
id: 18
uid: 942f4924-c9f1-4f35-990c-6201ab005d8d
title: "Extended precision in seq and printf"
type: feature
status: done
milestone: "v1.0"
created: 2026-10-29
updated: 2026-10-30
closed_at: 2026-10-30
priority: p2
effort: m
area: compat
---

## Problem

GNU uses `long double`; Dogfish uses f64, so extreme values differ
(`seq -f %g 1e20 1e20`), and `printf %a` is unsupported.

## Proposal

A small extended-precision (x87 80-bit semantics on x86, IEEE quad elsewhere,
matching GNU per platform) or arbitrary-precision decimal path for these tools.

## Acceptance criteria

- [x] The extreme cases in ROADMAP.md match GNU; `printf %a` is supported.

## Notes

- 2026-10-30: `common/ldbl` is a software `long double` in the platform's format: x87 80-bit on x86 and x86-64, IEEE binary128 where glibc uses it (64-bit Arm Linux, RISC-V, LoongArch, IBM Z), `double` elsewhere (Apple silicon, 32-bit Arm); PowerPC's double-double is not modelled (ROADMAP.md). It holds sort's glibc `strtold` port, now generic over the format and reporting `ERANGE` (overflow, inexact tiny results, tininess after rounding on x86), add and multiply with round-to-nearest-even and x86's negative default NaN, and ports of glibc's `printf_fp` and `printf_fphex` (`%a` prints `0x8p-3` style for the 80-bit format). sort keeps the 80-bit format on every platform, unchanged in behaviour and speed.
- seq follows seq.c: format errors first, both `seq_fast` attempts (digit operands, then "%.0Lf" spellings and `inf`), the zero-increment check before LAST is scanned, `first + i*step` and the extra-number check in `long double`. The exact-i64 shortcut is kept for the default "%.0Lf" format only (the first version also took user formats and broke tests/seq/seq.pl). printf reports `Numerical result out of range` as `vstrtold` does, and an empty numeric argument is 0 without a diagnostic.
- Checked against glibc beyond the test suite: 50,000 random x87 additions and multiplications against C `long double`, 10,000 random `%a/%e/%f/%g` conversions each for `double` (glibc printf) and binary128 (libquadmath), and `util/ldbl-fuzz.py` (printf and seq against GNU, over 100,000 cases per run, no divergence). `seq 10M` unchanged, `seq -w 0 0.5 2000000` about 20% faster than before.
