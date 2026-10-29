---
id: 7
uid: 7a944362-0e1f-421b-8176-00a0a271b8f1
title: "Review the major GitHub Action bumps"
type: chore
status: planned
milestone: "v0.2"
created: 2026-10-29
updated: 2026-10-29
priority: p2
effort: s
area: release
---

## Purpose

Dependabot opened #2–#6, all major Action versions, which correctly were not
auto-merged: actions/checkout 5→7, release-please-action 4→5,
action-gh-release 2→3, fetch-metadata 2→3, action-semantic-pull-request 5→6.
release-please-action v4 runs on the deprecated Node 20.

## Approach

Read each changelog for breaking inputs and outputs the workflows use
(`fromJSON(steps.release.outputs.pr).headBranchName`, `updated-dependencies-json`,
`generate_release_notes`), adjust, and merge one at a time so a failure is
attributable.

## Acceptance criteria

- [ ] Each bump merged or closed with its reason recorded here.
- [ ] No workflow warns about Node 20.
