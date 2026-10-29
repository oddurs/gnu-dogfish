---
id: 58
uid: 6f790aea-27c4-42e6-b6f9-38312238e091
title: "rm --safe and undo hang on a FIFO across file systems"
type: bug
status: done
milestone: "v0.3"
assignee: claude
claimed: 2026-10-29
created: 2026-10-29
updated: 2026-10-29
closed_at: 2026-10-29
priority: p1
effort: s
area: safety
---

## What happens

When the trash (`DOGFISH_HOME`) is on another file system than the file,
moving it there falls back to copy + remove (`move_path` → `copy_tree` in
dogfish-core). `copy_tree` copied every non-directory, non-symlink by opening
and reading it, so `rm --safe` of a FIFO blocked forever, and a device node
such as `/dev/zero` would have been copied without end. Undo goes through
the same path. Reported by the cp/mv agent (which no longer sends special
files there).

## What should happen

Special files (FIFOs, sockets, device nodes) are recreated with `mknod` at
the destination, never opened.

## Reproduction

1. `D=$(mktemp -d -p /dev/shm); mkfifo $D/p; DOGFISH_HOME=$(mktemp -d) timeout 10 dogfish rm --safe $D/p` → exit 124 (hung) on `bae578a`.

## Acceptance criteria

- [x] A regression check reproduces the failure and passes with the fix.
- [x] `rm --safe` then `undo` round-trips a FIFO across file systems.

## Result

`copy_tree` recreates special files with `mknod` (type, mode and device
number from `lstat`). Unit tests copy a FIFO alone and inside a tree; with no
writer, reading it would block, so the old code hangs them. The reproduction
above now exits 0 and `undo` restores the FIFO as a FIFO.
