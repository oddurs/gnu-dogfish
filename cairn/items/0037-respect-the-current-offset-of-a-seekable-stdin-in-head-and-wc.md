---
id: 37
uid: 47330a95-326e-4015-8e00-7c495988ac8b
title: "Respect the current offset of a seekable stdin in head and wc"
type: bug
status: planned
milestone: "v0.3"
depends_on:
- 8
created: 2026-10-29
updated: 2026-10-29
priority: p1
effort: s
area: compat
---

## What happens

`(head -n 1; cat) < file` loses data: head reads ahead and never seeks back,
so `cat` sees nothing. `head -c -4` from a non-zero offset is wrong. `wc -c`
on a 1 TiB sparse file reads it into memory and fails with `out of memory`;
GNU uses the size from `fstat` minus the current offset.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/head/head-pos.sh`
- `tests/head/head-c.sh`
- `tests/wc/wc-proc.sh`

## What should happen

head leaves the offset just after what it printed, as GNU does; `wc -c` on a
regular file is O(1); all listed tests pass.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/head/head-pos.sh tests/head/head-c.sh tests/wc/wc-proc.sh"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.
