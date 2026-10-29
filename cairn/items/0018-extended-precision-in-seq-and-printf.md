---
id: 18
uid: 942f4924-c9f1-4f35-990c-6201ab005d8d
title: "Extended precision in seq and printf"
type: feature
status: backlog
milestone: "v1.0"
created: 2026-10-29
updated: 2026-10-29
priority: p2
effort: m
area: compat
---

## Problem

GNU uses `long double`; Dogfish uses f64, so extreme values differ
(`seq -f %g 1e20 1e20`), and `printf %a` is unsupported.

## Proposal

A small extended-precision (x87 80-bit semantics on x86, IEEE quad elsewhere,
matching GNU per platform) or arbitrary-precision decimal path for these tools.

## Acceptance criteria

- [ ] The extreme cases in ROADMAP.md match GNU; `printf %a` is supported.
