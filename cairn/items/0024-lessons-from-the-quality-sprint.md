---
id: 24
uid: eff342e3-7f0f-4cd5-8058-f60d36cc5972
title: "Lessons from the quality sprint"
type: decision
status: done
created: 2026-10-29
updated: 2026-10-29
closed_at: 2026-10-29
priority: p1
effort: s
area: direction
---

## Context

The 0.2 quality sprint (#1) took Dogfish from "fast, flashy, unsafe" to
GNU-exact in pipes with 1,915 tests. It also surfaced how the work should
have been ordered. Recorded so the next phase does not repeat it.

## Options and tradeoffs

What we did versus what, in hindsight, we would do:

- Differential testing was built after the first round of fixes; it found more
  bugs than anything else and should come first, together with GNU's own suite (0008).
- Parallel agents rewrote tools in the same 8,000-line file and each grew its
  own option parser and quoting; the shared GNU layer and one module per tool
  should exist before parallel work starts (0010, 0011).
- Most CI failures were environment differences (macOS types, non-root, APFS,
  BSD tools, toolchain drift) that a local CI-equivalent catches (0009).
- The undo journal was redesigned three times reactively; destructive features
  need written invariants and model-based tests from the start (0013).
- Performance claims were measured on the wrong baseline; measure against GNU
  on Linux and track it (0022).
- Automation needs the same rigour as code: the Dependabot auto-merge trusted a
  semver label Cargo does not share and merged without CI (0004).

## Decision

Work the v0.3 milestone in this order: GNU test suite (0008), local CI
equivalent (0009), one module per tool (0010), one compatibility layer (0011),
then fuzzing (0012), model-based safety tests (0013) and the NDJSON schema
(0014). Each step leaves every suite green and the GNU pass count no lower.

## Revisit when

The GNU pass rate is known (0008) or the uutils decision (0016) changes the scope.

## Acceptance criteria

- [x] The choice, its evidence, and its consequences are recorded.
