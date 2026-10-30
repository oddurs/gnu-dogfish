//! `dogfish-core`: Shared high-performance infrastructure for Dogfish utilities.
//!
//! Features:
//! - **Global `--stats` telemetry**: Microsecond timing, throughput, and kernel strategy tracking.
//! - **Structured output**: the `-J/--json` record contract ([`ndjson`]: NDJSON,
//!   versioned, one schema per tool), and aligned, colorized tables on interactive TTYs.
//! - **Zero-Copy & SIMD I/O**: `mmap`, 64 KiB aligned buffered writers, macOS `clonefile` / Linux CoW,
//!   and one standard output with GNU `close_stdout` write-error semantics.
//! - **Undo Journal**: Transaction log and trash staging for reversible `rm`, `mv`, and `cp`.

pub mod context;
pub mod crash;
pub mod format;
pub mod io;
pub mod journal;
pub mod ndjson;
pub mod stdout;
pub mod table;

pub use context::{ColorMode, DogfishContext, Switches, Telemetry};
pub use crash::crash_point;
pub use format::{
    Style, format_duration_human, format_mode_rwx, format_relative_time, format_size, gnu_io_error,
    gnu_quote, gnu_quote_file, parse_duration_str, parse_size_str, sparkline_bar,
    strip_os_error_suffix,
};
pub use io::{
    CopyMethod, DEFAULT_BUF_SIZE, FastReader, InputSource, copy_file_fast, copy_tree, move_path,
    same_file,
};
pub use journal::{JournalAction, JournalEntry, Take, UndoJournal};
pub use ndjson::{ErrorRecord, Record, RecordWriter};
pub use table::{
    ColumnAlign, InputRecord, Table, default_key, emit_records, eval_json_where, parse_ndjson,
};
