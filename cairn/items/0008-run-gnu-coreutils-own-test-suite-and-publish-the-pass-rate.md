---
id: 8
uid: 44cd79f9-13d0-4ccc-91b9-b85e6eefcfd8
title: "Run GNU coreutils' own test suite and publish the pass rate"
type: feature
status: planned
milestone: "v0.3"
depends_on:
- 4
created: 2026-10-29
updated: 2026-10-29
priority: p1
effort: l
area: testing
---

## Problem

"Matches GNU" rests on 1,841 differential cases we wrote. GNU's own suite
(`tests/` in coreutils, ~700 shell and Perl tests accumulated over decades) is
the authority on the edge cases that matter, and a pass rate against it is the
one compatibility number a distribution will believe. uutils/coreutils
publishes exactly this.

## Proposal

A script (`util/gnu-tests.sh`, wrapped by `cargo xtask gnu-tests`) that:

1. fetches a pinned coreutils release tarball (9.4, matching our reference
   binaries) and verifies its checksum and signature;
2. configures and builds it once, so its test harness and helpers exist;
3. replaces each built `src/<tool>` that Dogfish implements with a symlink to
   the Dogfish binary;
4. runs `make check` for those tools' tests (`TESTS=...`), never as root, in a
   throwaway directory;
5. parses `tests/**/*.log` into PASS/FAIL/SKIP/ERROR per test and writes
   `gnu-results.json` plus a Markdown summary.

CI runs it on Linux nightly and on demand, uploads the results as an artifact,
and fails if the pass count drops below the committed baseline
(`util/gnu-baseline.json`). The README shows the current number.

## Acceptance criteria

- [ ] One command reproduces the run locally from a clean checkout.
- [ ] Results are per test, with the failing ones listed, and the totals are published in the README.
- [ ] CI fails on a regression against the committed baseline and can update it deliberately.
- [ ] Every failing test is either fixed or filed as an item (grouped by tool).
