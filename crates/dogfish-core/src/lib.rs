//! `dogfish-core`: Shared high-performance infrastructure for Dogfish utilities.
//!
//! Features:
//! - **Global `--stats` telemetry**: Microsecond timing, throughput, and kernel strategy tracking.
//! - **Adaptive NDJSON / Table Output**: Emit structured JSON Lines when piped or `--json` is passed,
//!   and automatically render aligned, colorized tables & sparklines on interactive TTYs.
//! - **Zero-Copy & SIMD I/O**: `mmap`, 64 KiB aligned buffered writers, macOS `clonefile` / Linux CoW,
//!   and graceful `SIGPIPE` / `EPIPE` handling.
//! - **Undo Journal**: Transaction log and trash staging for reversible `rm`, `mv`, and `cp`.

pub mod context;
pub mod format;
pub mod io;
pub mod journal;
pub mod table;

pub use context::{ColorMode, DogfishContext, Telemetry};
pub use format::{
    Style, format_duration_human, format_mode_rwx, format_relative_time, format_size, gnu_io_error,
    gnu_quote, gnu_quote_file, parse_duration_str, parse_size_str, sparkline_bar,
    strip_os_error_suffix,
};
pub use io::{
    CopyMethod, DEFAULT_BUF_SIZE, FastReader, InputSource, copy_file_fast, copy_tree, move_path,
    same_file,
};
pub use journal::{JournalAction, JournalEntry, UndoJournal};
pub use table::{ColumnAlign, Table, emit_objects_or_table, eval_json_where, read_ndjson_stdin};
