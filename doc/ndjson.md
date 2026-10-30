# Structured output: `-J/--json`

`-J` (or `--json`) is a Dogfish extension GNU coreutils does not have: it
turns a tool's output into records, one JSON object per line, for
`dogfish query`, `jq` or any program to read. Without `-J`, every tool
prints what GNU coreutils 9.4 prints, byte for byte.

Every tool that takes `-J` follows the one contract below. It is
implemented once, in `dogfish-core` (`dogfish_core::ndjson`), and each
tool's records are described by a JSON Schema in [`doc/schema/`](./schema/),
built into the binary:

```sh
dogfish schema          # the tools that write records (and `error`)
dogfish schema ls       # the JSON Schema of ls -J records
dogfish schema error    # the error record, which any of them may write
```

## Framing

- **NDJSON**: one JSON object per line, UTF-8, each line ended by `\n`.
  Never pretty-printed, never an array, nothing else on standard output.
- Records are **written as they are produced**: `ls -R`, `du` and
  `seq` stream one record per entry; nothing waits for the end of the
  input. Records are buffered in blocks of up to 32 KiB, a record is
  written at once when 10 ms have passed since the last write (so a slow
  producer streams record by record), and on a terminal every record is
  written as it completes.
- Diagnostics stay on standard error, in GNU's words, and the exit status
  is GNU's. Each is also an error record on standard output
  ([Errors](#errors)).
- Standard output holds records and nothing else: with `-J`, the text
  `-v`, `-c` or `cp --debug` would print there is not written. Prompts
  (`cp -i`, `rm -i`, `mv -i`, `ln -i`) are on standard error, as in GNU, so
  `-J` leaves them alone; a declined prompt is no action and has no
  record.

## Types

| Kind | Encoding | Example |
|---|---|---|
| Size, count | integer (bytes or units; never a human-readable string) | `"size":4096` |
| Time | two fields: an RFC 3339 string in UTC with nanoseconds, and the integer nanoseconds since the epoch, `<name>_ns` | `"mtime":"2026-10-29T18:03:19.123456789Z","mtime_ns":1793296999123456789` |
| Mode | `mode`, the permission bits in octal, and `mode_symbolic`, as `ls -l` shows the mode | `"mode":"0644","mode_symbolic":"-rw-r--r--"` |
| Path, name | the string as received. When its bytes are not UTF-8 the string is the lossy form (with U+FFFD) and `<name>_bytes` holds the bytes in base64; otherwise there is no `_bytes` field | `"path":"bad�","path_bytes":"YmFk/w=="` |

There are no presentation fields: no human-readable sizes, relative times
("3m ago") or colours. A field that does not apply is left out, not `null`
(a file whose owner has no name has `uid` but no `user`).

Years before 0 or after 9999 cannot be written in RFC 3339; such a time is
written in ISO 8601's expanded form (`+10000-01-01T...Z`), and its `_ns`
field is exact.

## Names

Field names are snake_case and mean the same in every tool:

| Field | Meaning | Tools |
|---|---|---|
| `path` | the file's path: the operand as given, or a directory joined with an entry's name | ls, stat, du, wc, realpath, md5sum, sha256sum, b2sum, df, history; rm, mkdir, chmod, chown, touch, cp, mv; error records |
| `name` | an entry's name in its directory; a variable's name | ls; env, printenv |
| `type` | file type: `file`, `directory`, `symlink`, `fifo`, `socket`, `block_device`, `char_device`, `unknown` | ls, stat; cp, mv (of the source) |
| `size` | bytes: a file's length (`st_size`); what du counts; a file system's size | ls, stat, du, df |
| `blocks` | 512-byte blocks allocated (`st_blocks`) | ls, stat |
| `mode`, `mode_symbolic` | see Types | ls, stat, chmod (the mode after) |
| `mode_before`, `mode_before_symbolic` | the mode before a change | chmod |
| `uid`, `gid` | user and group IDs | ls, stat, id, whoami, chown (after) |
| `uid_before`, `gid_before` | the IDs before a change | chown |
| `user`, `group` | their names, when they have one | ls, stat, id, whoami |
| `nlink` | hard links | ls, stat |
| `inode` | inode number | ls, stat |
| `dev` | device number of the file system holding the file | ls, stat |
| `rdev_major`, `rdev_minor` | of a device file, its device numbers | ls, stat |
| `mtime`, `atime`, `ctime`, `btime` | modification, access, status change and birth times (each with `_ns`; `btime` only where the file system records it) | ls, stat, du (`--time`) |
| `target` | what a symlink points to, as stored | ls, stat |
| `resolved` | a canonical name | realpath |
| `value` | a variable's value; a number | env, printenv; seq |
| `time` | the instant a record is about | date, history |
| `bytes` | bytes read; bytes of data copied | wc, md5sum, sha256sum, b2sum; cp, mv |
| `files` | entries counted below a directory | du |
| `source`, `dest`, `backup` | the two ends of a copy, move or link, and the `--backup` name the destination's earlier file got | history; cp, mv, ln |
| `action` | what an action record reports (see [Action records](#action-records)) | cp, mv, rm, mkdir, ln, chmod, chown, touch |
| `undo_id` | the `dogfish undo` journal entry an action belongs to | cp, mv, rm --safe |
| `status`, `changed` | the outcome of a chmod or chown: `changed`, `retained`, `failed`, `inaccessible`, `skipped`; and whether the file changed. Of a `--check` line: `ok`, `failed`, `unreadable` | chmod, chown; md5sum, sha256sum, b2sum |
| `created` | the file did not exist before | touch |
| `error`, `errno` | a diagnostic without the `tool: ` prefix, and the system error number | error records |
| `*_duration_ns` | a duration in nanoseconds (`_ns` alone marks an epoch time) | bench |

Each tool's schema lists all of its fields with a description of each.

## The records of each tool

| Tool | One record per | Schema |
|---|---|---|
| `ls -J` | file listed (with `-R`, in every directory; headers and totals are not records) | [ls](./schema/ls.v1.json) |
| `stat -J` | operand | [stat](./schema/stat.v1.json) |
| `df -J` | file system (and the `--total` row, marked `"total":true`) | [df](./schema/df.v1.json) |
| `du -J` | line `du` would print, in the same order | [du](./schema/du.v1.json) |
| `wc -J` | input, with every count | [wc](./schema/wc.v1.json) |
| `md5sum`, `sha256sum`, `b2sum -J` | file hashed; with `--check`, line checked (`status`: `ok`, `failed`, `unreadable`) | [md5sum](./schema/md5sum.v1.json), [sha256sum](./schema/sha256sum.v1.json), [b2sum](./schema/b2sum.v1.json) |
| `realpath -J` | operand | [realpath](./schema/realpath.v1.json) |
| `env -J`, `printenv -J` | variable | [env](./schema/env.v1.json), [printenv](./schema/printenv.v1.json) |
| `id -J` | user | [id](./schema/id.v1.json) |
| `uname -J`, `nproc -J`, `whoami -J` | run (one record) | [uname](./schema/uname.v1.json), [nproc](./schema/nproc.v1.json), [whoami](./schema/whoami.v1.json) |
| `date -J` | time shown (one per line of `--file`) | [date](./schema/date.v1.json) |
| `seq -J` | number: `{"value":N}` | [seq](./schema/seq.v1.json) |
| `dogfish history -J` | journal entry | [history](./schema/history.v1.json) |
| `dogfish bench -J` | benchmark | [bench](./schema/bench.v1.json) |
| `cp -J`, `mv -J`, `rm -J`, `mkdir -J`, `ln -J` | action, where `-v` prints a line ([Action records](#action-records)) | [cp](./schema/cp.v1.json), [mv](./schema/mv.v1.json), [rm](./schema/rm.v1.json), [mkdir](./schema/mkdir.v1.json), [ln](./schema/ln.v1.json) |
| `chmod -J`, `chown -J` | file processed, where `-v` (or with `-c` alone, `-c`) prints a line | [chmod](./schema/chmod.v1.json), [chown](./schema/chown.v1.json) |
| `touch -J` | file whose times were set | [touch](./schema/touch.v1.json) |
| any of them | diagnostic ([Errors](#errors)) | [error](./schema/error.v1.json) |

Example:

```console
$ dogfish ls -J src | head -n 1
{"path":"src/main.rs","name":"main.rs","type":"file","size":14897,"blocks":32,"mode":"0644","mode_symbolic":"-rw-r--r--","nlink":1,"uid":1000,"user":"ada","gid":1000,"group":"ada","inode":3412071,"dev":2049,"mtime":"2026-10-29T18:03:19.123456789Z","mtime_ns":1793296999123456789,"atime":"2026-10-29T18:03:20.000000001Z","atime_ns":1793297000000000001,"ctime":"2026-10-29T18:03:19.123456789Z","ctime_ns":1793296999123456789}
```

## Action records

The tools that change files report what they did with `-J`: one record per
action, written as it completes, the structured form of `-v`. A tool given
`-J` decides what to report exactly as `-v` does (`chmod` and `chown` as
`-v`, or with `-c` alone as `-c`), so the records are the `-v` lines, in
the same order, and the `-v` text is not written. `touch` has no `-v`: it
writes one record per file whose times it set.

| `action` | Written when | Fields | `-v` says |
|---|---|---|---|
| `copy` | cp (and mv across file systems) has made a copy: a regular file once its data is written, a directory when it is created (before its contents) | `source`, `dest`, `type` (of the source), `bytes` (regular files), `backup`, `undo_id` | `'a' -> 'b'`, `copied 'a' -> 'b'` |
| `move` | mv renamed a file | `source`, `dest`, `type`, `backup`, `undo_id` | `renamed 'a' -> 'b'` |
| `link`, `symlink` | ln, `cp -l`/`-s` (and `cp -a` for a file hard-linked to one already copied) made a link | `source` (the file linked to; for a symlink, its target as stored), `dest` (the link), `backup`, `undo_id` | `'b' => 'a'`, `'b' -> 'a'`, `'a' -> 'b'` |
| `backup` | `--backup` renamed the destination's file, just before the action that replaces it (the next record, whose `backup` names it) | `source` (the file), `dest` (its backup name) | `(backup: 'b~')`, `'b~' ~ ...` |
| `unbackup` | a backup was renamed back after the action that was to replace its file failed | `source` (the backup), `dest` | `... (unbackup)` |
| `remove`, `rmdir` | rm removed a file or a directory; cp removed a destination to replace it; mv removed a source after copying it across file systems | `path`, `undo_id` | `removed 'f'`, `removed directory 'd'` |
| `trash` | `rm --safe` moved an operand to the trash | `path`, `undo_id` | `trashed 'f' (undo id #N ...)` |
| `mkdir` | mkdir (each parent with `-p`), `cp --parents` or mv across file systems created a directory | `path` (and `source` for cp, `undo_id` for mv) | `mkdir: created directory 'd'`, `created directory 'd'` |
| `chmod` | chmod processed a file | `path`, `status`, `changed`, `mode_before`(`_symbolic`), `mode`(`_symbolic`) | `mode of 'f' changed from ... to ...`, `... retained as ...`, `failed to change mode ...`, `... could not be accessed`, `neither symbolic link ... nor referent has been changed` |
| `chown` | chown processed a file | `path`, `status`, `changed`, `uid_before`, `gid_before`, `uid`, `gid` | `changed ownership of ...`, `ownership of ... retained`, `failed to change ...` |
| `touch` | touch set a file's times | `path`, `created` | (touch has no `-v`) |

`status` is `changed`, `retained` or `failed` (the change was refused; the
modes and ids then say what was asked for), `inaccessible` (the file could
not be reached) or, for `chmod -R`, `skipped` (a symbolic link, which
neither it nor its referent changes); `changed` is true exactly when the
status is `changed`.

`undo_id` is the id of the `dogfish undo` journal entry the action is part
of (one per command-line operand: every record of `cp -r d e` has the
same), when the journal had recorded it by then. `dogfish history -J` lists
the entries by `id`.

A copy's record is written once the copy is made, so the one exception to
"`-v`'s order" is `cp -f` on a destination it cannot open: the `remove`
record of the old destination comes before the `copy` record, where `-v`
prints the copy's line first. A copy that fails has no record, but
an error record.

```console
$ dogfish cp -rJ src dst
{"action":"copy","source":"src","dest":"dst","type":"directory","undo_id":12}
{"action":"copy","source":"src/a.txt","dest":"dst/a.txt","type":"file","bytes":24,"undo_id":12}
$ dogfish cp -rJ src missing dst | dogfish query -w 'action == copy' -f dest,bytes
$ dogfish chmod -RJ go-w . | dogfish query -w changed=true
```

## Reading records: `query`, `head`, `tail`, `sort`, `uniq`, `cut`, `shuf`

These tools take any producer's records on standard input. They pass a
record on as it came (the same line, field order and `_bytes` twins
included), so their output is in the producer's schema:

- `dogfish query -w EXPR -s FIELD -d -f FIELDS -n N` filters (`size>10K`,
  `type=file`, `name~rs`, `action == copy`, `changed=true`, `errno=2`;
  `-w` may be given more than once, and every expression must hold), sorts,
  selects fields and limits. The expression is an option argument, not an
  operand: `query -w 'action == copy'`;
- `head -n N`, `tail -n N` and `shuf` pick records;
- `sort --by FIELD [--where EXPR]` sorts them (by `name`, or `path` when the
  records have no `name`); integers, nanosecond times included, compare
  exactly;
- `uniq --by FIELD [-c]` groups adjacent records (`-c` adds `count`);
- `cut -f FIELDS [--where EXPR]` and `query -f` keep the named fields, in
  the order named, each with its `_bytes` twin.

They handle records when `-J` is given, when their output is a terminal
(which shows the records as a table, unless `-J` asks for the records
there too), and for the Dogfish options that only make sense for records
(`sort --by`/`--where`, `cut -f NAME`/`--where`). Otherwise they are GNU's
tools working on lines, byte for byte, which for `head` and `tail` gives
the same records; `query` always reads records.

## Errors

A diagnostic is written to standard error in GNU's form, byte for byte,
and the exit status is GNU's. With `-J`, each diagnostic is also an error
record on standard output, in order with the other records: after the
records of what was done before it, before those of what came after
([error](./schema/error.v1.json)).

```console
$ dogfish rm -rJ d missing
{"action":"remove","path":"d/f"}
{"action":"rmdir","path":"d"}
{"error":"cannot remove 'missing': No such file or directory","path":"missing","errno":2}
```

- `error` is the diagnostic as standard error shows it, without the
  `tool: ` prefix. One record per line of diagnostic; lines that are no
  diagnostic (`Try 'rm --help' for more information.`) have none.
- `path` is the file the diagnostic names first, as given (with its
  `path_bytes` twin when it is not UTF-8). It is left out when the
  diagnostic names no file.
- `errno` is the system error number whose message ends the diagnostic
  (2 for `No such file or directory`); left out when there is none
  (`-r not specified; omitting directory 'd'`).

Every tool given `-J` does this, from the point `-J` is read on its
command line, so a usage error after it (`missing operand`) is a record
too. `-f` (`rm`, `chmod`, `chown`) silences diagnostics, and so their
records. Any record stream may hold error records; their shape is the same
in every tool, and every tool's schema includes it. `dogfish query -w
errno=2`, `-w 'error~denied'` and so on find them; a filter on another
field leaves them out.

Error records are made in one place: the function every diagnostic goes
through (`dogfish_core::gnu_error!`, which `eprintln!` is in the tools)
also hands the message to `dogfish_core::ndjson::diagnostic`. It takes
`path` from the file names quoted for messages since the last diagnostic
(the quoting functions note them when `-J` is on) and `errno` from the
message's last `: ` part. The record waits for the next record any
record writer to standard output writes, or for its flush, so that it
lands in order.

## Versions and compatibility

What this page describes is **version 1**. `-J` and `--json` select it;
`--json=1` names it explicitly, and any other version is a usage error:

```console
$ dogfish ls --json=2
ls: unsupported JSON version '2' (supported: 1)
Try 'ls --help' for more information.
```

The compatibility policy:

- **Fields may be added** to any record in any release. Readers must ignore
  fields they do not know (the schemas allow them).
- **Removing, renaming or retyping a field**, changing its meaning, or
  changing the framing needs a **new version**, selected with `--json=2`;
  version 1 stays available alongside it, with its own schemas
  (`doc/schema/<tool>.v2.json`).
- A field that is left out when it does not apply (`user` without a user
  name, `btime` without a birth time, `_bytes` for UTF-8 names) may be
  absent from any record; the schema marks the fields every record has as
  required.

The tests (`crates/dogfish/tests/ndjson.rs`) run every producer and
validate each record against its schema, also refusing any field the
schema does not declare, so a change to the output that the schema does
not describe fails CI; `cargo xtask schemas` checks that every schema file
is a valid JSON Schema (draft 2020-12) and that the binary's copy is
current.
