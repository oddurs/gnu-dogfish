---
id: 62
uid: 0ee2712f-51c9-47e8-a871-d30c07b87c06
title: "chown -L: detect directory cycles as GNU does"
type: bug
status: done
milestone: "v0.3"
created: 2026-10-29
updated: 2026-10-30
closed_at: 2026-10-30
priority: p3
effort: s
area: compat
---

## What happens

`chown -RL` on a tree with a symlink back to an ancestor follows the loop until ELOOP or ENAMETOOLONG. GNU's fts detects the cycle and warns (`chown: WARNING: Circular directory structure.` with the directory named).

## What should happen

Cycle detection by device and inode of the directories on the current path, with GNU's warning and exit status.

## Reproduction

1. In a temporary directory: `mkdir -p d/e; ln -s .. d/e/up; chown -RLv "$(id -u)" d`.

## Acceptance criteria

- [x] A differential case against GNU for `chown -RL` (and `chmod -RL` if it shares the walk) on a looping tree.

## Notes

- 2026-10-30: GNU never warns here. fts in logical mode keeps a hash of the directories being walked (fts-cycle.c `enter_dir`/`leave_dir`); one already in it is FTS_DC, which chown changes like a file but does not enter, and `cycle_warning_required` (xfts.c) is false under FTS_LOGICAL, so no "Circular directory structure" warning and exit status 0. The warning is only for a cycle in a physical walk (-P, or -H below the operands), i.e. a corrupted file system or a bind mount; Dogfish does not port that lazy `cycle_check` yet (untestable unprivileged).
- chown's walk now keeps the same dev/ino set under -L. chmod has no -L in GNU either (checked); du -L and cp -RL already matched GNU on these trees. Differential cases: `chown_dereference_directory_cycles`, `cp_dereference_directory_cycle*`, `chmod_recursive_dereference_is_no_option` in gnu_compat_fileops.rs.
- Seen in passing: GNU `chown -RLv` on a dangling symlink prints "failed to change ownership of 'x' from <garbage> to N" (an uninitialised stat); Dogfish prints it without "from"; not matchable byte for byte.
