---
id: 22
uid: cf554b40-4407-429b-b5f7-1adb948ba0d1
title: "Track performance against GNU in CI"
type: chore
status: done
assignee: claude
closed_at: 2026-10-30
milestone: "v1.0"
created: 2026-10-29
updated: 2026-10-30
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

- [x] Benchmarks run on every merge to main with history kept.
- [x] A synthetic 20% slowdown is detected.

## Notes

- 2026-10-30: `.github/workflows/bench.yml` runs `util/bench.py --history` on each merge to main (REPEAT=5) and appends every run to `history.ndjson` on the `bench-history` branch. The check compares GNU/Dogfish ratios, not times, so a slower runner slows both sides. A ratio more than 15% below the median of the last ten runs on the same CPU model is measured again at twice the repeats, and fails the job only if it still regresses. `util/bench.py --self-test` runs in the job first: run-to-run noise passes, and a 20% slowdown in one benchmark is caught. A dedicated runner would allow a tighter threshold.
- 2026-10-30: the first run on main (9d42b3a) took 6 minutes, created `bench-history` and recorded 38/47 benchmarks at or above GNU. With no earlier runs on that CPU there was nothing to compare; later runs compare against it.
