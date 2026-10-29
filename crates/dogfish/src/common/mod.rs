//! Code shared by several tools.
//!
//! Every tool parses its options with one port of glibc's `getopt_long`
//! ([`getopt`]). The file tools share further GNU conventions: quoting of
//! file names in messages ([`quote`]), prompts, mode strings and backups.
//! stat, df, du and ls share more GNU compatibility helpers:
//! `human_readable` number formatting and `--block-size` parsing ([`human`]),
//! `strftime` ([`strftime`]), `quotearg` file-name quoting and `fnmatch`
//! patterns ([`fnmatch`]).

pub(crate) mod backup;
pub(crate) mod canonicalize;
pub(crate) mod copy;
pub(crate) mod date_format;
pub(crate) mod escapes;
pub(crate) mod exit_status;
pub(crate) mod file_info;
pub(crate) mod file_times;
pub(crate) mod files0;
pub(crate) mod fnmatch;
pub(crate) mod getopt;
pub(crate) mod gnu_regex;
pub(crate) mod head_tail;
pub(crate) mod human;
pub(crate) mod input;
pub(crate) mod interval;
pub(crate) mod lines;
pub(crate) mod locale;
pub(crate) mod mode;
pub(crate) mod path;
pub(crate) mod posixver;
pub(crate) mod preserve_root;
pub(crate) mod printf_format;
pub(crate) mod prompt;
pub(crate) mod quote;
pub(crate) mod signals;
pub(crate) mod strftime;
pub(crate) mod strtod;
pub(crate) mod targets;
pub(crate) mod uname_info;
pub(crate) mod users;
pub(crate) mod xstrtol;
