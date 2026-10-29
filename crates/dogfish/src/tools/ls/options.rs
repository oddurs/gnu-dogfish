//! `ls` options and their parsing.

use dogfish_core::ColorMode;

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, argmatch, getopt_error, lossy, print_help, print_version, try_help,
};
use crate::common::human::{
    HUMAN_AUTOSCALE, HUMAN_BASE_1024, HUMAN_SI, env_block_size, parse_block_size,
};
use crate::common::quote::{Quoting, quote_bytes};
use crate::common::strftime::Timespec;
use crate::tools::ls::colors::LsColors;

/// GNU `ls` options. `-L` means `--dereference`, except with `--tree`, where it
/// keeps its Dogfish meaning of `--level` (see [`ls_option_table`]).
const LS_OPTIONS: &[OptSpec<&str>] = &[
    // Long options in GNU's table order (it decides ambiguity diagnostics).
    OptSpec::both(b'a', "all", HasArg::No, "all"),
    OptSpec::both(b'b', "escape", HasArg::No, "escape"),
    OptSpec::both(b'd', "directory", HasArg::No, "directory"),
    OptSpec::both(b'D', "dired", HasArg::No, "dired"),
    OptSpec::long("full-time", HasArg::No, "full-time"),
    OptSpec::long(
        "group-directories-first",
        HasArg::No,
        "group-directories-first",
    ),
    OptSpec::both(b'h', "human-readable", HasArg::No, "human-readable"),
    OptSpec::both(b'i', "inode", HasArg::No, "inode"),
    OptSpec::both(b'k', "kibibytes", HasArg::No, "kibibytes"),
    OptSpec::both(b'n', "numeric-uid-gid", HasArg::No, "numeric-uid-gid"),
    OptSpec::both(b'G', "no-group", HasArg::No, "no-group"),
    OptSpec::both(b'q', "hide-control-chars", HasArg::No, "hide-control-chars"),
    OptSpec::both(b'r', "reverse", HasArg::No, "reverse"),
    OptSpec::both(b's', "size", HasArg::No, "size"),
    OptSpec::both(b'w', "width", HasArg::Required, "width"),
    OptSpec::both(b'A', "almost-all", HasArg::No, "almost-all"),
    OptSpec::both(b'B', "ignore-backups", HasArg::No, "ignore-backups"),
    OptSpec::long("classify", HasArg::Optional, "classify"),
    OptSpec::long("file-type", HasArg::No, "file-type"),
    OptSpec::long("si", HasArg::No, "si"),
    OptSpec::both(
        b'H',
        "dereference-command-line",
        HasArg::No,
        "dereference-command-line",
    ),
    OptSpec::long(
        "dereference-command-line-symlink-to-dir",
        HasArg::No,
        "dereference-command-line-symlink-to-dir",
    ),
    OptSpec::long("hide", HasArg::Required, "hide"),
    OptSpec::both(b'I', "ignore", HasArg::Required, "ignore"),
    OptSpec::long("indicator-style", HasArg::Required, "indicator-style"),
    OptSpec::both(b'L', "dereference", HasArg::No, "dereference"),
    OptSpec::both(b'N', "literal", HasArg::No, "literal"),
    OptSpec::both(b'Q', "quote-name", HasArg::No, "quote-name"),
    OptSpec::long("quoting-style", HasArg::Required, "quoting-style"),
    OptSpec::both(b'R', "recursive", HasArg::No, "recursive"),
    OptSpec::long("format", HasArg::Required, "format"),
    OptSpec::long("show-control-chars", HasArg::No, "show-control-chars"),
    OptSpec::long("sort", HasArg::Required, "sort"),
    OptSpec::both(b'T', "tabsize", HasArg::Required, "tabsize"),
    OptSpec::long("time", HasArg::Required, "time"),
    OptSpec::long("time-style", HasArg::Required, "time-style"),
    OptSpec::long("zero", HasArg::No, "zero"),
    OptSpec::long("color", HasArg::Optional, "color"),
    OptSpec::long("hyperlink", HasArg::Optional, "hyperlink"),
    OptSpec::long("block-size", HasArg::Required, "block-size"),
    OptSpec::both(b'Z', "context", HasArg::No, "context"),
    OptSpec::long("author", HasArg::No, "author"),
    OptSpec::long("help", HasArg::No, "help"),
    OptSpec::long("version", HasArg::No, "version"),
    // Dogfish extensions.
    OptSpec::long("tree", HasArg::No, "tree").exact(),
    OptSpec::long("level", HasArg::Required, "level").exact(),
    OptSpec::both(b'J', "json", HasArg::No, "json").exact(),
    // Short-only options.
    OptSpec::short(b'c', HasArg::No, "ctime"),
    OptSpec::short(b'C', HasArg::No, "vertical"),
    OptSpec::short(b'f', HasArg::No, "unsorted-all"),
    OptSpec::short(b'g', HasArg::No, "no-owner"),
    OptSpec::short(b'l', HasArg::No, "long"),
    OptSpec::short(b'm', HasArg::No, "commas"),
    OptSpec::short(b'o', HasArg::No, "long-no-group"),
    OptSpec::short(b'p', HasArg::No, "slash"),
    OptSpec::short(b'S', HasArg::No, "sort-size"),
    OptSpec::short(b't', HasArg::No, "sort-time"),
    OptSpec::short(b'u', HasArg::No, "atime"),
    OptSpec::short(b'U', HasArg::No, "unsorted"),
    OptSpec::short(b'v', HasArg::No, "sort-version"),
    OptSpec::short(b'x', HasArg::No, "across"),
    OptSpec::short(b'X', HasArg::No, "sort-extension"),
    OptSpec::short(b'1', HasArg::No, "one-per-line"),
    // `-F` takes no argument; only `--classify[=WHEN]` does.
    OptSpec::short(b'F', HasArg::No, "classify"),
];

