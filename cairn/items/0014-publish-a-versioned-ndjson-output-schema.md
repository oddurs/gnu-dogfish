---
id: 14
uid: 26255f4d-3353-47ba-80d5-29011cffc507
title: "Publish a versioned NDJSON output schema"
type: docs
status: planned
milestone: "v0.3"
depends_on:
- 11
created: 2026-10-29
updated: 2026-10-29
priority: p1
effort: m
area: direction
---

## Reader and question

Someone writing a script or tool against `dogfish ls -J`, `du -J`, `df -J`,
`stat`, `wc --table -J`, `env -J`, or `dogfish query` asks: which fields exist,
what types and units are they, and will they change under me?

## Change

`doc/ndjson-schema.md` plus JSON Schema files per tool, versioned
(`schema: 1`), with a compatibility policy (fields added freely; removing or
changing one needs a new version). A test validates every `-J` output against
its schema.

## Acceptance criteria

- [ ] Every `-J` producer has a published schema, validated in CI.
- [ ] The compatibility policy is stated and referenced from the README.
