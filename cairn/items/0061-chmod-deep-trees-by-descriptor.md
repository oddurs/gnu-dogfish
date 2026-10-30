---
id: 61
uid: f16c6de2-b4fb-4bcf-b58b-18334e77a9df
title: "chmod -R: walk trees deeper than 128 levels by descriptor"
type: bug
status: done
milestone: "v0.3"
created: 2026-10-29
updated: 2026-10-29
closed_at: 2026-10-29
priority: p2
effort: m
area: safety
---

## What happens

Past 128 levels chmod reaches entries by full path (`WalkPath::c_path`). That fails beyond `PATH_MAX`, and a symlink swapped into a directory on that path between checks could be followed.

## What should happen

chmod walks deep trees as rm and du do since 0039: relative to an open directory descriptor, recovering a closed parent through `..` with its device and inode checked (`DirFd::open_parent`).

## Reproduction

1. Make a 300-level tree of 200-byte names in a temporary directory and run `chmod -R u+w` on it: the deepest entries fail with ENAMETOOLONG.

## Acceptance criteria

- [x] `chmod -R` on a tree deeper than `PATH_MAX` changes every entry, matching GNU's output and status.
- [x] No path below the walk's root is resolved through a full path name at depth.

## Notes

- 2026-10-29: rm's release-and-reacquire `Parent` walk moved to `common/dirfd.rs`; chmod and chown now use it with `fchmodat`/`fchownat` at every depth (chown went by full path at every depth before). `chown -L` still walks by path, as GNU's does. A differential test runs 7 chmod/chown commands on 300 levels of 200-byte names against GNU.
