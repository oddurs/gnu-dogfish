# Roadmap & Known Gaps

GNU/Dogfish aims to be byte-for-byte compatible with GNU coreutils 9.4 for every option it implements when output is not a terminal. Everything below is a **known** difference: either not implemented yet, or a deliberate choice. GNU's own test suite measures how far that goes: 365 of the 375 GNU tests that exercise only Dogfish programs pass (see the README), and each failing test is a cairn item in [`cairn/items`](./cairn/items). The list below covers the differences known before that suite ran. If you hit one that isn't listed, please [open an issue](https://github.com/oddurs/gnu-dogfish/issues/new/choose) with the exact command line.

## Cross-cutting

- **Locales.** Dogfish compares and sorts bytes as GNU does under `LC_ALL=C`. Collation (`strcoll`) in other locales is not implemented, so `sort`, `ls` and `uniq` order can differ from GNU in e.g. `en_US.UTF-8`. Character counting (`wc -m`, `wc -L`, `printf` character constants) follows the locale's character set as glibc decides it: bytes in the C locale, UTF-8 sequences in a UTF-8 locale; other multibyte character sets are treated as bytes.
- **Options** are parsed like glibc 2.39's `getopt_long` (permutation, `POSIXLY_CORRECT`, clustering, abbreviations, ambiguity, messages), but a GNU option Dogfish does not implement yet is refused with `TOOL: option 'X' is not supported yet` (exit 1) rather than ignored or emulated, e.g. `sort -m`, `date --resolution`.
- **File names that are not UTF-8** work in `cp`, `mv`, `ln`, `mkdir`, `mktemp`, `rm`, `stat`, `df`, `realpath`, `basename`, `dirname`, `chmod`, `chown`, `touch`, `env`, `printenv`, `id`, `tail`, `ls`, `du` and the checksum tools (`md5sum`, `sha256sum`, `b2sum`, including `-c` lists). The other tools that read files (`cat`, `head`, `wc`, `sort`, ...) still turn such a name into U+FFFD replacement characters, so they report it as missing.
- **Extended floating point.** `seq` and `printf` use 64-bit doubles where GNU uses `long double`, so a few extreme values differ (e.g. `seq -f %g 1e20 1e20`). `%a` is not supported in `printf`.
- **SELinux.** `-Z`/`--context` options are accepted and ignored.

## File & directory tools

- **`ls`**: `--color` follows GNU's built-in table and `LS_COLORS` exactly, except the `ca` (file capabilities) colour, which is never applied: the reference GNU build (Ubuntu) has no libcap and doesn't apply it either.
- **`cp` / `mv` / `ln`**
  - `cp -r` refuses to copy a directory into itself before anything is copied, whereas GNU copies part of the tree first (and lists it with `-v`). The message and exit status match.
  - Overwritten and removed files are kept in the undo trash, so other hard links to them show a link count one higher than under GNU (`DOGFISH_UNDO=off` restores GNU's behaviour). FIFOs and device files hold no data and are not kept: undoing a copy over one removes the copy but does not bring the special file back.
  - Extended attributes, ACLs and SELinux contexts are not copied (`--preserve=xattr,context` and `-a` accept them). `cp --debug` prints nothing.
  - `cp -n` behaves as upstream 9.4 (`not replacing 'f'`, exit status 1; `-b -n` is rejected). Ubuntu's 9.4 package carries 9.5's behaviour instead (a warning, then skipping with status 0).
  - GNU 9.4's `mv -b -n` prints its "mutually exclusive" error and still moves the file (a GNU bug); Dogfish errors without moving.
  - `ln -si F F` answered with `y`, and `ln -sf` onto a file that the new symlink would point back to (`ln -sf f d/f` for an existing `d/f`), leave the file alone; GNU replaces it with a symlink to itself, losing its contents. `ln -sf` reports "are the same file", `ln -si` does nothing.
- **`du`**: `-L` on a tree deeper than 128 levels keeps one directory open per level (GNU's logical walk goes by full path), so where such a walk hits `ELOOP` or `ENAMETOOLONG` can differ from GNU.
- **`df`**: which file systems count as pseudo file systems follows gnulib, plus Debian/Ubuntu's patch that hides `devtmpfs` and `squashfs` (snaps); other distributions may show those two. `--sync` / `--no-sync` are accepted and ignored.
- **`ls -l`** ACL (`+`) and SELinux (`.`) markers are detected from extended attributes.

## Text tools

- **`shuf --random-source=FILE`** is deterministic for a given FILE, but uses its bytes differently from GNU's `randint`, so the order differs from GNU's for the same FILE.
- **`tac -r`** and **`nl -bpBRE`** use a small built-in regex engine with GNU's syntax (Emacs syntax for `tac`, POSIX basic for `nl`), matching bytes in the C locale; for `tac -r`, `^` and `$` also match at the start of the file and at the end of the unread data, where GNU anchors at its read-buffer boundaries.

## System tools

- **`env -v`**: not every debug line is reproduced.
- **`id ''`**: the error message lacks GNU's errno suffix.
- **`printf '\u...'`**: always emits UTF-8, even in the C locale.
- **`date`**: setting the clock is refused; `--resolution` is not implemented.

## Deliberate extensions (not bugs)

- Rich TTY output for `ls`, `df`, `du`, `stat`, `wc --table`, `env --table`; NDJSON input shown as a table by `head`, `tail`, `sort`, `uniq`, `shuf` and `cut -f NAME` on a terminal (or with `-J`) — piped output stays GNU's.
- `-J/--json` NDJSON output and `dogfish query`.
- Compound durations such as `500ms` or `1m30s` for `sleep`, `timeout` and `watch` (GNU rejects them). Spelled-out units such as `1day` stay invalid, as in GNU.
- `rm --safe`, `dogfish undo`, `dogfish history`, `--stats`. The switches `--stats`, `--no-color` and (for tools with rich terminal output that GNU gives no `--color`) `--color[=WHEN]` are accepted only when spelled in full, before `--` or a command, and never by `echo`, `printf`, `true`, `false` or `yes`, which keep GNU's behaviour; `DOGFISH_STATS=1` and `NO_COLOR` set them for every tool. Dogfish's own long options (`--json`, `ls --tree`, `rm --safe`, ...) are never abbreviated and never listed in GNU's ambiguity messages.
- `ls -T` is GNU's tab size; the tree view is `ls --tree [-L DEPTH]`.

## Platforms

- Linux is tested against GNU on every CI run. On macOS, CI builds, lints and runs the Dogfish test suites; the GNU differential cases skip there because the system tools are BSD.
- macOS-specific code paths (`clonefile`, `getmntinfo`) are exercised by CI but not compared against GNU.
