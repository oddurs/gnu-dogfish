---
id: 65
uid: d3891c1c-63db-4ad9-86cb-e41785fd20df
title: "Undo restores hard links that cp --preserve=links replaced"
type: bug
status: done
milestone: "v0.3"
created: 2026-10-30
updated: 2026-10-30
closed_at: 2026-10-30
priority: p1
effort: s
area: safety
---

## What happens

The undo model test failed on macOS: `cp -T -r d a; rm --safe c; cp -a -T d a`, then undoing all three, left `a/b` and `a/c` as two files where they had been one file with two links. `cp -a` (like GNU's) replaces a destination with more than one link instead of writing through it. Undo restored the directory in place and compared contents only, so two files that had the same bytes were left unlinked.

## What should happen

Undoing an in-place copy restores the link structure the backup records. Files that are hard links of each other in the backup are hard links again. Files that are not linked in the backup do not share an inode afterwards.

## Acceptance criteria

- [x] `restore_in_place` relinks files the backup links and separates files it does not, with unit tests for both directions.
- [x] The macOS seed is kept in `undo_model.proptest-regressions`.
