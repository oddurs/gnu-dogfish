# Roadmap & Known Gaps

GNU/Dogfish aims to be byte-for-byte compatible with GNU coreutils 9.4 for every option it implements when output is not a terminal. Everything below is a **known** difference: either not implemented yet, or a deliberate choice. If you hit one that isn't listed, please [open an issue](https://github.com/oddurs/gnu-dogfish/issues/new/choose) with the exact command line.

## Cross-cutting

- **Locales.** Dogfish compares and sorts bytes as GNU does under `LC_ALL=C`. Collation (`strcoll`) in other locales is not implemented, so `sort`, `ls` and `uniq` order can differ from GNU in e.g. `en_US.UTF-8`. Character counting (`wc -m`, `wc -L`) is UTF-8 aware.
- **Ambiguous long-option abbreviations** (`cat --s`) report `unrecognized option` instead of GNU's `option '--s' is ambiguous; possibilities: ...`. Unambiguous abbreviations work, except that a lone `--vers` is not taken as `--version` for clap-parsed tools.
- **Extended floating point.** `seq` and `printf` use 64-bit doubles where GNU uses `long double`, so a few extreme values differ (e.g. `seq -f %g 1e20 1e20`). `%a` is not supported in `printf`.
- **SELinux.** `-Z`/`--context` options are accepted and ignored.

## File & directory tools

- **`ls`**: `--dired` (`-D`) is not implemented; `--hyperlink=WHEN` is validated but no hyperlinks are emitted. `--color` follows GNU's built-in table and `LS_COLORS` exactly, except the `ca` (file capabilities) colour, which is never applied: the reference GNU build (Ubuntu) has no libcap and doesn't apply it either.
- **`cp` / `mv`**
  - Copying or moving a directory into itself is refused before anything is copied, whereas GNU copies part of the tree first. The message and exit status match.
  - Overwritten and removed files are kept in the undo trash, so other hard links to them show a link count one higher than under GNU (`DOGFISH_UNDO=off` restores GNU's behaviour).
  - `cp -R` does not copy device nodes or sockets (FIFOs work). `--sparse`, `--reflink` and `--debug` are accepted but do nothing special.
  - Hard-link loops are only detected through the into-itself check.
  - `mv . x` / `mv d/.. x` report "subdirectory of itself" rather than GNU's "Device or resource busy". A few trailing-slash-on-symlink edge cases (`mv ld/ d`) word their error differently.
  - GNU 9.4's `mv -b -n` prints its "mutually exclusive" error and still moves the file (a GNU bug); Dogfish errors without moving.
- **`touch -d`**, **`date -d`**: the common GNU date formats are supported (`@epoch`, ISO 8601 with zones, month names, am/pm, relative items such as `yesterday` and `+1 day`). The full `parse_datetime` grammar is not: no `TZ="..."` prefix inside the string, no military zones, no unusual local zone abbreviations. `date --debug` is not implemented.
- **`du`**: `--time` and `--files0-from` are not implemented.
- **`df`**: which file systems count as pseudo file systems follows gnulib, plus Debian/Ubuntu's patch that hides `devtmpfs` and `squashfs` (snaps); other distributions may show those two. `--sync` / `--no-sync` are accepted and ignored.
- **`ls -l`** ACL (`+`) and SELinux (`.`) markers are detected from extended attributes.

## System tools

- **`env -v`**: not every debug line is reproduced.
- **`id ''`**: the error message lacks GNU's errno suffix.
- **`printf '\u...'`**: always emits UTF-8, even in the C locale.
- **`date`**: setting the clock is refused.

## Deliberate extensions (not bugs)

- Rich TTY output for `ls`, `df`, `du`, `stat`, `wc --table`, `env --table`.
- `-J/--json` NDJSON output and `dogfish query`.
- Human durations such as `500ms` or `1m30s` for `sleep`, `timeout` and `watch` (GNU rejects them).
- `rm --safe`, `dogfish undo`, `dogfish history`, `--stats`.
- `ls -T` is GNU's tab size; the tree view is `ls --tree [-L DEPTH]`.

## Platforms

- Linux is tested against GNU on every CI run. On macOS, CI builds, lints and runs the Dogfish test suites; the GNU differential cases skip there because the system tools are BSD.
- macOS-specific code paths (`clonefile`, `getmntinfo`) are exercised by CI but not compared against GNU.