/// With `--tree`, `-L N` is the tree depth (`dogfish ls --tree -L 2`).
fn ls_option_table(args: &[OsString]) -> Vec<OptSpec<&'static str>> {
    let tree = args
        .iter()
        .map(|a| a.as_bytes())
        .take_while(|a| *a != b"--")
        .any(|a| a == b"--tree");
    LS_OPTIONS
        .iter()
        .map(|s| {
            if tree && s.key == "dereference" {
                OptSpec::short(b'L', HasArg::Required, "level")
            } else {
                OptSpec {
                    short: s.short,
                    long: s.long,
                    arg: s.arg,
                    key: s.key,
                    exact: s.exact,
                }
            }
        })
        .collect()
}

const LS_HELP: &str = "\
Usage: ls [OPTION]... [FILE]...
List information about the FILEs (the current directory by default).
Sort entries alphabetically if none of -cftuvSUX nor --sort is specified.

  -a, --all                  do not ignore entries starting with .
  -A, --almost-all           do not list implied . and ..
      --author               with -l, print the author of each file
  -b, --escape               print C-style escapes for nongraphic characters
      --block-size=SIZE      with -l, scale sizes by SIZE when printing them
  -B, --ignore-backups       do not list implied entries ending with ~
  -c                         with -lt: sort by, and show, ctime; with -l: show
                               ctime and sort by name; otherwise: sort by ctime
  -C                         list entries by columns
  -d, --directory            list directories themselves, not their contents
  -D, --dired                generate output designed for Emacs' dired mode
  -f                         same as -a -U
  -F, --classify[=WHEN]      append indicator (one of */=>@|) to entries
      --file-type            likewise, except do not append '*'
      --format=WORD          across -x, commas -m, horizontal -x, long -l,
                               single-column -1, verbose -l, vertical -C
      --full-time            like -l --time-style=full-iso
  -g                         like -l, but do not list owner
      --group-directories-first  group directories before files
  -G, --no-group             in a long listing, don't print group names
  -h, --human-readable       with -l and -s, print sizes like 1K 234M 2G etc.
      --si                   likewise, but use powers of 1000 not 1024
  -H, --dereference-command-line  follow symbolic links listed on the command line
      --hide=PATTERN         do not list implied entries matching shell PATTERN
      --hyperlink[=WHEN]     hyperlink file names WHEN
      --indicator-style=WORD  none, slash (-p), file-type, classify (-F)
  -i, --inode                print the index number of each file
  -I, --ignore=PATTERN       do not list implied entries matching shell PATTERN
  -k, --kibibytes            default to 1024-byte blocks for file system usage
  -l                         use a long listing format
  -L, --dereference          show information for the file a symbolic link
                               references (with --tree: the tree depth)
  -m                         fill width with a comma separated list of entries
  -n, --numeric-uid-gid      like -l, but list numeric user and group IDs
  -N, --literal              print entry names without quoting
  -o                         like -l, but do not list group information
  -p                         append / indicator to directories
  -q, --hide-control-chars   print ? instead of nongraphic characters
  -Q, --quote-name           enclose entry names in double quotes
      --quoting-style=WORD   literal, locale, shell, shell-always, shell-escape,
                               shell-escape-always, c, escape
  -r, --reverse              reverse order while sorting
  -R, --recursive            list subdirectories recursively
  -s, --size                 print the allocated size of each file, in blocks
  -S                         sort by file size, largest first
      --sort=WORD            none (-U), size (-S), time (-t), version (-v),
                               extension (-X), width
      --time=WORD            atime, ctime, mtime or birth time for -l and -t
      --time-style=STYLE     full-iso, long-iso, iso, locale, or +FORMAT
  -t                         sort by time, newest first
  -T, --tabsize=COLS         assume tab stops at each COLS instead of 8
  -u                         with -lt / -l / alone: like -c, but for atime
  -U                         do not sort; list entries in directory order
  -v                         natural sort of (version) numbers within text
  -w, --width=COLS           set output width to COLS.  0 means no limit
  -x                         list entries by lines instead of by columns
  -X                         sort alphabetically by entry extension
  -Z, --context              print any security context of each file
      --zero                 end each output line with NUL, not newline
  -1                         list one file per line
      --tree                 Dogfish: render the hierarchy as a tree (-L depth)
      --level=N              Dogfish: maximum --tree depth (default 3)
  -J, --json                 Dogfish: emit NDJSON records
      --help                 display this help and exit
      --version              output version information and exit

