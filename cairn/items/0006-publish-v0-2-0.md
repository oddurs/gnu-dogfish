---
id: 6
uid: 447d4a97-3be5-404a-ac72-f85bf75559d9
title: "Publish v0.2.0"
type: chore
status: planned
milestone: "v0.2"
depends_on:
- 4
- 5
created: 2026-10-29
updated: 2026-10-29
priority: p1
effort: s
area: release
---

## Purpose

Ship the safety and GNU-compatibility work as v0.2.0 with release archives.

## Approach

Merge the `chore: release 0.2.0` PR that Release Please opens. Before merging,
fold the hand-written "Unreleased" section of `CHANGELOG.md` into the generated
0.2.0 entry: Release Please only lists commit subjects, and the squash of #1 is
a single line.

## Acceptance criteria

- [ ] The v0.2.0 tag exists and the GitHub Release has both archives and their `.sha256` files.
- [ ] `CHANGELOG.md` has one 0.2.0 section with the detailed notes and no leftover "Unreleased".
- [ ] A downloaded Linux archive's binary runs `dogfish --version` on a machine other than the CI runner.
