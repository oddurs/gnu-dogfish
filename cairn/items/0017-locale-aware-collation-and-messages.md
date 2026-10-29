---
id: 17
uid: 9f26534a-9cf5-4150-87c1-e47cbc8a3986
title: "Locale-aware collation and messages"
type: feature
status: backlog
milestone: "v1.0"
created: 2026-10-29
updated: 2026-10-29
priority: p2
effort: xl
area: compat
---

## Problem

Dogfish compares bytes as under `LC_ALL=C`. In `en_US.UTF-8`, GNU `sort`, `ls`,
`uniq` and `comm` collate with `strcoll`, and messages are translated.

## Proposal

Collation through ICU4X (pure Rust, data compiled in or loaded per locale);
message catalogs in gettext `.po` format so GNU's existing translations can be
reused where licensing allows.

## Acceptance criteria

- [ ] Differential cases in at least `en_US.UTF-8` and one non-English locale pass for sort, ls, uniq.
- [ ] Messages are translatable and one translation ships.
