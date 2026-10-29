---
id: 19
uid: 7bcdcdca-c770-4ed5-a1d9-638440067df8
title: "Full GNU date-string grammar"
type: feature
status: backlog
milestone: "v1.0"
created: 2026-10-29
updated: 2026-10-29
priority: p2
effort: l
area: compat
---

## Problem

`date -d` and `touch -d` support the common formats, not all of gnulib's
`parse_datetime`: no `TZ="..."` prefix, no military zones, no unusual zone
abbreviations, no `date --debug`.

## Proposal

A faithful port of the parse_datetime grammar with GNU's tests as the corpus,
fuzzed differentially (0012).

## Acceptance criteria

- [ ] GNU's date tests pass; differential fuzzing finds no divergence in an hour.
