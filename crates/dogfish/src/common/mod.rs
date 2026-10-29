//! Code shared by several tools.
//!
//! The file tools share GNU conventions: getopt-style option parsing
//! ([`tool_args`]), quoting of file names in messages ([`quote`]), prompts,
//! mode strings and backups. stat, df, du and ls share further GNU
//! compatibility helpers: `getopt_long`-style option parsing ([`gnu_getopt`]),
//! `human_readable` number formatting and `--block-size` parsing ([`human`]),
//! `strftime` ([`strftime`]), `quotearg` file-name quoting and `fnmatch`
//! patterns ([`fnmatch`]).

pub(crate) mod arg_order;
pub(crate) mod backup;
pub(crate) mod canonicalize;
pub(crate) mod date_format;
pub(crate) mod dirfd;
pub(crate) mod escapes;
pub(crate) mod exit_status;
pub(crate) mod file_info;
pub(crate) mod file_times;
pub(crate) mod fnmatch;
pub(crate) mod getopt;
pub(crate) mod gnu_getopt;
pub(crate) mod head_tail;
pub(crate) mod human;
pub(crate) mod interval;
pub(crate) mod lines;
pub(crate) mod mode;
pub(crate) mod output;
pub(crate) mod path;
pub(crate) mod preserve_root;
pub(crate) mod printf_format;
pub(crate) mod prompt;
pub(crate) mod quote;
pub(crate) mod sha256;
pub(crate) mod signals;
pub(crate) mod strftime;
pub(crate) mod strtod;
pub(crate) mod targets;
pub(crate) mod tool_args;
pub(crate) mod uname_info;
pub(crate) mod users;
