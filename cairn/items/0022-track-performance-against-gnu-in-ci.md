---
id: 22
uid: cf554b40-4407-429b-b5f7-1adb948ba0d1
title: "Track performance against GNU in CI"
type: chore
status: backlog
milestone: "v1.0"
created: 2026-10-29
updated: 2026-10-29
priority: p2
effort: m
area: performance
---

## Problem

Speed claims were once measured on one machine against BSD tools, and `seq`
silently got 2× slower in a rewrite. Nothing would catch the next regression.

## Proposal

A `hyperfine`-based benchmark set (a superset of `dogfish bench`) run on a
dedicated or consistent runner, results stored over time, and a CI check that
flags regressions beyond noise.

## Acceptance criteria

- [ ] Benchmarks run on every merge to main with history kept.
- [ ] A synthetic 20% slowdown is detected.
