---
id: 47
uid: 414040d2-e0a5-4141-8811-8d5acbcde595
title: "mv: remaining GNU test failures"
type: bug
status: done
assignee: claude
claimed: 2026-10-29
closed_at: 2026-10-29
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

- `mv -T --backup=numbered C E/` builds the backup name from `E/`; strip trailing slashes.
- For an unwritable destination the cp-fallback asks `overwrite 'f'?` instead of
  `replace 'f', overriding mode 0000 (---------)?`, and `-f` does not unlink and retry.
- `cp f no-such/` reports `Is a directory`; GNU says `Not a directory`.
- `--update=none --update=all` keeps `none`; the last `--update` wins.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/mv/backup-dir.sh`
- `tests/mv/i-2.sh`
- `tests/mv/trailing-slash.sh`
- `tests/mv/update.sh`

## What should happen

All listed tests pass.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/mv/backup-dir.sh tests/mv/i-2.sh tests/mv/trailing-slash.sh"`, as a non-root user.

## Acceptance criteria

- [x] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [x] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.

## Result

Fixed by porting GNU's copy.c, cp-hash.c, ln.c, force-link.c and backupfile.c into `common/copy.rs` (shared by cp and mv) and ln: backup names strip trailing slashes, the cp-fallback prompt and `-f` retry, ENOTDIR wording, and the last `--update` wins. Every listed GNU test passes (util/gnu-test-one.sh) and is in `util/gnu-baseline.json`; 104 new differential cases cover the cp/mv/ln changes, and the undo trash keeps every replaced destination (special files are recreated, never read).
