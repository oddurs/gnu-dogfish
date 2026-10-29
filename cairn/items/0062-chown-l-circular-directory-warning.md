---
id: 62
uid: 0ee2712f-51c9-47e8-a871-d30c07b87c06
title: "chown -L: detect directory cycles as GNU does"
type: bug
status: planned
milestone: "v0.3"
created: 2026-10-29
updated: 2026-10-29
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

- [ ] A differential case against GNU for `chown -RL` (and `chmod -RL` if it shares the walk) on a looping tree.
