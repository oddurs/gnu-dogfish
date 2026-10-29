---
id: 7
uid: 7a944362-0e1f-421b-8176-00a0a271b8f1
title: "Review the major GitHub Action bumps"
type: chore
status: done
assignee: claude
claimed: 2026-10-29
closed_at: 2026-10-29
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

- [x] Each bump merged or closed with its reason recorded here.
- [x] No workflow warns about Node 20.

## Result

Each bump was checked against its upstream release notes and this repo's use
of its inputs and outputs; every one only moved to the Node 24 runtime.
Merged: #2 dependabot/fetch-metadata v3, #3 amannn/action-semantic-pull-request
v6, #4 googleapis/release-please-action v5, #5 actions/checkout v7 (v7 blocks
fork checkouts in `pull_request_target`, which no workflow here does), #6
softprops/action-gh-release v3. #3–#6 were retitled `ci: ...` first: they
predate the Dependabot commit prefix and failed the PR-title check. #16 moved
the GNU workflow (newer than #5) to checkout v7, actions/cache v5 and
actions/upload-artifact v6, the Node 24 majors that need no input changes
(upload-artifact v7 changes upload behaviour). Closed #13, the RustCrypto
0.11 group, which does not compile; see 0015.
