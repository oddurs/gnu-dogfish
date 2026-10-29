---
id: 12
uid: 8e274512-9d60-4754-817f-d67c3a651085
title: "Fuzz every parser"
type: feature
status: done
milestone: "v0.3"
depends_on:
- 11
created: 2026-10-29
updated: 2026-10-29
closed_at: 2026-10-29
priority: p1
effort: m
area: security
---

## Problem

The riskiest code is the hand-written grammars: `date -d`/`touch -d`, `printf`
formats, `sort -k` keys, `tr` sets, `chmod` modes, `LS_COLORS`, `--block-size`,
`seq` numbers, base64/basenc decoding. They take untrusted input and were
written quickly.

## Proposal

`cargo-fuzz` targets per parser in `fuzz/`, with seed corpora from the
differential and GNU suites; a differential fuzz target that runs Dogfish and
GNU on generated inputs and reports the first divergence; a CI job that fuzzes
each target for a few minutes per run and keeps the corpus.

## Acceptance criteria

- [x] A fuzz target exists for every parser listed above.
- [x] Every crash or divergence found is fixed with a regression case.
- [x] CI runs a time-boxed fuzz pass on schedule.

## Notes

- 2026-10-29: 13 cargo-fuzz targets in `fuzz/` (its own workspace), seed corpora, `cargo xtask fuzz`, and a weekly workflow (300 s per target). About four minutes per target found and fixed overflows in printf widths, ls `--time-style` widths, duration and size parsing.
