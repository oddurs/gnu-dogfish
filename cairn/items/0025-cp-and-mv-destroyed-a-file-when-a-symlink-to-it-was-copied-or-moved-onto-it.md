---
id: 25
uid: c1f595b1-e963-462e-adbb-c51511e78f7a
title: "cp and mv destroyed a file when a symlink to it was copied or moved onto it"
type: bug
status: done
milestone: "v0.2"
depends_on:
- 8
created: 2026-10-29
updated: 2026-10-29
closed_at: 2026-10-29
priority: p0
effort: m
area: safety
---

## What happens

Found by GNU's own suite (0008). With `s -> f`:

- `mv s f`, `cp -a s f`, `cp -d s f` and `cp --remove-destination -d s f`
  replaced `f` with a symlink to itself and exited 0. The data was gone
  (only the undo trash kept it). `mv` did the same through a chain (`s2 -> s -> f`).
- `cp -a --attributes-only a b` truncated `b` when `b` had another hard link.

Dogfish compared only the `lstat` inodes of source and destination, so it
never saw that the source resolved to the destination; and it removed a
multiply-linked destination for `--preserve=links` even when no data was copied.

The other side of the same rule was too strict: `mv -b h1 h2` (hard links),
`mv -b s1 s2` (hard links to one symlink), `cp -b f s` and `cp -sf f s`
were refused as "the same file", where GNU makes the backup or recreates the link.

## What should happen

Exactly GNU's `same_file_ok` decisions: refuse whenever the copy or move would
lose the file, allow it whenever a backup or a remaining hard link keeps it.

## Reproduction

1. `cd "$(mktemp -d)" && echo data > f && ln -s f s && dogfish mv s f && cat f` → `Too many levels of symbolic links`.

GNU tests: `tests/mv/symlink-onto-hardlink.sh`, `tests/mv/symlink-onto-hardlink-to-self.sh`, `tests/cp/attr-existing.sh`.

## Acceptance criteria

- [x] A regression check reproduces the failure and passes with the fix.
- [x] cp and mv share one port of GNU's `same_file_ok`.
- [x] Every symlink/hard-link combination tried matches GNU (22 cp and 12 mv cases).

## Result

`same_file_ok` in `fs_ops.rs` is a line-for-line port of GNU's `copy.c`, called
by cp (with GNU's stat-or-lstat choice of destination) and by mv. `cp
--attributes-only` no longer removes the destination for `--preserve=links`
or `-d`, as in GNU, where those removals need a data copy. 18 differential
cases cover it in `gnu_compat_fileops.rs` (`mv_symlink_*`, `mv_*hard_links`,
`cp_*symlink*`, `cp_backup_hard_links`, `cp_attributes_only_keeps_linked_contents`).
14 of them fail before the fix; all pass
after, and the full suite (1,933 tests) passes as root and as non-root. In GNU's
suite (run 40894914076) six tests went from failing to passing:
`cp/attr-existing`, `mv/symlink-onto-hardlink`, `mv/symlink-onto-hardlink-to-self`,
`mv/hard-4`, `mv/part-symlink` and `mv/into-self-2`: 235/374, no regressions.
