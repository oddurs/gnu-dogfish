---
id: 8
uid: 44cd79f9-13d0-4ccc-91b9-b85e6eefcfd8
title: "Run GNU coreutils' own test suite and publish the pass rate"
type: feature
status: done
milestone: "v0.3"
assignee: claude
claimed: 2026-10-29
closed_at: 2026-10-29
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

- [x] One command reproduces the run locally from a clean checkout.
- [x] Results are per test, with the failing ones listed, and the totals are published in the README.
- [x] CI fails on a regression against the committed baseline and can update it deliberately.
- [x] Every failing test is either fixed or filed as an item (grouped by tool).

## 2026-10-29

The first CI run reported 379/379: `make check-TESTS` relinked every swapped
`src/<tool>` from GNU's objects (newer than the Dogfish binary), so GNU was
tested against itself. Fixed with `make -o src/<tool>` and a check, before
and after the run, that every swapped program reports `GNU/Dogfish`. A
perfect score from a new compatibility harness should be treated as a bug.

## 2026-10-29

Triage of the 145 failures found no harness causes, and two data-loss bugs
(symlink moved or copied onto its referent; `--attributes-only` truncating a
linked file), fixed at once as 0025. The rest are filed as 0026–0056, with
the parser-level ones added to 0011 and the date ones to 0019.

## Result

`util/gnu-tests.sh` builds Dogfish and the pinned, checksum-verified GNU 9.4,
swaps Dogfish in for the 49 programs it implements, and runs the 417 GNU
tests whose programs under test are all Dogfish's. First result (CI run
40885586807): **229 of 374 passing (61.2%)**, 43 skipped, now the committed
baseline in `util/gnu-baseline.json` and in the README. `.github/workflows/gnu-tests.yml`
runs it on PRs that touch the code, on `main` and nightly; a test leaving the
baseline fails the job (checked locally with a doctored baseline: exit 1), and
`--update-baseline` rewrites it deliberately. Every failing test is in an item:
0011, 0019, 0025–0056. Local runs need a non-root user; this session's
sandbox could not configure GNU, so the end-to-end run is verified in CI only.
