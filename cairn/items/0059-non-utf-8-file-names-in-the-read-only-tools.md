---
id: 59
uid: f16684ce-763f-41f4-b4ca-83630a77c87c
title: "Non-UTF-8 file names in the read-only tools"
type: bug
status: done
milestone: "v0.3"
created: 2026-10-29
updated: 2026-10-30
closed_at: 2026-10-30
priority: p1
effort: m
area: compat
---

## What happens

cat, head, tail, wc, ls, stat, du, sort and about 17 other tools turn an operand that is not valid UTF-8 into U+FFFD characters and then report the file as missing. cp, mv, ln, mkdir, mktemp and rm keep raw bytes (0011).

## What should happen

Every tool keeps operands as OsStr/bytes end to end, opens the file named, and prints the name as GNU would (quoted per the tool's quoting rules).

## Reproduction

1. `f=$(printf 'a\377b'); echo x > "$f"; dogfish cat "$f"` reports the file missing; GNU prints x.

## Acceptance criteria

- [x] Every tool that takes file operands handles a name containing byte 0xff; differential cases cover each.

## 2026-10-29

Done: stat, df, realpath, basename, dirname, chmod, chown, touch, mkdir, env, printenv, id, md5sum/sha256sum/b2sum (including `-c` lists and backslash escaping), with 31 `@xff@` differential cases. Earlier: cp, mv, ln, mkdir, mktemp, rm (0011). Open: cat, head, tail, wc, sort, uniq, cut, tr, tac, paste, nl, shuf, ls, du (the text-tools and ls/du work), and readlink (not implemented).

## Notes

- 2026-10-30: The rest is done. `dogfish_core::InputSource` held a `String`; it now holds the operand's bytes (`OsString`), and cat, head, wc, cut, tac, paste and nl pass operands through unchanged. Also fixed: uniq (input and output), shuf (`-o`, `--random-source`, and `read error` without a name, as GNU), base64/basenc, date (`-f`, `-r`), and the `--files0-from` lists of wc and sort (`Files0From::next_name`, the lossy reader, is gone). wc -J records carry `path_bytes` (schema updated). tail, ls, du and tr needed nothing (tr takes no files). Differential `@xff@` cases for every tool touched, in gnu_compat_text.rs (fixture tolerates EILSEQ) plus tee and `sort -o` tree cases in gnu_compat_fileops.rs. util/bench.py on the text tools: no change beyond noise.
- Left: readlink is still not implemented. `printf` (format and arguments) and `seq -f` still read their text arguments lossily; those are not file names (ROADMAP.md says so). `tac` on a directory reports EISDIR where GNU reports EINVAL, for any name.
