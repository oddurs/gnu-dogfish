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
  Failing in 0008's first run: `tests/date/date.pl` (`TZ="EST5"` inside the
  string with `TZ=PST8`, military zone `09:00T`, plus a non-UTF-8 argument,
  0011), `tests/date/date-tz.sh` (an embedded 2,000-character `TZ="..."`), and
  `tests/date/date-debug.sh` (`--debug`, 0035; `Sun, 90-12-11 + 3 days - 90
  minutes`).
