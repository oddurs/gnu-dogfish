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
dogfish schema          # the tools that write records
dogfish schema ls       # the JSON Schema of ls -J records
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
  is GNU's.

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
| `path` | the file's path: the operand as given, or a directory joined with an entry's name | ls, stat, du, wc, realpath, md5sum, sha256sum, b2sum, df, history |
| `name` | an entry's name in its directory; a variable's name | ls; env, printenv |
| `type` | file type: `file`, `directory`, `symlink`, `fifo`, `socket`, `block_device`, `char_device`, `unknown` | ls, stat |
| `size` | bytes: a file's length (`st_size`); what du counts; a file system's size | ls, stat, du, df |
| `blocks` | 512-byte blocks allocated (`st_blocks`) | ls, stat |
| `mode`, `mode_symbolic` | see Types | ls, stat |
| `uid`, `gid` | user and group IDs | ls, stat, id, whoami |
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
| `bytes` | bytes read | wc, md5sum, sha256sum, b2sum |
| `files` | entries counted below a directory | du |
| `source`, `dest`, `backup` | the two ends of a copy or move, and what the destination held | history |
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

Example:

```console
$ dogfish ls -J src | head -n 1
{"path":"src/main.rs","name":"main.rs","type":"file","size":14897,"blocks":32,"mode":"0644","mode_symbolic":"-rw-r--r--","nlink":1,"uid":1000,"user":"ada","gid":1000,"group":"ada","inode":3412071,"dev":2049,"mtime":"2026-10-29T18:03:19.123456789Z","mtime_ns":1793296999123456789,"atime":"2026-10-29T18:03:20.000000001Z","atime_ns":1793297000000000001,"ctime":"2026-10-29T18:03:19.123456789Z","ctime_ns":1793296999123456789}
```

## Reading records: `query`, `head`, `tail`, `sort`, `uniq`, `cut`, `shuf`

These tools take any producer's records on standard input. They pass a
record on as it came (the same line, field order and `_bytes` twins
included), so their output is in the producer's schema:

- `dogfish query -w EXPR -s FIELD -d -f FIELDS -n N` filters (`size>10K`,
  `type=file`, `name~rs`), sorts, selects fields and limits;
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

A diagnostic is written to standard error in GNU's form, and the exit
status is GNU's. The contract reserves one more record shape, which tools
that change files will write to standard output as well, in order with
their other records:

```json
{"error":"No such file or directory","path":"missing","errno":2}
```

`error` is the diagnostic without the `tool: ` prefix, `errno` the system
error number when there is one.

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
