---
id: 14
uid: 26255f4d-3353-47ba-80d5-29011cffc507
title: "One structured-output contract: NDJSON everywhere, versioned schemas"
type: feature
status: planned
milestone: "v0.3"
depends_on:
- 11
created: 2026-10-29
updated: 2026-10-30
priority: p1
effort: l
area: direction
---

## Problem

`-J/--json` is Dogfish's main difference from GNU, but today it is a
collection of per-tool choices. An audit on `bae578a`:

- NDJSON (one object per line): ls, stat, df, du, wc, nproc, env, realpath,
  md5sum/sha256sum/b2sum. head, tail, sort and uniq consume NDJSON.
- Not NDJSON: `id`, `uname`, `printenv`, `date` and `history` print
  multi-line pretty JSON; `seq -J` prints a JSON array. None of them can
  feed `dogfish query`.
- No `-J`: cp, mv, rm, mkdir, ln, chmod, chown, touch (they act on files
  but cannot report what they did), basename, dirname, mktemp, cat, tr, cut.
- Inconsistent fields: times are strings in one format in stat, relative
  text (`"modified_relative": "0s ago"`) in ls; sizes come with
  human-readable duplicates; similar things have different names.
- No schema, version or compatibility promise, and no test that holds any
  of it stable.

## Proposal

One contract, implemented once in dogfish-core and used by every tool:

- **Framing**: NDJSON only. One JSON object per line, UTF-8, `\n`-terminated,
  no pretty printing, no arrays at top level; streamed as produced.
- **Types**: sizes and counts are integers in bytes/units; times are RFC 3339
  strings in UTC with nanoseconds (`2026-10-29T18:03:19.123456789Z`) plus an
  integer `*_ns` epoch field; modes as an octal string and a symbolic string;
  paths as given (with a `path_bytes` base64 field only when the path is not
  valid UTF-8). No presentation fields (human sizes, relative times, colours)
  in JSON.
- **Names**: snake_case, shared across tools (`path`, `size`, `mtime`, ...).
- **Actions**: tools that change the file system report one record per action
  with `-J` (`{"action":"copy","source":...,"dest":...,"bytes":...}`,
  `"remove"`, `"backup"`, `"link"`, `"mkdir"`, `"chmod"`, ...), the structured
  form of `-v`, written as each action completes.
- **Errors**: a diagnostic stays on stderr in GNU's form; with `-J` an
  `{"error":...,"path":...,"errno":...}` record is also written to stdout so a
  pipeline sees the failure in order.
- **Schemas**: one JSON Schema per tool under `doc/schema/`, version `1`,
  printed by `dogfish schema TOOL`; `doc/ndjson.md` states the compatibility
  policy (fields may be added; removing, renaming or retyping one needs a new
  version, selected with `--json=2`).
- `-J` is an extension GNU does not have; without it every tool stays
  GNU-exact. ROADMAP.md says so.

## Acceptance criteria

- [ ] Every tool that produces records supports `-J` with NDJSON framing, and every file-changing tool reports its actions with `-J`.
- [x] Every `-J` producer has a published JSON Schema (version 1), and CI validates real output of every tool against it.
- [x] `dogfish query`, `head`, `tail`, `sort`, `uniq` and `cut` consume any producer's output.
- [x] `doc/ndjson.md` states the contract and compatibility policy, linked from the README.

## Notes

- 2026-10-30: phase 2 (branch `ndjson-contract-2`). cp, mv, rm, mkdir, ln, chmod, chown and touch take `-J`: one action record per `-v` line, in `-v`'s order, instead of the text (`doc/ndjson.md`, "Action records"); with -J every diagnostic is also an error record on stdout, made once in `gnu_error!` (`ndjson::diagnostic`), stderr unchanged. Schemas for the eight tools and `error.v1.json`; every schema accepts the error record; tests/ndjson.rs validates real output of all of them and holds -J's records to -v's lines and -J's stderr to GNU's.
- Criterion 1 is still open: tools that produce text GNU-style but no records yet (basename, dirname, mktemp, cat, tr, cut as a producer), and `mktemp`, `tee`, `dogfish undo` change files without action records.
