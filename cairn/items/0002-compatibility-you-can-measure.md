---
id: 2
uid: a8354515-838a-4f8d-81dd-050e127eb906
key: "v0.3"
title: "Compatibility you can measure"
type: milestone
status: planned
created: 2026-10-29
updated: 2026-10-29
---

Turn "compatible with GNU" from a claim into a number anyone can reproduce,
and reshape the code so that number can keep rising without regressions:
GNU's own test suite runs against Dogfish in CI with a published pass rate,
every tool lives in its own module on one shared GNU-compatibility layer, the
parsers are fuzzed, the undo system is tested against a model, and the NDJSON
output that makes Dogfish different is a versioned, documented contract.

## Release gate

- GNU test-suite pass rate published and tracked in CI (0008).
- One option parser, one quoting implementation, one error/exit-code path (0011).
- Fuzz targets for every parser and model-based undo tests running in CI (0012, 0013).
- NDJSON schema 1 published (0014).
