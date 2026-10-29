//! The utilities the multicall binary provides, for dispatch, `--help`
//! and the manual pages.

pub const ALL_TOOLS: &[(&str, &str, &str)] = &[
    // Tier 1: File & Directory
    (
        "ls",
        "File & Dir",
        "List directory contents with colors, --tree, and --json",
    ),
    (
        "cp",
        "File & Dir",
        "Copy files/dirs using zero-copy CoW (APFS clonefile)",
    ),
    (
        "mv",
        "File & Dir",
        "Move/rename files (recorded to UndoJournal)",
    ),
    (
        "rm",
        "File & Dir",
        "Remove files/dirs (supports --safe undoable trash)",
    ),
    ("mkdir", "File & Dir", "Create directories (-p, -m, -v)"),
    (
        "ln",
        "File & Dir",
        "Create hard or symbolic links (-s, -f, -v)",
    ),
    (
        "chmod",
        "File & Dir",
        "Change file mode bits (symbolic +x or octal 755, -R)",
    ),
    ("chown", "File & Dir", "Change file owner and group (-R)"),
    (
        "stat",
        "File & Dir",
        "Inspect file metadata with visual card or --json",
    ),
    (
        "touch",
        "File & Dir",
        "Update file access/modification timestamps",
    ),
    (
        "df",
        "File & Dir",
        "Report filesystem disk usage with sparkline bars",
    ),
    (
        "du",
        "File & Dir",
        "Parallel disk usage estimator with sparklines",
    ),
    (
        "realpath",
        "File & Dir",
        "Print resolved canonical physical path (-m, --json)",
    ),
    (
        "basename",
        "File & Dir",
        "Strip directory and suffix from filenames (-a, -s)",
    ),
    (
        "dirname",
        "File & Dir",
        "Strip last component from file name",
    ),
    (
        "mktemp",
        "File & Dir",
        "Create a temporary file or directory safely (-d, -p)",
    ),
    // Tier 2: Text & Streaming
    (
        "cat",
        "Text & Stream",
        "Zero-copy mmap file concatenator (-n, -b, -s, -E, -T)",
    ),
    (
        "tac",
        "Text & Stream",
        "Concatenate and print files in reverse (SIMD memrchr)",
    ),
    (
        "head",
        "Text & Stream",
        "First N lines/bytes or NDJSON objects (auto-table)",
    ),
    (
        "tail",
        "Text & Stream",
        "Last N lines/bytes (-f follow) or NDJSON objects",
    ),
    (
        "wc",
        "Text & Stream",
        "SIMD line/word/char/byte counter (--table, --json)",
    ),
    (
        "sort",
        "Text & Stream",
        "Parallel line & NDJSON sorter (--by <field>, --where)",
    ),
    (
        "uniq",
        "Text & Stream",
        "Deduplicate lines or NDJSON objects (--by <field>)",
    ),
    (
        "cut",
        "Text & Stream",
        "Slice bytes/chars/fields or NDJSON (-f, --where)",
    ),
    (
        "tr",
        "Text & Stream",
        "Translate, squeeze, or delete characters",
    ),
    (
        "paste",
        "Text & Stream",
        "Merge lines of files side-by-side or serially",
    ),
    ("nl", "Text & Stream", "Number lines of files"),
    (
        "shuf",
        "Text & Stream",
        "Fisher-Yates random shuffler for lines, ranges (-i), or NDJSON",
    ),
    (
        "query",
        "Text & Stream",
        "SQL-like NDJSON filter, sort & projection (-w, -s, -f, -n)",
    ),
    // Tier 3: Hashing & Encoding
    (
        "b2sum",
        "Hash & Encode",
        "Parallel BLAKE2b message digest & --check verifier",
    ),
    (
        "sha256sum",
        "Hash & Encode",
        "Parallel SHA-256 message digest & --check verifier",
    ),
    (
        "md5sum",
        "Hash & Encode",
        "Parallel MD5 message digest & --check verifier",
    ),
    ("base64", "Hash & Encode", "Base64 encoder and decoder"),
    (
        "basenc",
        "Hash & Encode",
        "Multi-radix encoder/decoder (base64url, base32, hex, bin)",
    ),
    // Tier 4: Shell & System
    ("echo", "Shell & Sys", "Display a line of text (-n, -e)"),
    ("printf", "Shell & Sys", "Format and print data"),
    (
        "env",
        "Shell & Sys",
        "Inspect (--table, --json) or run in modified environment",
    ),
    (
        "printenv",
        "Shell & Sys",
        "Print values of specified environment variables (--json)",
    ),
    (
        "id",
        "Shell & Sys",
        "Print user and group identity (--json)",
    ),
    (
        "uname",
        "Shell & Sys",
        "Print system kernel/machine information (--json)",
    ),
    ("hostname", "Shell & Sys", "Print system network hostname"),
    (
        "nproc",
        "Shell & Sys",
        "Print number of available processing units (--json)",
    ),
    ("whoami", "Shell & Sys", "Print effective username"),
    (
        "yes",
        "Shell & Sys",
        "64 KiB page-aligned high-throughput repeater",
    ),
    ("true", "Shell & Sys", "Exit with status 0"),
    ("false", "Shell & Sys", "Exit with status 1"),
    (
        "sleep",
        "Shell & Sys",
        "Suspend execution for human duration (500ms, 1m30s)",
    ),
    (
        "timeout",
        "Shell & Sys",
        "Run a command with a human time limit (e.g. timeout 2s cmd)",
    ),
    (
        "watch",
        "Shell & Sys",
        "Execute a program periodically with timing header (-n)",
    ),
    (
        "seq",
        "Shell & Sys",
        "Print fast buffered numeric sequences",
    ),
    (
        "date",
        "Shell & Sys",
        "Format or display dates (-u, -I, -R, -d, --json)",
    ),
    (
        "tee",
        "Shell & Sys",
        "Read from stdin and fan out to stdout + files",
    ),
    // Meta-Tools
    (
        "bench",
        "Meta-Tool",
        "Run head-to-head benchmarks against system utilities",
    ),
    (
        "undo",
        "Meta-Tool",
        "Undo the most recent rm --safe, mv, or cp operation",
    ),
    (
        "history",
        "Meta-Tool",
        "View the reversible file operation journal",
    ),
    (
        "init",
        "Meta-Tool",
        "Generate shell aliases (`eval \"$(dogfish init zsh --safe-rm)\"`)",
    ),
    (
        "install-links",
        "Meta-Tool",
        "Install BusyBox-style symlinks for all 40 utilities",
    ),
];
