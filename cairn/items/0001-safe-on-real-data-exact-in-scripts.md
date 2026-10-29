---
id: 1
uid: 2232f42d-4daf-4573-a157-5d4c3f4d5f9f
key: "v0.2"
title: "Safe on real data, exact in scripts"
type: milestone
status: doing
created: 2026-10-29
updated: 2026-10-29
---

The first release where Dogfish can stand in for GNU coreutils 9.4 in scripts:
GNU's output, messages and exit codes whenever output is not a terminal, the
data-loss bugs of 0.1.0 fixed, and portable, automatically published release
archives. The work itself merged in #1 (`53427e4`); what remains is shipping it.

## Release gate

- `main` builds and CI is green (see 0004).
- The release automation can open its PR (0005), and the `chore: release 0.2.0`
  PR carries the full CHANGELOG notes (0006).
- Release archives for Linux x86_64 and macOS arm64 are attached with SHA-256 sums.