On a terminal Dogfish shows colors and a table/grid; when output is not a
terminal it is identical to GNU ls (one entry per line by default).
";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LsFormat {
    Long,
    OnePerLine,
    Columns,
    Across,
    Commas,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LsSort {
    Name,
    None,
    Time,
    Size,
    Extension,
    Version,
    Width,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LsTime {
    Mtime,
    Atime,
    Ctime,
    Birth,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Indicator {
    None,
    Slash,
    FileType,
    Classify,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LsDeref {
    Never,
    CommandLineSymlinkToDir,
    CommandLine,
    Always,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LsIgnore {
    /// Hide names starting with `.`.
    Default,
    /// `-A`: hide only `.` and `..`.
    DotAndDotDot,
    /// `-a`: hide nothing.
    Minimal,
}

pub(crate) struct LsOpts {
    pub(crate) ignore: LsIgnore,
    pub(crate) ignore_patterns: Vec<Vec<u8>>,
    pub(crate) hide_patterns: Vec<Vec<u8>>,
    pub(crate) immediate_dirs: bool,
    pub(crate) recursive: bool,
    pub(crate) format: LsFormat,
    pub(crate) sort: LsSort,
    pub(crate) reverse: bool,
    pub(crate) dirs_first: bool,
    pub(crate) time: LsTime,
    pub(crate) indicator: Indicator,
    pub(crate) deref: LsDeref,
    pub(crate) inode: bool,
    pub(crate) blocks: bool,
    pub(crate) owner: bool,
    pub(crate) group: bool,
    pub(crate) author: bool,
    pub(crate) numeric: bool,
    pub(crate) context: bool,
    pub(crate) quoting: Quoting,
    pub(crate) hide_control: bool,
    pub(crate) human_opts: u32,
    pub(crate) block_size: u64,
    pub(crate) file_human_opts: u32,
    pub(crate) file_block_size: u64,
    /// `[non-recent, recent]` strftime formats for `-l`.
    pub(crate) time_formats: [String; 2],
    pub(crate) line_length: usize,
    pub(crate) tab_size: usize,
    pub(crate) eol: u8,
    /// GNU `print_with_color`, as `--color`, `-f` and `--zero` leave it.
    pub(crate) color: bool,
    /// The last `--color=WHEN`, which also governs the rich terminal output.
    pub(crate) color_mode: Option<ColorMode>,
    /// The colour table in effect for GNU-style output (`None`: no colour).
    pub(crate) colors: Option<LsColors>,
    /// GNU `check_symlink_mode`: follow symlinks to colour or group them.
    pub(crate) check_symlink_mode: bool,
    /// `--dired` (only with `-l`): Emacs dired byte offsets after the listing.
    pub(crate) dired: bool,
    /// `--hyperlink`: names as OSC 8 `file://` links.
    pub(crate) hyperlink: bool,
    pub(crate) tree: bool,
    pub(crate) level: usize,
    pub(crate) json: bool,
    pub(crate) now: std::cell::Cell<Timespec>,
}

/// Parses the GNU `--time-style` argument into `[non-recent, recent]` formats.
fn ls_time_formats(style: &str) -> std::result::Result<[String; 2], i32> {
    let mut style = style;
    // `posix-STYLE` means STYLE only outside the POSIX locale.
    if let Some(rest) = style.strip_prefix("posix-") {
        let locale = ["LC_ALL", "LC_TIME", "LANG"]
            .iter()
            .filter_map(|v| std::env::var(v).ok())
            .find(|v| !v.is_empty())
            .unwrap_or_default();
        style = if locale.is_empty() || locale == "C" || locale == "POSIX" {
            "locale"
        } else {
            rest
        };
    }
    if let Some(fmt) = style.strip_prefix('+') {
        return match fmt.split_once('\n') {
            Some((_, recent)) if recent.contains('\n') => {
                eprintln!(
                    "ls: invalid time style format {}",
                    quote_bytes(fmt.as_bytes(), Quoting::Locale)
                );
                Err(2)
            }
            Some((old, recent)) => Ok([old.to_string(), recent.to_string()]),
            None => Ok([fmt.to_string(), fmt.to_string()]),
        };
    }
    let pair = |a: &str, b: &str| Ok([a.to_string(), b.to_string()]);
    match style {
        "full-iso" => pair("%Y-%m-%d %H:%M:%S.%N %z", "%Y-%m-%d %H:%M:%S.%N %z"),
        "long-iso" => pair("%Y-%m-%d %H:%M", "%Y-%m-%d %H:%M"),
        "iso" => pair("%Y-%m-%d ", "%m-%d %H:%M"),
        "locale" => pair("%b %e  %Y", "%b %e %H:%M"),
        _ => {
            eprintln!(
                "ls: invalid argument {} for 'time style'\nValid arguments are:\n  \
                 - [posix-]full-iso\n  - [posix-]long-iso\n  - [posix-]iso\n  \
                 - [posix-]locale\n  - +FORMAT (e.g., +%H:%M) for a 'date'-style format",
                quote_bytes(style.as_bytes(), Quoting::Locale)
            );
            Err(try_help("ls"))
        }
    }
}

/// Parses `ls` arguments; `Err` carries the exit status after a diagnostic.
/// Like GNU, the default format is columns on a terminal, one per line otherwise.
pub(crate) fn parse_ls_args(
    args: &[OsString],
    stdout_is_tty: bool,
) -> std::result::Result<(LsOpts, Vec<OsString>), i32> {
    let specs = ls_option_table(args);
    let mut parsed = Getopt::new(args.to_vec(), &specs);

    let (env_opts, env_size) = env_block_size("LS_BLOCK_SIZE");
    let env_sizes_files =
        std::env::var_os("LS_BLOCK_SIZE").is_some() || std::env::var_os("BLOCK_SIZE").is_some();
    let mut o = LsOpts {
        ignore: LsIgnore::Default,
        ignore_patterns: Vec::new(),
        hide_patterns: Vec::new(),
        immediate_dirs: false,
        recursive: false,
        format: if stdout_is_tty {
            LsFormat::Columns
        } else {
            LsFormat::OnePerLine
        },
        sort: LsSort::Name,
        reverse: false,
        dirs_first: false,
        time: LsTime::Mtime,
        indicator: Indicator::None,
        deref: LsDeref::CommandLineSymlinkToDir,
        inode: false,
        blocks: false,
        owner: true,
        group: true,
        author: false,
        numeric: false,
        context: false,
        quoting: Quoting::Literal,
        hide_control: false,
        human_opts: env_opts,
        block_size: env_size,
        file_human_opts: if env_sizes_files { env_opts } else { 0 },
        file_block_size: if env_sizes_files { env_size } else { 1 },
        time_formats: ["%b %e  %Y".into(), "%b %e %H:%M".into()],
        line_length: 80,
        tab_size: 8,
        eol: b'\n',
        color: false,
        color_mode: None,
        colors: None,
        check_symlink_mode: false,
        dired: false,
        hyperlink: false,
        tree: false,
        level: 3,
        json: false,
        now: std::cell::Cell::new(Timespec::now()),
    };
    let mut width_opt: Option<usize> = None;
    let mut tab_size_opt: Option<usize> = None;
    let mut deref: Option<LsDeref> = None;
    let mut sort_specified = false;
    let mut time_style: Option<String> = std::env::var("TIME_STYLE").ok();
    let mut kibibytes = false;
    let mut explicit_sizes = false;
    let mut time_explicit = false;
    let mut quoting: Option<Quoting> = None;
    let mut hide_control: Option<bool> = None;
    let default_format = if stdout_is_tty {
        LsFormat::Columns
    } else {
        LsFormat::OnePerLine
    };
    while let Some(item) = parsed.next() {
        let (key, raw) = item.map_err(|e| getopt_error("ls", &e))?;
        let spelled = parsed.last_spelling();
        let value = raw.as_deref().map(lossy).unwrap_or_default();
        match key {
            "all" => o.ignore = LsIgnore::Minimal,
            "almost-all" => o.ignore = LsIgnore::DotAndDotDot,
            "author" => o.author = true,
            "escape" => quoting = Some(Quoting::Escape),
            "block-size" => match parse_block_size(&value) {
                Ok((opts, size)) => {
                    (o.human_opts, o.block_size) = (opts, size);
                    (o.file_human_opts, o.file_block_size) = (opts, size);
                    explicit_sizes = true;
                }
                Err(e) => {
                    eprintln!("ls: {}", e.message(&spelled, &value));
                    return Err(2);
                }
            },
            "ignore-backups" => o.ignore_patterns.extend([b"*~".to_vec(), b".*~".to_vec()]),
            "ctime" => {
                o.time = LsTime::Ctime;
                time_explicit = true;
            }
            "atime" => {
                o.time = LsTime::Atime;
                time_explicit = true;
            }
            "vertical" => o.format = LsFormat::Columns,
            "across" => o.format = LsFormat::Across,
            "commas" => o.format = LsFormat::Commas,
            // GNU: -1 has no effect after -l.
            "one-per-line" if o.format != LsFormat::Long => o.format = LsFormat::OnePerLine,
            "one-per-line" => {}
            "long" => o.format = LsFormat::Long,
            "color" | "hyperlink" => {
                let when = [
                    ("always", "always"),
                    ("yes", "always"),
                    ("force", "always"),
                    ("never", "never"),
                    ("no", "never"),
                    ("none", "never"),
                    ("auto", "auto"),
                    ("tty", "auto"),
                    ("if-tty", "auto"),
                ];
                let when = match &raw {
                    None => "always",
                    Some(v) => argmatch("ls", &format!("--{key}"), v, &when)?,
                };
                let on = when == "always" || (when == "auto" && stdout_is_tty);
                if key == "color" {
                    o.color = on;
                    o.color_mode = ColorMode::from_when(when);
                } else {
                    o.hyperlink = on;
                }
            }
            "directory" => o.immediate_dirs = true,
            "dired" => o.dired = true,
            // GNU: like -a -U, without colours, hyperlinks, -s or -l.
            "unsorted-all" => {
                o.ignore = LsIgnore::Minimal;
                o.sort = LsSort::None;
                sort_specified = true;
                if o.format == LsFormat::Long {
                    o.format = default_format;
                }
                o.color = false;
                o.hyperlink = false;
                o.blocks = false;
            }
            "classify" => {
                let when = [
                    ("always", "always"),
                    ("yes", "always"),
                    ("force", "always"),
                    ("never", "never"),
                    ("no", "never"),
                    ("none", "never"),
                    ("auto", "auto"),
                    ("tty", "auto"),
                    ("if-tty", "auto"),
                ];
                let when = match &raw {
                    None => "always",
                    Some(v) => argmatch("ls", "--classify", v, &when)?,
                };
                // Output is not a terminal here, so `auto` means no indicators.
                if when == "always" {
                    o.indicator = Indicator::Classify;
                } else if when == "auto" {
                    o.indicator = Indicator::None;
                }
            }
            "file-type" => o.indicator = Indicator::FileType,
            "slash" => o.indicator = Indicator::Slash,
            "indicator-style" => {
                let choices = [
                    ("none", "none"),
                    ("slash", "slash"),
                    ("file-type", "file-type"),
                    ("classify", "classify"),
                ];
                o.indicator = match argmatch("ls", "--indicator-style", &value, &choices)? {
                    "none" => Indicator::None,
                    "slash" => Indicator::Slash,
                    "file-type" => Indicator::FileType,
                    _ => Indicator::Classify,
                };
            }
            "format" => {
                let choices = [
                    ("verbose", "long"),
                    ("long", "long"),
                    ("commas", "commas"),
                    ("horizontal", "across"),
                    ("across", "across"),
                    ("vertical", "vertical"),
                    ("single-column", "single-column"),
                ];
                o.format = match argmatch("ls", "--format", &value, &choices)? {
                    "long" => LsFormat::Long,
                    "commas" => LsFormat::Commas,
                    "across" => LsFormat::Across,
                    "vertical" => LsFormat::Columns,
                    _ => LsFormat::OnePerLine,
                };
            }
            "full-time" => {
                o.format = LsFormat::Long;
                time_style = Some("full-iso".into());
            }
            "no-owner" => {
                o.format = LsFormat::Long;
                o.owner = false;
            }
            "long-no-group" => {
                o.format = LsFormat::Long;
                o.group = false;
            }
            "no-group" => o.group = false,
            "numeric-uid-gid" => {
                o.format = LsFormat::Long;
                o.numeric = true;
            }
            "group-directories-first" => o.dirs_first = true,
            "human-readable" | "si" => {
                let opts = if key == "si" {
                    HUMAN_AUTOSCALE | HUMAN_SI
                } else {
                    HUMAN_AUTOSCALE | HUMAN_SI | HUMAN_BASE_1024
                };
                (o.human_opts, o.block_size) = (opts, 1);
                (o.file_human_opts, o.file_block_size) = (opts, 1);
                explicit_sizes = true;
            }
            "dereference-command-line" => deref = Some(LsDeref::CommandLine),
            "dereference-command-line-symlink-to-dir" => {
                deref = Some(LsDeref::CommandLineSymlinkToDir)
            }
            "dereference" => deref = Some(LsDeref::Always),
            "hide" => o.hide_patterns.push(raw.unwrap_or_default().into_vec()),
            "ignore" => o.ignore_patterns.push(raw.unwrap_or_default().into_vec()),
            "inode" => o.inode = true,
            "kibibytes" => kibibytes = true,
            "literal" => quoting = Some(Quoting::Literal),
            "hide-control-chars" => hide_control = Some(true),
            "show-control-chars" => hide_control = Some(false),
            "quote-name" => quoting = Some(Quoting::C),
            "quoting-style" => {
                let choices: Vec<(&str, &str)> = Quoting::NAMES.iter().map(|n| (*n, *n)).collect();
                let name = argmatch("ls", "--quoting-style", &value, &choices)?;
                quoting = Quoting::from_name(name);
            }
            "reverse" => o.reverse = true,
            "recursive" => o.recursive = true,
            "size" => o.blocks = true,
            "sort-size" => {
                o.sort = LsSort::Size;
                sort_specified = true;
            }
            "sort-time" => {
                o.sort = LsSort::Time;
                sort_specified = true;
            }
            "unsorted" => {
                o.sort = LsSort::None;
                sort_specified = true;
            }
            "sort-version" => {
                o.sort = LsSort::Version;
                sort_specified = true;
            }
            "sort-extension" => {
                o.sort = LsSort::Extension;
                sort_specified = true;
            }
            "sort" => {
                let choices = [
                    ("none", "none"),
                    ("time", "time"),
                    ("size", "size"),
                    ("extension", "extension"),
                    ("version", "version"),
                    ("width", "width"),
                ];
                o.sort = match argmatch("ls", "--sort", &value, &choices)? {
                    "none" => LsSort::None,
                    "time" => LsSort::Time,
                    "size" => LsSort::Size,
                    "extension" => LsSort::Extension,
                    "version" => LsSort::Version,
                    _ => LsSort::Width,
                };
                sort_specified = true;
            }
            "time" => {
                let choices = [
                    ("atime", "atime"),
                    ("access", "atime"),
                    ("use", "atime"),
                    ("ctime", "ctime"),
                    ("status", "ctime"),
                    ("mtime", "mtime"),
                    ("modification", "mtime"),
                    ("birth", "birth"),
                    ("creation", "birth"),
                ];
                o.time = match argmatch("ls", "--time", &value, &choices)? {
                    "atime" => LsTime::Atime,
                    "ctime" => LsTime::Ctime,
                    "mtime" => LsTime::Mtime,
                    _ => LsTime::Birth,
                };
                time_explicit = o.time != LsTime::Mtime;
            }
            "time-style" => time_style = Some(value),
            "tabsize" => tab_size_opt = Some(tab_size_arg(&value)?),
            "width" => match decode_line_length(&value) {
                Some(n) => width_opt = Some(n),
                None => {
                    eprintln!(
                        "ls: invalid line width: {}",
                        quote_bytes(value.as_bytes(), Quoting::Locale)
                    );
                    return Err(2);
                }
            },
            "context" => o.context = true,
            "zero" => {
                o.eol = b'\0';
                hide_control = Some(false);
                if o.format != LsFormat::Long {
                    o.format = LsFormat::OnePerLine;
                }
                o.color = false;
                quoting = Some(Quoting::Literal);
            }
            "tree" => o.tree = true,
            "level" => {
                o.level = value.parse().map_err(|_| {
                    eprintln!(
                        "ls: invalid tree level: {}",
                        quote_bytes(value.as_bytes(), Quoting::Locale)
                    );
                    2
                })?
            }
            "json" => o.json = true,
            "help" => return Err(print_help(LS_HELP)),
            "version" => return Err(print_version("ls")),
            _ => unreachable!("unhandled ls option"),
        }
    }
    // GNU consults COLUMNS only when the format or colouring needs a width.
    o.line_length = match width_opt {
        Some(n) => n,
        None if o.color
            || matches!(
                o.format,
                LsFormat::Columns | LsFormat::Across | LsFormat::Commas
            ) =>
        {
            match std::env::var_os("COLUMNS").filter(|c| !c.is_empty()) {
                Some(c) => decode_line_length(&c.to_string_lossy()).unwrap_or_else(|| {
                    eprintln!(
                        "ls: ignoring invalid width in environment variable COLUMNS: {}",
                        quote_bytes(c.as_encoded_bytes(), Quoting::Locale)
                    );
                    80
                }),
                None => 80,
            }
        }
        None => 80,
    };
    if matches!(
        o.format,
        LsFormat::Columns | LsFormat::Across | LsFormat::Commas
    ) {
        o.tab_size = tab_size(tab_size_opt);
    }
    if kibibytes && !explicit_sizes && std::env::var_os("LS_BLOCK_SIZE").is_none() {
        (o.human_opts, o.block_size) = (0, 1024);
    }
    if let Some(style) = time_style {
        o.time_formats = ls_time_formats(&style)?;
    }
    if time_explicit && !sort_specified && o.format != LsFormat::Long {
        o.sort = LsSort::Time;
    }
    o.deref = deref.unwrap_or(
        if o.immediate_dirs || o.indicator == Indicator::Classify || o.format == LsFormat::Long {
            LsDeref::Never
        } else {
            LsDeref::CommandLineSymlinkToDir
        },
    );
    o.quoting = quoting.unwrap_or_else(|| match std::env::var_os("QUOTING_STYLE") {
        None => Quoting::Literal,
        // GNU ARGMATCH: an exact name or an unambiguous abbreviation.
        Some(s) => Quoting::from_name(&s.to_string_lossy())
            .or_else(|| {
                let mut found = Quoting::NAMES
                    .iter()
                    .filter(|n| !s.is_empty() && n.as_bytes().starts_with(s.as_encoded_bytes()));
                match (found.next(), found.next()) {
                    (Some(name), None) => Quoting::from_name(name),
                    _ => None,
                }
            })
            .unwrap_or_else(|| {
                eprintln!(
                    "ls: ignoring invalid value of environment variable QUOTING_STYLE: {}",
                    quote_bytes(s.as_encoded_bytes(), Quoting::Locale)
                );
                Quoting::Literal
            }),
    });
    o.hide_control = hide_control.unwrap_or(false);
    // GNU: --dired means something only for -l without --hyperlink.
    o.dired &= o.format == LsFormat::Long && !o.hyperlink;
    if o.dired && o.eol == b'\0' {
        eprintln!("ls: --dired and --zero are incompatible");
        return Err(2);
    }
    Ok((o, parsed.operands()))
}

/// Why [`xstrtoumax`] rejected a number.
#[derive(Debug, PartialEq, Eq)]
enum NumError {
    Invalid,
    Overflow,
}

/// gnulib `xstrtoumax (spec, nullptr, 0, &val, "")`: optional leading
/// blanks and `+`, then a number in C syntax (decimal, `0x` hex or `0`
/// octal) and nothing after it.
fn xstrtoumax(spec: &str) -> Result<u64, NumError> {
    let s = spec.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    if s.starts_with('-') {
        return Err(NumError::Invalid);
    }
    let s = s.strip_prefix('+').unwrap_or(s);
    let (digits, radix) = if let Some(hex) = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .filter(|h| h.starts_with(|c: char| c.is_ascii_hexdigit()))
    {
        (hex, 16)
    } else if s.len() > 1 && s.starts_with('0') {
        (&s[1..], 8)
    } else {
        (s, 10)
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return Err(NumError::Invalid);
    }
    // The digits are valid, so the only possible failure is overflow.
    u64::from_str_radix(digits, radix).map_err(|_| NumError::Overflow)
}

/// The largest line length or tab size GNU accepts (`MIN (PTRDIFF_MAX, SIZE_MAX)`).
const MAX_COLUMNS: u64 = isize::MAX as u64;

/// GNU `decode_line_length` (for `-w` and COLUMNS): too large means
/// unlimited (0).
fn decode_line_length(spec: &str) -> Option<usize> {
    match xstrtoumax(spec) {
        Ok(n) if n <= MAX_COLUMNS => Some(usize::try_from(n).unwrap_or(0)),
        Ok(_) | Err(NumError::Overflow) => Some(0),
        Err(NumError::Invalid) => None,
    }
}

/// GNU's `-T` argument (`xnumtoumax (optarg, 0, 0, MAX, "", ...)`); `Err`
/// carries the exit status after the diagnostic.
fn tab_size_arg(value: &str) -> std::result::Result<usize, i32> {
    let quoted = quote_bytes(value.as_bytes(), Quoting::Locale);
    match xstrtoumax(value) {
        Ok(n) if n <= MAX_COLUMNS => Ok(usize::try_from(n).unwrap_or(usize::MAX)),
        Err(NumError::Invalid) => {
            eprintln!("ls: invalid tab size: {quoted}");
            Err(2)
        }
        // Out of range: GNU adds strerror (EOVERFLOW).
        _ => {
            eprintln!("ls: invalid tab size: {quoted}: Value too large for defined data type");
            Err(2)
        }
    }
}

/// The tab size for the grid formats: `-T`, else a valid TABSIZE, else 8.
fn tab_size(opt: Option<usize>) -> usize {
    if let Some(n) = opt {
        return n;
    }
    match std::env::var_os("TABSIZE") {
        None => 8,
        Some(v) => match xstrtoumax(&v.to_string_lossy()) {
            Ok(n) => usize::try_from(n).unwrap_or(usize::MAX),
            Err(_) => {
                eprintln!(
                    "ls: ignoring invalid tab size in environment variable TABSIZE: {}",
                    quote_bytes(v.as_encoded_bytes(), Quoting::Locale)
                );
                8
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_in_c_syntax() {
        assert_eq!(xstrtoumax("010"), Ok(8));
        assert_eq!(xstrtoumax("0x10"), Ok(16));
        assert_eq!(xstrtoumax(" +7"), Ok(7));
        assert_eq!(xstrtoumax("0"), Ok(0));
        assert_eq!(xstrtoumax("08"), Err(NumError::Invalid));
        assert_eq!(xstrtoumax("0x"), Err(NumError::Invalid));
        assert_eq!(xstrtoumax("-1"), Err(NumError::Invalid));
        assert_eq!(xstrtoumax("1k"), Err(NumError::Invalid));
        assert_eq!(xstrtoumax("99999999999999999999"), Err(NumError::Overflow));
        assert_eq!(decode_line_length("99999999999999999999"), Some(0));
        assert_eq!(decode_line_length("08"), None);
    }
}
