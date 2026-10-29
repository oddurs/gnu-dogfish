---
id: 36
uid: 18edcb89-9d04-4567-9ea9-10ac75b432eb
title: "Make cross-device mv complete: hard links, --no-copy, FIFOs, failure messages"
type: bug
status: done
assignee: claude
claimed: 2026-10-29
closed_at: 2026-10-29
milestone: "v0.3"
depends_on:
- 8
created: 2026-10-29
updated: 2026-10-29
priority: p1
effort: l
area: safety
---

## What happens

When `rename(2)` fails with EXDEV, mv copies and removes. That fallback does
not keep hard links between the moved files, ignores `--no-copy`, opens and
reads a FIFO (hanging), and reports `cannot back up ... before overwriting`
where GNU says `inter-device move failed: ... unable to remove target`.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/mv/hard-link-1.sh`
- `tests/mv/part-hardlink.sh`
- `tests/mv/no-copy.sh`
- `tests/mv/mv-special-1.sh`
- `tests/mv/part-fail.sh`

## What should happen

The copy fallback preserves hard links like `cp -a`, honours `--no-copy`,
recreates special files, and matches GNU's messages. All listed tests pass.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/mv/hard-link-1.sh tests/mv/part-hardlink.sh tests/mv/no-copy.sh"`, as a non-root user.

## Acceptance criteria

- [x] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [x] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.

## Result

Fixed by porting GNU's copy.c, cp-hash.c, ln.c, force-link.c and backupfile.c into `common/copy.rs` (shared by cp and mv) and ln: the cross-device copy fallback preserves hard links, honours `--no-copy`, recreates special files and reports GNU's messages. Every listed GNU test passes (util/gnu-test-one.sh) and is in `util/gnu-baseline.json`; 104 new differential cases cover the cp/mv/ln changes, and the undo trash keeps every replaced destination (special files are recreated, never read).
