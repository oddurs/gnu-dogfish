use std::collections::{HashMap, HashSet, VecDeque};
#[cfg(target_os = "macos")]
use std::ffi::CStr;
use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{
    DirBuilderExt, DirEntryExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink,
};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone, Timelike};
use dogfish_core::{
    ColumnAlign, CopyMethod, DogfishContext, JournalAction, Style, Table, UndoJournal,
    emit_objects_or_table, format_mode_rwx, format_relative_time, format_size, gnu_io_error,
    io::ignore_broken_pipe, move_path, same_file, sparkline_bar,
};
use rayon::prelude::*;
use serde::Serialize;

use crate::sys::{get_groupname_by_gid, get_username_by_uid};

// ---------------------------------------------------------------------------
// 0. GNU conventions shared by the file tools: getopt-style option parsing,
//    quoting of file names in messages, prompts, mode strings and backups.
// ---------------------------------------------------------------------------

// Permission bits as `u32` (`libc::mode_t` is `u16` on macOS).
const S_ISUID: u32 = 0o4000;
const S_ISGID: u32 = 0o2000;
const S_ISVTX: u32 = 0o1000;
const S_IRWXU: u32 = 0o700;
const S_IRWXG: u32 = 0o070;
const S_IRWXO: u32 = 0o007;
const S_IRWXUGO: u32 = 0o777;
const CHMOD_MODE_BITS: u32 = 0o7777;

/// Whether an option takes an argument, as in `getopt_long`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HasArg {
    No,
    Required,
    Optional,
}

/// One option: any of the letters in `shorts` and/or the `long` name.
struct ToolOpt<T> {
    shorts: &'static str,
    long: &'static str,
    arg: HasArg,
    id: T,
}

const fn opt<T>(shorts: &'static str, long: &'static str, arg: HasArg, id: T) -> ToolOpt<T> {
    ToolOpt {
        shorts,
        long,
        arg,
        id,
    }
}

/// Parsed command line: options in the order given (so later flags can override
/// earlier ones, as in GNU) and the operands.
struct ToolArgs<T> {
    opts: Vec<(T, Option<String>)>,
    operands: Vec<String>,
}

/// Prints a usage error the way every GNU utility does; returns exit status 1.
fn usage_error(tool: &str, msg: &str) -> i32 {
    eprintln!("{tool}: {msg}\nTry '{tool} --help' for more information.");
    1
}

enum LongMatch<'a, T> {
    Spec(&'a ToolOpt<T>),
    Help,
    Version,
}

/// Resolves a (possibly abbreviated) long option name. `Err` lists the
/// candidates: none means unrecognized, several means ambiguous.
fn find_long<'a, T: Copy + PartialEq>(
    name: &str,
    specs: &'a [ToolOpt<T>],
) -> std::result::Result<(&'static str, LongMatch<'a, T>), Vec<&'static str>> {
    let mut candidates: Vec<(&'static str, LongMatch<'a, T>)> = specs
        .iter()
        .filter(|s| !s.long.is_empty())
        .map(|s| (s.long, LongMatch::Spec(s)))
        .collect();
    candidates.push(("help", LongMatch::Help));
    candidates.push(("version", LongMatch::Version));
    if let Some(i) = candidates.iter().position(|(long, _)| *long == name) {
        return Ok(candidates.swap_remove(i));
    }
    let mut matches: Vec<_> = candidates
        .into_iter()
        .filter(|(long, _)| long.starts_with(name))
        .collect();
    let same_option = |a: &LongMatch<T>, b: &LongMatch<T>| match (a, b) {
        (LongMatch::Spec(x), LongMatch::Spec(y)) => x.id == y.id && x.arg == y.arg,
        _ => false,
    };
    if matches.len() == 1
        || (!matches.is_empty() && matches.iter().all(|m| same_option(&m.1, &matches[0].1)))
    {
        return Ok(matches.swap_remove(0));
    }
    Err(matches.into_iter().map(|(long, _)| long).collect())
}

/// Parses `args` (including `argv[0]`) like glibc's `getopt_long`: options and
/// operands may be interleaved, short options bundle, long options may be
/// abbreviated, and `--` ends option processing. `--help` and `--version` are
/// handled here. `Err` carries the exit status after help, version or a usage
/// error has been printed.
fn parse_tool_args<T: Copy + PartialEq>(
    tool: &str,
    args: &[String],
    specs: &[ToolOpt<T>],
    help: &str,
) -> std::result::Result<ToolArgs<T>, i32> {
    getopt_impl(tool, args, specs, help, false)
}

/// Like [`parse_tool_args`], but options end at the first operand (getopt's `+`
/// mode, used by `basename`).
fn parse_tool_args_posix<T: Copy + PartialEq>(
    tool: &str,
    args: &[String],
    specs: &[ToolOpt<T>],
    help: &str,
) -> std::result::Result<ToolArgs<T>, i32> {
    getopt_impl(tool, args, specs, help, true)
}

fn getopt_impl<T: Copy + PartialEq>(
    tool: &str,
    args: &[String],
    specs: &[ToolOpt<T>],
    help: &str,
    stop_at_operand: bool,
) -> std::result::Result<ToolArgs<T>, i32> {
    let mut opts = Vec::new();
    let mut operands = Vec::new();
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        if arg == "--" {
            operands.extend(rest.cloned());
            break;
        }
        if let Some(body) = arg.strip_prefix("--") {
            let (name, value) = match body.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (body, None),
            };
            let (full, found) = match find_long(name, specs) {
                Ok(m) => m,
                Err(c) if c.is_empty() => {
                    return Err(usage_error(tool, &format!("unrecognized option '{arg}'")));
                }
                Err(c) => {
                    let list: Vec<String> = c.iter().map(|l| format!("'--{l}'")).collect();
                    return Err(usage_error(
                        tool,
                        &format!(
                            "option '--{name}' is ambiguous; possibilities: {}",
                            list.join(" ")
                        ),
                    ));
                }
            };
            let has_arg = match &found {
                LongMatch::Spec(s) => s.arg,
                _ => HasArg::No,
            };
            let value = match (has_arg, value) {
                (HasArg::No, Some(_)) => {
                    return Err(usage_error(
                        tool,
                        &format!("option '--{full}' doesn't allow an argument"),
                    ));
                }
                (HasArg::Required, None) => match rest.next() {
                    Some(v) => Some(v.clone()),
                    None => {
                        return Err(usage_error(
                            tool,
                            &format!("option '--{full}' requires an argument"),
                        ));
                    }
                },
                (_, v) => v,
            };
            match found {
                LongMatch::Spec(s) => opts.push((s.id, value)),
                LongMatch::Help => {
                    print!("{help}");
                    return Err(0);
                }
                LongMatch::Version => {
                    println!("{tool} (GNU/Dogfish) {}", env!("CARGO_PKG_VERSION"));
                    return Err(0);
                }
            }
        } else if arg.len() > 1 && arg.starts_with('-') {
            let body = &arg[1..];
            for (idx, c) in body.char_indices() {
                let Some(spec) = specs.iter().find(|s| s.shorts.contains(c)) else {
                    return Err(usage_error(tool, &format!("invalid option -- '{c}'")));
                };
                let tail = &body[idx + c.len_utf8()..];
                match spec.arg {
                    HasArg::No => opts.push((spec.id, None)),
                    HasArg::Optional => {
                        opts.push((spec.id, (!tail.is_empty()).then(|| tail.to_string())));
                        break;
                    }
                    HasArg::Required => {
                        let value = if !tail.is_empty() {
                            tail.to_string()
                        } else if let Some(v) = rest.next() {
                            v.clone()
                        } else {
                            return Err(usage_error(
                                tool,
                                &format!("option requires an argument -- '{c}'"),
                            ));
                        };
                        opts.push((spec.id, Some(value)));
                        break;
                    }
                }
            }
        } else {
            operands.push(arg.clone());
            if stop_at_operand {
                operands.extend(rest.cloned());
                break;
            }
        }
    }
    Ok(ToolArgs { opts, operands })
}

/// Matches `arg` (or an unambiguous prefix of it) against `table`, like GNU
/// `argmatch`; prints GNU's diagnostic and returns `Err(1)` otherwise.
fn argmatch<V: Copy + PartialEq>(
    tool: &str,
    context: &str,
    arg: &str,
    table: &[(&str, V)],
) -> std::result::Result<V, i32> {
    if let Some((_, v)) = table.iter().find(|(name, _)| *name == arg) {
        return Ok(*v);
    }
    let matches: Vec<V> = table
        .iter()
        .filter(|(name, _)| name.starts_with(arg) && !arg.is_empty())
        .map(|(_, v)| *v)
        .collect();
    if let Some(first) = matches.first()
        && matches.iter().all(|v| v == first)
    {
        return Ok(*first);
    }
    let problem = if matches.is_empty() {
        "invalid"
    } else {
        "ambiguous"
    };
    let mut msg = format!(
        "{tool}: {problem} argument {} for {}\nValid arguments are:",
        quote_locale(arg),
        quote_locale(context)
    );
    let mut prev: Option<V> = None;
    for (name, v) in table {
        if prev == Some(*v) {
            msg.push_str(&format!(", '{name}'"));
        } else {
            msg.push_str(&format!("\n  - '{name}'"));
        }
        prev = Some(*v);
    }
    eprintln!("{msg}\nTry '{tool} --help' for more information.");
    Err(1)
}

/// `quoteaf`: how GNU names files in most messages (`'a b'`, `"it's"`, `'nl'$'\n''x'`).
fn qa(s: impl AsRef<OsStr>) -> String {
    quote_bytes(s.as_ref().as_bytes(), Quoting::ShellEscapeAlways)
}

/// `quotef`: quotes only names that need it, and `:` (used before `: error` text).
fn qf(s: impl AsRef<OsStr>) -> String {
    String::from_utf8_lossy(&quote_bytes_raw(
        s.as_ref().as_bytes(),
        Quoting::ShellEscape,
        b":",
    ))
    .into_owned()
}

/// `quote`: GNU's locale quoting, `'tab\tname'` (curly quotes in UTF-8 locales).
fn quote_locale(s: impl AsRef<OsStr>) -> String {
    quote_bytes(s.as_ref().as_bytes(), Quoting::Locale)
}

/// Prints `tool: question` on stderr and reads one line from stdin; true when it
/// starts with `y` or `Y` (GNU's `yesno` in the C locale). EOF answers no.
fn ask(tool: &str, question: &str) -> bool {
    eprint!("{tool}: {question}");
    let _ = io::stderr().flush();
    let mut line = Vec::new();
    match io::stdin().lock().read_until(b'\n', &mut line) {
        Ok(n) if n > 0 => matches!(line[0], b'y' | b'Y'),
        _ => false,
    }
}

/// The process umask (reading it requires setting it, so it is restored at once).
fn process_umask() -> u32 {
    let old = unsafe { libc::umask(0) };
    unsafe { libc::umask(old) };
    mode_t_to_u32(old)
}

// `libc::mode_t` is `u32` on Linux and `u16` on macOS: the casts below are
// needed on one platform and redundant on the other.
#[allow(clippy::unnecessary_cast)]
fn mode_t_to_u32(mode: libc::mode_t) -> u32 {
    mode as u32
}

#[allow(clippy::unnecessary_cast)]
fn u32_to_mode_t(mode: u32) -> libc::mode_t {
    (mode & CHMOD_MODE_BITS) as libc::mode_t
}

/// The nine `rwx` characters of `mode`, with `s`/`S`/`t`/`T` for the special bits
/// (`strmode` without the type letter).
fn strmode9(mode: u32) -> String {
    let mut s = String::with_capacity(9);
    for (shift, special, letter) in [(6, S_ISUID, 's'), (3, S_ISGID, 's'), (0, S_ISVTX, 't')] {
        let bits = (mode >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(match (bits & 1 != 0, mode & special != 0) {
            (true, true) => letter,
            (false, true) => letter.to_ascii_uppercase(),
            (true, false) => 'x',
            (false, false) => '-',
        });
    }
    s
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModeFlag {
    Ordinary,
    CopyExisting,
    XIfAnyX,
}

/// One clause of a compiled `chmod` mode (a port of gnulib's `modechange`).
#[derive(Clone, Copy)]
struct ModeChange {
    op: u8,
    flag: ModeFlag,
    affected: u32,
    value: u32,
    mentioned: u32,
}

/// Compiles an octal or symbolic mode (`755`, `u+x,g-w`, `=rwX`, `go=u`).
fn mode_compile(spec: &str) -> Option<Vec<ModeChange>> {
    let s = spec.as_bytes();
    let octal_digit = |b: Option<&u8>| b.copied().filter(|b| (b'0'..=b'7').contains(b));
    if octal_digit(s.first()).is_some() {
        let mut octal = 0u32;
        for &b in s {
            octal_digit(Some(&b))?;
            octal = octal * 8 + u32::from(b - b'0');
            if octal > CHMOD_MODE_BITS {
                return None;
            }
        }
        // Fewer than five digits leave a directory's set-id bits alone.
        let mentioned = if s.len() < 5 {
            (octal & (S_ISUID | S_ISGID)) | S_ISVTX | S_IRWXUGO
        } else {
            CHMOD_MODE_BITS
        };
        return Some(vec![ModeChange {
            op: b'=',
            flag: ModeFlag::Ordinary,
            affected: CHMOD_MODE_BITS,
            value: octal,
            mentioned,
        }]);
    }

    let mut changes = Vec::new();
    let mut i = 0;
    loop {
        let mut affected = 0;
        loop {
            match s.get(i) {
                Some(b'u') => affected |= S_ISUID | S_IRWXU,
                Some(b'g') => affected |= S_ISGID | S_IRWXG,
                Some(b'o') => affected |= S_ISVTX | S_IRWXO,
                Some(b'a') => affected |= CHMOD_MODE_BITS,
                Some(b'=' | b'+' | b'-') => break,
                _ => return None,
            }
            i += 1;
        }
        loop {
            let op = s[i];
            i += 1;
            let mut mentioned = 0;
            let mut flag = ModeFlag::CopyExisting;
            let value;
            match s.get(i) {
                Some(b'0'..=b'7') => {
                    let mut octal = 0u32;
                    while let Some(b) = octal_digit(s.get(i)) {
                        octal = octal * 8 + u32::from(b - b'0');
                        if octal > CHMOD_MODE_BITS {
                            return None;
                        }
                        i += 1;
                    }
                    if affected != 0 || s.get(i).is_some_and(|&b| b != b',') {
                        return None;
                    }
                    affected = CHMOD_MODE_BITS;
                    mentioned = CHMOD_MODE_BITS;
                    value = octal;
                    flag = ModeFlag::Ordinary;
                }
                Some(b'u') => {
                    value = S_IRWXU;
                    i += 1;
                }
                Some(b'g') => {
                    value = S_IRWXG;
                    i += 1;
                }
                Some(b'o') => {
                    value = S_IRWXO;
                    i += 1;
                }
                _ => {
                    let mut v = 0;
                    flag = ModeFlag::Ordinary;
                    loop {
                        match s.get(i) {
                            Some(b'r') => v |= 0o444,
                            Some(b'w') => v |= 0o222,
                            Some(b'x') => v |= 0o111,
                            Some(b'X') => flag = ModeFlag::XIfAnyX,
                            Some(b's') => v |= S_ISUID | S_ISGID,
                            Some(b't') => v |= S_ISVTX,
                            _ => break,
                        }
                        i += 1;
                    }
                    value = v;
                }
            }
            changes.push(ModeChange {
                op,
                flag,
                affected,
                value,
                mentioned: if mentioned != 0 {
                    mentioned
                } else if affected != 0 {
                    affected & value
                } else {
                    value
                },
            });
            if !matches!(s.get(i), Some(b'=' | b'+' | b'-')) {
                break;
            }
        }
        match s.get(i) {
            Some(b',') => i += 1,
            None => return Some(changes),
            _ => return None,
        }
    }
}

/// Applies compiled `changes` to `old`; returns the new mode and the bits the
/// changes cared about. Clauses without a `who` part honour `umask`.
fn mode_adjust(old: u32, dir: bool, umask: u32, changes: &[ModeChange]) -> (u32, u32) {
    let mut mode = old & CHMOD_MODE_BITS;
    let mut mode_bits = 0;
    for c in changes {
        let omit = (if dir { S_ISUID | S_ISGID } else { 0 }) & !c.mentioned;
        let mut value = c.value;
        match c.flag {
            ModeFlag::Ordinary => {}
            ModeFlag::CopyExisting => {
                value &= mode;
                let spread = |mask: u32, all: u32| if value & mask != 0 { all } else { 0 };
                value |= spread(0o444, 0o444) | spread(0o222, 0o222) | spread(0o111, 0o111);
            }
            ModeFlag::XIfAnyX => {
                if mode & 0o111 != 0 || dir {
                    value |= 0o111;
                }
            }
        }
        value &= (if c.affected != 0 { c.affected } else { !umask }) & !omit;
        match c.op {
            b'=' => {
                let preserved = (if c.affected != 0 { !c.affected } else { 0 }) | omit;
                mode_bits |= CHMOD_MODE_BITS & !preserved;
                mode = (mode & preserved) | value;
            }
            b'+' => {
                mode_bits |= value;
                mode |= value;
            }
            _ => {
                mode_bits |= value;
                mode &= !value;
            }
        }
    }
    (mode, mode_bits)
}

/// `--backup` version control, as in GNU `backupfile`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BackupMode {
    None,
    Simple,
    Numbered,
    Existing,
}

const BACKUP_ARGS: &[(&str, BackupMode)] = &[
    ("none", BackupMode::None),
    ("off", BackupMode::None),
    ("simple", BackupMode::Simple),
    ("never", BackupMode::Simple),
    ("existing", BackupMode::Existing),
    ("nil", BackupMode::Existing),
    ("numbered", BackupMode::Numbered),
    ("t", BackupMode::Numbered),
];

/// The mode for `-b` / `--backup[=CONTROL]`: CONTROL, else `$VERSION_CONTROL`,
/// else `existing`.
fn backup_mode(tool: &str, arg: Option<&str>) -> std::result::Result<BackupMode, i32> {
    match arg {
        Some(a) => argmatch(tool, "backup type", a, BACKUP_ARGS),
        None => match std::env::var("VERSION_CONTROL") {
            Ok(v) if !v.is_empty() => argmatch(tool, "$VERSION_CONTROL", &v, BACKUP_ARGS),
            _ => Ok(BackupMode::Existing),
        },
    }
}

/// `-S SUFFIX`, else `$SIMPLE_BACKUP_SUFFIX`, else `~` (a suffix with a `/` is ignored).
fn backup_suffix(arg: Option<String>) -> String {
    arg.or_else(|| std::env::var("SIMPLE_BACKUP_SUFFIX").ok())
        .filter(|s| !s.is_empty() && !s.contains('/'))
        .unwrap_or_else(|| "~".to_string())
}

/// The name an existing `dst` is renamed to before being replaced.
fn backup_name(dst: &Path, mode: BackupMode, suffix: &str) -> PathBuf {
    let with_suffix = |s: &str| {
        let mut name = dst.as_os_str().to_os_string();
        name.push(s);
        PathBuf::from(name)
    };
    if mode == BackupMode::Simple {
        return with_suffix(suffix);
    }
    let base = dst.file_name().map(OsStr::as_bytes).unwrap_or_default();
    let dir = match dst.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let highest = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let rest = name.as_bytes().strip_prefix(base)?.strip_prefix(b".~")?;
            let digits = rest.strip_suffix(b"~")?;
            (digits.first().is_some_and(|d| (b'1'..=b'9').contains(d))
                && digits.iter().all(u8::is_ascii_digit))
            .then(|| std::str::from_utf8(digits).ok()?.parse::<u64>().ok())
            .flatten()
        })
        .max();
    match (mode, highest) {
        (BackupMode::Existing, None) => with_suffix(suffix),
        (_, n) => with_suffix(&format!(".~{}~", n.unwrap_or(0) + 1)),
    }
}

/// Renames `dst` to its backup name before it is replaced. With a journal, a
/// backup file that already exists is itself saved first, and the rename is
/// recorded so `dogfish undo` can bring everything back.
fn make_backup(
    dst: &Path,
    mode: BackupMode,
    suffix: &str,
    journal: Option<&UndoJournal>,
    tool: &str,
) -> io::Result<PathBuf> {
    let backup = backup_name(dst, mode, suffix);
    let saved = match journal {
        Some(j) => j.backup_existing(&backup, false)?,
        None => None,
    };
    if let Err(e) = fs::rename(dst, &backup) {
        if let Some(s) = &saved {
            let _ = move_path(Path::new(s), &backup);
        }
        return Err(e);
    }
    if let Some(j) = journal {
        let _ = j.record(
            tool,
            JournalAction::Move {
                src: absolute_lossy(dst),
                dst: absolute_lossy(&backup),
                backup_path: saved,
            },
        );
    }
    Ok(backup)
}

fn absolute_lossy(p: &Path) -> String {
    std::path::absolute(p)
        .unwrap_or_else(|_| p.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// gnulib `last_component`: the final name in `s`, ignoring trailing slashes
/// (empty for `/` or the empty string).
fn last_component(s: &[u8]) -> &[u8] {
    let mut base = s.iter().take_while(|&&b| b == b'/').count();
    let mut last_was_slash = false;
    for (i, &b) in s.iter().enumerate().skip(base) {
        if b == b'/' {
            last_was_slash = true;
        } else if last_was_slash {
            base = i;
            last_was_slash = false;
        }
    }
    &s[base..]
}

/// gnulib `base_len`: length of `s` without trailing slashes (but `/` stays).
fn base_len(s: &[u8]) -> usize {
    let mut len = s.len();
    while len > 1 && s[len - 1] == b'/' {
        len -= 1;
    }
    len
}

/// The file name a source keeps inside a target directory (`d/` → `d`).
fn operand_base(src: &str) -> OsString {
    let last = last_component(src.as_bytes());
    OsString::from_vec(last[..base_len(last)].to_vec())
}

/// `dir/name` without doubling a trailing slash (gnulib `file_name_concat`).
fn concat_path(dir: &str, name: &OsStr) -> PathBuf {
    let mut s = OsString::from(dir);
    if !dir.ends_with('/') {
        s.push("/");
    }
    s.push(name);
    PathBuf::from(s)
}

/// Strips trailing slashes, keeping a lone `/`.
fn strip_trailing_slashes(s: &str) -> String {
    let trimmed = s.trim_end_matches('/');
    if trimmed.is_empty() && !s.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// True if `a` and `b` (already stat'ed) are the same inode.
fn same_inode(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// The names in `dir`: in `readdir` order (as GNU's `fts` visits them), or sorted
/// by inode number (as GNU `cp` copies them).
fn dir_names(dir: &Path, by_inode: bool) -> io::Result<Vec<OsString>> {
    let mut names = fs::read_dir(dir)?
        .map(|e| e.map(|e| (e.ino(), e.file_name())))
        .collect::<io::Result<Vec<_>>>()?;
    if by_inode {
        names.sort_by_key(|(ino, _)| *ino);
    }
    Ok(names.into_iter().map(|(_, name)| name).collect())
}

fn c_path(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

/// True when the calling user may not write `p` (GNU asks before removing or
/// replacing such files). Symbolic links are never write-protected.
fn write_protected(p: &Path) -> bool {
    let Ok(c) = c_path(p) else {
        return false;
    };
    let denied = unsafe { libc::access(c.as_ptr(), libc::W_OK) } != 0;
    denied && io::Error::last_os_error().raw_os_error() == Some(libc::EACCES)
}

/// A time to set with `utimensat`.
#[derive(Clone, Copy)]
enum Stamp {
    Now,
    Omit,
    At(i64, u32),
}

fn set_file_times(path: &Path, atime: Stamp, mtime: Stamp, follow: bool) -> io::Result<()> {
    let spec = |s: Stamp| match s {
        Stamp::Now => libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_NOW,
        },
        Stamp::Omit => libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        Stamp::At(sec, nsec) => libc::timespec {
            tv_sec: sec,
            tv_nsec: nsec.into(),
        },
    };
    let times = [spec(atime), spec(mtime)];
    let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
    let c = c_path(path)?;
    if unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), flags) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Decides whether `rm`, `mv` or `cp` should ask before acting on a file.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Interactive {
    /// Never ask (`-f`).
    Never,
    /// Ask only about write-protected files when stdin is a terminal (the default).
    Sometimes,
    /// Always ask (`-i`).
    Always,
}

/// The `--preserve-root` failsafe message for recursive `rm`, `chmod` and `chown`.
fn refuse_root(tool: &str, operand: &str) {
    if operand == "/" {
        eprintln!("{tool}: it is dangerous to operate recursively on '/'");
    } else {
        eprintln!(
            "{tool}: it is dangerous to operate recursively on {} (same as '/')",
            qa(operand)
        );
    }
    eprintln!("{tool}: use --no-preserve-root to override this failsafe");
}

// ---------------------------------------------------------------------------
// 1. mkdir
// ---------------------------------------------------------------------------
const MKDIR_HELP: &str = "\
Usage: mkdir [OPTION]... DIRECTORY...
Create the DIRECTORY(ies), if they do not already exist.

  -m, --mode=MODE   set file mode (as in chmod), not a=rwx - umask
  -p, --parents     no error if existing, make parent directories as needed,
                    with their file modes unaffected by any -m option
  -v, --verbose     print a message for each created directory
  -Z, --context     accepted for compatibility (SELinux is not supported)
      --help        display this help and exit
      --version     output version information and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum MkdirOpt {
    Mode,
    Parents,
    Verbose,
    Context,
}

pub fn run_mkdir(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    use MkdirOpt::*;
    const SPECS: &[ToolOpt<MkdirOpt>] = &[
        opt("m", "mode", HasArg::Required, Mode),
        opt("p", "parents", HasArg::No, Parents),
        opt("v", "verbose", HasArg::No, Verbose),
        opt("Z", "", HasArg::No, Context),
        opt("", "context", HasArg::Optional, Context),
    ];
    let parsed = match parse_tool_args("mkdir", &args, SPECS, MKDIR_HELP) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let (mut mode_arg, mut parents, mut verbose) = (None, false, false);
    for (o, v) in parsed.opts {
        match o {
            Mode => mode_arg = v,
            Parents => parents = true,
            Verbose => verbose = true,
            Context => {}
        }
    }
    if parsed.operands.is_empty() {
        return Ok(usage_error("mkdir", "missing operand"));
    }

    // As in GNU: the final directory is made under `umask & !mode` (so -m wins),
    // ancestors under the umask minus u+wx.
    let umask = process_umask();
    let (mode, mode_bits) = match &mode_arg {
        Some(m) => match mode_compile(m) {
            Some(changes) => mode_adjust(S_IRWXUGO, true, umask, &changes),
            None => {
                eprintln!("mkdir: invalid mode {}", quote_locale(m));
                return Ok(1);
            }
        },
        None => (S_IRWXUGO, 0),
    };
    let masks = MkdirMasks {
        own: if mode_arg.is_some() {
            umask & !mode
        } else {
            umask
        },
        ancestor: umask & !0o300,
    };

    let mut status = 0;
    for dir in &parsed.operands {
        match mkdir_one(dir, parents, mode, mode_bits, masks, verbose) {
            Ok(()) => ctx.telemetry.add_items(1),
            Err(msg) => {
                eprintln!("mkdir: {msg}");
                status = 1;
            }
        }
    }
    Ok(status)
}

/// The umasks `mkdir` creates directories under.
#[derive(Clone, Copy)]
struct MkdirMasks {
    own: u32,
    ancestor: u32,
}

/// mkdir(2) under a given umask (restored afterwards).
fn mkdir_with_umask(path: &str, mode: u32, umask: u32) -> io::Result<()> {
    let old = unsafe { libc::umask(u32_to_mode_t(umask)) };
    let res = fs::DirBuilder::new().mode(mode).create(path);
    unsafe { libc::umask(old) };
    res
}

/// Byte offsets where each non-final component of `path` ends.
fn ancestor_ends(path: &str) -> Vec<usize> {
    let b = path.as_bytes();
    let mut ends = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'/' {
            i += 1;
            continue;
        }
        while i < b.len() && b[i] != b'/' {
            i += 1;
        }
        ends.push(i);
    }
    ends.pop();
    ends
}

fn mkdir_one(
    dir: &str,
    parents: bool,
    mode: u32,
    mode_bits: u32,
    masks: MkdirMasks,
    verbose: bool,
) -> std::result::Result<(), String> {
    let announce = |d: &str| {
        if verbose {
            println!("mkdir: created directory {}", qa(d));
        }
    };
    let cannot =
        |d: &str, why: String| format!("cannot create directory {}: {why}", quote_locale(d));
    if parents {
        for end in ancestor_ends(dir) {
            let prefix = &dir[..end];
            match mkdir_with_umask(prefix, S_IRWXUGO, masks.ancestor) {
                Ok(()) => announce(prefix),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    if !Path::new(prefix).is_dir() {
                        let dangling = fs::metadata(prefix).is_err();
                        let why = if dangling {
                            "File exists"
                        } else {
                            "Not a directory"
                        };
                        return Err(cannot(prefix, why.to_string()));
                    }
                }
                Err(e) => return Err(cannot(prefix, gnu_io_error(&e))),
            }
        }
    }
    // gnulib `make_dir_parents`: when special bits are involved, create the
    // directory without group/other write first, then fix the bits -m asked for.
    let keep_special = ((mode_bits & (S_ISUID | S_ISGID)) | (mode & S_ISVTX)) == 0;
    let mkdir_mode = if keep_special { mode } else { mode & !0o022 };
    match mkdir_with_umask(dir, mkdir_mode, masks.own) {
        Ok(()) => {
            announce(dir);
            let umask_ok = mode & mode_bits & S_IRWXUGO == 0;
            if !(keep_special && umask_ok)
                && let Ok(meta) = fs::metadata(dir)
            {
                let st = meta.mode() & CHMOD_MODE_BITS;
                if (st ^ mode) & mode_bits != 0 {
                    let new = mode | (st & !mode_bits);
                    fs::set_permissions(dir, fs::Permissions::from_mode(new)).map_err(|e| {
                        format!(
                            "cannot change permissions of {}: {}",
                            qa(dir),
                            gnu_io_error(&e)
                        )
                    })?;
                }
            }
            Ok(())
        }
        Err(e)
            if parents && e.kind() == io::ErrorKind::AlreadyExists && Path::new(dir).is_dir() =>
        {
            Ok(())
        }
        Err(e) => Err(cannot(dir, gnu_io_error(&e))),
    }
}

// ---------------------------------------------------------------------------
// 2. touch — `-d` understands the common GNU date formats (see `parse_datetime`)
// ---------------------------------------------------------------------------
const TOUCH_HELP: &str = "\
Usage: touch [OPTION]... FILE...
Update the access and modification times of each FILE to the current time.

A FILE argument that does not exist is created empty, unless -c or -h
is supplied.

  -a                     change only the access time
  -c, --no-create        do not create any files
  -d, --date=STRING      parse STRING and use it instead of current time
  -f                     (ignored)
  -h, --no-dereference   affect each symbolic link instead of any referenced
                         file
  -m                     change only the modification time
  -r, --reference=FILE   use this file's times instead of current time
  -t STAMP               use [[CC]YY]MMDDhhmm[.ss] instead of current time
      --time=WORD        change the specified time:
                           WORD is access, atime, or use: equivalent to -a
                           WORD is modify or mtime: equivalent to -m
      --help             display this help and exit
      --version          output version information and exit

STRING may be '@SECONDS', an ISO date such as '2020-01-31 13:45:00.5 +0100',
'Jan 31 2020', '1/31/2020', and relative items like 'yesterday',
'2 days ago' or '+3 hours'.
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum TouchOpt {
    Access,
    NoCreate,
    Date,
    Ignored,
    NoDeref,
    Modify,
    Reference,
    StampArg,
    Time,
}

pub fn run_touch(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    use TouchOpt::*;
    const SPECS: &[ToolOpt<TouchOpt>] = &[
        opt("a", "", HasArg::No, Access),
        opt("c", "no-create", HasArg::No, NoCreate),
        opt("d", "date", HasArg::Required, Date),
        opt("f", "", HasArg::No, Ignored),
        opt("h", "no-dereference", HasArg::No, NoDeref),
        opt("m", "", HasArg::No, Modify),
        opt("r", "reference", HasArg::Required, Reference),
        opt("t", "", HasArg::Required, StampArg),
        opt("", "time", HasArg::Required, Time),
    ];
    let parsed = match parse_tool_args("touch", &args, SPECS, TOUCH_HELP) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let (mut atime, mut mtime, mut no_create, mut no_deref) = (false, false, false, false);
    let (mut date, mut reference, mut stamp) = (None, None, None);
    for (o, v) in parsed.opts {
        match o {
            Access => atime = true,
            Modify => mtime = true,
            NoCreate => no_create = true,
            NoDeref => no_deref = true,
            Date => date = v,
            Reference => reference = v,
            StampArg => stamp = v,
            Time => {
                const WORDS: &[(&str, bool)] = &[
                    ("atime", true),
                    ("access", true),
                    ("use", true),
                    ("mtime", false),
                    ("modify", false),
                ];
                match argmatch("touch", "--time", v.as_deref().unwrap_or_default(), WORDS) {
                    Ok(true) => atime = true,
                    Ok(false) => mtime = true,
                    Err(code) => return Ok(code),
                }
            }
            Ignored => {}
        }
    }
    if stamp.is_some() && (date.is_some() || reference.is_some()) {
        return Ok(usage_error(
            "touch",
            "cannot specify times from more than one source",
        ));
    }
    if !atime && !mtime {
        atime = true;
        mtime = true;
    }
    if parsed.operands.is_empty() {
        return Ok(usage_error("touch", "missing file operand"));
    }

    let now = now_stamp();
    let (mut a_stamp, mut m_stamp) = (Stamp::Now, Stamp::Now);
    if let Some(r) = &reference {
        let meta = if no_deref {
            fs::symlink_metadata(r)
        } else {
            fs::metadata(r)
        };
        let meta = match meta {
            Ok(m) => m,
            Err(e) => {
                eprintln!(
                    "touch: failed to get attributes of {}: {}",
                    qa(r),
                    gnu_io_error(&e)
                );
                return Ok(1);
            }
        };
        let ns = |n: i64| u32::try_from(n).unwrap_or(0);
        a_stamp = Stamp::At(meta.atime(), ns(meta.atime_nsec()));
        m_stamp = Stamp::At(meta.mtime(), ns(meta.mtime_nsec()));
    }
    if let Some(d) = &date {
        // With -r, the date is relative to the reference file's times.
        let base = |s: Stamp| match s {
            Stamp::At(sec, ns) => (sec, ns),
            _ => now,
        };
        match (
            parse_datetime(d, base(a_stamp)),
            parse_datetime(d, base(m_stamp)),
        ) {
            (Some((asec, ans)), Some((msec, mns))) => {
                a_stamp = Stamp::At(asec, ans);
                m_stamp = Stamp::At(msec, mns);
            }
            _ => {
                eprintln!("touch: invalid date format {}", quote_locale(d));
                return Ok(1);
            }
        }
    }
    if let Some(t) = &stamp {
        match parse_touch_stamp(t) {
            Some((sec, ns)) => {
                a_stamp = Stamp::At(sec, ns);
                m_stamp = Stamp::At(sec, ns);
            }
            None => {
                eprintln!("touch: invalid date format {}", quote_locale(t));
                return Ok(1);
            }
        }
    }
    if !atime {
        a_stamp = Stamp::Omit;
    }
    if !mtime {
        m_stamp = Stamp::Omit;
    }

    let mut status = 0;
    for file in &parsed.operands {
        let path = Path::new(file);
        let mut open_err = None;
        if !no_create && !no_deref {
            // O_CREAT without O_TRUNC: never truncates, even if the file appeared meanwhile.
            if let Err(e) = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
                .mode(0o666)
                .open(path)
            {
                open_err = Some(e);
            }
        }
        match set_file_times(path, a_stamp, m_stamp, !no_deref) {
            Ok(()) => ctx.telemetry.add_items(1),
            Err(e) => {
                if let Some(oe) = open_err {
                    eprintln!("touch: cannot touch {}: {}", qa(file), gnu_io_error(&oe));
                } else if no_create && e.raw_os_error() == Some(libc::ENOENT) {
                    continue;
                } else {
                    eprintln!("touch: setting times of {}: {}", qa(file), gnu_io_error(&e));
                }
                status = 1;
            }
        }
    }
    Ok(status)
}

fn now_stamp() -> (i64, u32) {
    let d = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    (
        i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        d.subsec_nanos(),
    )
}

/// Seconds since the epoch of a local wall-clock time; in a DST gap the time is
/// read with the offset in force before the gap (as `mktime` does).
fn local_to_epoch(date: NaiveDate, secs_of_day: i64) -> Option<i64> {
    let midnight = date.and_hms_opt(0, 0, 0)?;
    let naive = midnight + chrono::Duration::seconds(secs_of_day);
    match Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(t) => Some(t.timestamp()),
        chrono::LocalResult::Ambiguous(t, _) => Some(t.timestamp()),
        chrono::LocalResult::None => {
            let before = Local
                .from_local_datetime(&(naive - chrono::Duration::hours(3)))
                .earliest()?;
            Some(naive.and_utc().timestamp() - i64::from(before.offset().local_minus_utc()))
        }
    }
}

fn days_in_month(year: i64, month: u32) -> u32 {
    let y = i32::try_from(year).unwrap_or(1970);
    let next = if month == 12 {
        NaiveDate::from_ymd_opt(y + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(y, month + 1, 1)
    };
    next.and_then(|n| n.pred_opt()).map_or(31, |d| d.day())
}

/// `touch -t [[CC]YY]MMDDhhmm[.ss]`, in local time.
fn parse_touch_stamp(s: &str) -> Option<(i64, u32)> {
    let (main, secs) = match s.split_once('.') {
        Some((m, ss)) if ss.len() == 2 && ss.bytes().all(|b| b.is_ascii_digit()) => {
            (m, ss.parse::<i64>().ok()?)
        }
        Some(_) => return None,
        None => (s, 0),
    };
    if !main.bytes().all(|b| b.is_ascii_digit()) || ![8, 10, 12].contains(&main.len()) {
        return None;
    }
    let num = |r: std::ops::Range<usize>| main[r].parse::<u32>().ok();
    let n = main.len();
    let (month, day, hour, minute) = (
        num(n - 8..n - 6)?,
        num(n - 6..n - 4)?,
        num(n - 4..n - 2)?,
        num(n - 2..n)?,
    );
    let year = match n {
        12 => i64::from(num(0..4)?),
        10 => {
            let yy = i64::from(num(0..2)?);
            if yy < 69 { 2000 + yy } else { 1900 + yy }
        }
        _ => i64::from(Local::now().year()),
    };
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || secs > 60
    {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(i32::try_from(year).ok()?, month, day)?;
    let epoch = local_to_epoch(date, i64::from(hour * 3600 + minute * 60))?;
    Some((epoch + secs, 0))
}

/// Time-zone abbreviations GNU `parse_datetime` knows, in seconds east of UTC.
const TZ_NAMES: &[(&str, i64)] = &[
    ("gmt", 0),
    ("ut", 0),
    ("utc", 0),
    ("z", 0),
    ("wet", 0),
    ("west", 3600),
    ("bst", 3600),
    ("art", -3 * 3600),
    ("brt", -3 * 3600),
    ("brst", -2 * 3600),
    ("nst", -(3 * 3600 + 1800)),
    ("ndt", -(2 * 3600 + 1800)),
    ("ast", -4 * 3600),
    ("adt", -3 * 3600),
    ("est", -5 * 3600),
    ("edt", -4 * 3600),
    ("cst", -6 * 3600),
    ("cdt", -5 * 3600),
    ("mst", -7 * 3600),
    ("mdt", -6 * 3600),
    ("pst", -8 * 3600),
    ("pdt", -7 * 3600),
    ("akst", -9 * 3600),
    ("akdt", -8 * 3600),
    ("hst", -10 * 3600),
    ("hast", -10 * 3600),
    ("hadt", -9 * 3600),
    ("sst", -11 * 3600),
    ("wat", 3600),
    ("cet", 3600),
    ("cest", 2 * 3600),
    ("met", 3600),
    ("mez", 3600),
    ("mest", 2 * 3600),
    ("mesz", 2 * 3600),
    ("eet", 2 * 3600),
    ("eest", 3 * 3600),
    ("cat", 2 * 3600),
    ("sast", 2 * 3600),
    ("eat", 3 * 3600),
    ("msk", 3 * 3600),
    ("msd", 4 * 3600),
    ("ist", 5 * 3600 + 1800),
    ("sgt", 8 * 3600),
    ("kst", 9 * 3600),
    ("jst", 9 * 3600),
    ("gst", 10 * 3600),
    ("nzst", 12 * 3600),
    ("nzdt", 13 * 3600),
];

const MONTH_NAMES: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

const DAY_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn month_number(word: &str) -> Option<u32> {
    let word = word.strip_suffix('.').unwrap_or(word);
    if word == "sept" {
        return Some(9);
    }
    let pos = MONTH_NAMES
        .iter()
        .position(|m| word.len() >= 3 && m.starts_with(&word[..3]))?;
    let full = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ][pos];
    (word.len() == 3 || full == word).then_some(pos as u32 + 1)
}

fn day_number(word: &str) -> Option<u32> {
    let word = word.strip_suffix('.').unwrap_or(word);
    let full = [
        "sunday",
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
    ];
    let pos = DAY_NAMES
        .iter()
        .position(|d| word.len() >= 3 && d.starts_with(&word[..3]))?;
    (word.len() == 3 || full[pos] == word || (word.len() == 4 && full[pos].starts_with(word)))
        .then_some(pos as u32)
}

/// Relative units: (months, seconds) per unit.
fn rel_unit(word: &str) -> Option<(i64, i64)> {
    let word = word
        .strip_suffix('s')
        .filter(|w| !w.is_empty())
        .unwrap_or(word);
    Some(match word {
        "year" => (12, 0),
        "month" => (1, 0),
        "fortnight" => (0, 14 * 86400),
        "week" => (0, 7 * 86400),
        "day" => (0, 86400),
        "hour" => (0, 3600),
        "minute" | "min" => (0, 60),
        "second" | "sec" => (0, 1),
        _ => return None,
    })
}

/// The items of a date string, as GNU `parse_datetime` collects them.
#[derive(Default)]
struct DateItems {
    year: Option<(i64, usize)>,
    month_day: Option<(u32, u32)>,
    time: Option<(u32, u32, u32, u32)>,
    meridian: Option<bool>,
    zone: Option<i64>,
    /// Day of the week (0 = Sunday) and its ordinal (`next` = 1, `last` = -1).
    weekday: Option<(u32, i64)>,
    rel_months: i64,
    rel_days: i64,
    rel_secs: i64,
    rels_seen: bool,
    last_rel: Option<(i64, i64, i64)>,
}

impl DateItems {
    fn add_rel(&mut self, n: i64, (months, secs): (i64, i64)) {
        let (m, d, s) = if secs % 86400 == 0 && secs != 0 {
            (0, n * (secs / 86400), 0)
        } else {
            (n * months, 0, n * secs)
        };
        self.rel_months += m;
        self.rel_days += d;
        self.rel_secs += s;
        self.rels_seen = true;
        self.last_rel = Some((m, d, s));
    }

    fn set_date(&mut self, year: Option<(i64, usize)>, month: u32, day: u32) -> Option<()> {
        if self.month_day.is_some() {
            return None;
        }
        self.month_day = Some((month, day));
        if year.is_some() {
            self.year = year;
        }
        Some(())
    }

    fn set_time(&mut self, t: (u32, u32, u32, u32)) -> Option<()> {
        if self.time.is_some() {
            return None;
        }
        self.time = Some(t);
        Some(())
    }
}

/// A cursor over the lowercased input.
struct DateLexer<'a> {
    s: &'a [u8],
    i: usize,
}

impl DateLexer<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn skip_space(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    fn digits(&mut self) -> Option<(i64, usize)> {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        let len = self.i - start;
        if len == 0 || len > 18 {
            return None;
        }
        Some((
            std::str::from_utf8(&self.s[start..self.i])
                .ok()?
                .parse()
                .ok()?,
            len,
        ))
    }

    fn word(&mut self) -> &str {
        let start = self.i;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'.')
        {
            self.i += 1;
        }
        std::str::from_utf8(&self.s[start..self.i]).unwrap_or("")
    }

    fn peek_word(&self) -> &str {
        let mut j = self.i;
        while self.s.get(j).is_some_and(|c| c.is_ascii_whitespace()) {
            j += 1;
        }
        let start = j;
        while self.s.get(j).is_some_and(|c| c.is_ascii_alphabetic()) {
            j += 1;
        }
        std::str::from_utf8(&self.s[start..j]).unwrap_or("")
    }

    /// `HH:MM[:SS[.frac]]` after the hour has been read.
    fn clock(&mut self, hour: i64) -> Option<(u32, u32, u32, u32)> {
        self.i += 1; // ':'
        let (minute, mlen) = self.digits()?;
        if mlen != 2 {
            return None;
        }
        let (mut second, mut nanos) = (0, 0);
        if self.peek() == Some(b':') {
            self.i += 1;
            let (s, slen) = self.digits()?;
            if slen != 2 {
                return None;
            }
            second = s;
            if matches!(self.peek(), Some(b'.' | b','))
                && self.s.get(self.i + 1).is_some_and(u8::is_ascii_digit)
            {
                self.i += 1;
                let start = self.i;
                while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.i += 1;
                }
                let frac = &self.s[start..self.i];
                for k in 0..9 {
                    nanos = nanos * 10 + frac.get(k).map_or(0, |d| u32::from(d - b'0'));
                }
            }
        }
        Some((
            u32::try_from(hour).ok()?,
            u32::try_from(minute).ok()?,
            u32::try_from(second).ok()?,
            nanos,
        ))
    }

    /// A numeric zone `+HH`, `+HHMM` or `+HH:MM`, if one starts here.
    fn numeric_zone(&mut self) -> Option<i64> {
        let save = self.i;
        let sign = match self.peek() {
            Some(b'+') => 1,
            Some(b'-') => -1,
            _ => return None,
        };
        self.i += 1;
        let Some((n, len)) = self.digits() else {
            self.i = save;
            return None;
        };
        let secs = match len {
            1 | 2 if self.peek() == Some(b':') => {
                self.i += 1;
                let (m, mlen) = self.digits()?;
                if mlen != 2 {
                    return None;
                }
                n * 3600 + m * 60
            }
            1 | 2 => n * 3600,
            4 => (n / 100) * 3600 + (n % 100) * 60,
            _ => {
                self.i = save;
                return None;
            }
        };
        Some(sign * secs)
    }

    /// After a time of day: an optional am/pm and an optional zone.
    fn time_suffix(&mut self, items: &mut DateItems) -> Option<()> {
        let save = self.i;
        self.skip_space();
        let w = self.peek_word().to_string();
        let meridian = match w.as_str() {
            "am" | "a" => Some(false),
            "pm" | "p" => Some(true),
            _ => None,
        };
        if let Some(pm) = meridian {
            self.word();
            if self.peek() == Some(b'.') {
                self.i += 1;
            }
            items.meridian = Some(pm);
        } else {
            self.i = save;
        }
        if self.peek() == Some(b'z') && !self.s.get(self.i + 1).is_some_and(u8::is_ascii_alphabetic)
        {
            self.i += 1;
            items.zone = Some(0);
        } else if let Some(z) = self.numeric_zone() {
            items.zone = Some(z);
        }
        Some(())
    }
}

/// Parses the date formats GNU `touch -d` / `date -d` accept most often:
/// `@SECONDS[.frac]`, ISO 8601 dates and times (`2020-01-31`, `2020-01-31T13:45:00Z`,
/// `2020-01-31 13:45:00.5 +0100`), `YYYYMMDD`, `1/31/2020`, `Jan 31 2020`,
/// `31 Jan 2020`, times with `am`/`pm`, zone names (`UTC`, `EST`, `CET`, …),
/// weekdays, and relative items (`yesterday`, `2 days ago`, `+3 hours`,
/// `next week`). `now` is `(seconds, nanoseconds)`; unspecified fields default
/// like GNU: a date alone means midnight, relative items alone apply to `now`.
fn parse_datetime(input: &str, now: (i64, u32)) -> Option<(i64, u32)> {
    let lower = input.trim().to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix('@') {
        return parse_epoch(rest.trim());
    }
    let mut lx = DateLexer {
        s: lower.as_bytes(),
        i: 0,
    };
    let mut it = DateItems::default();
    loop {
        lx.skip_space();
        let Some(c) = lx.peek() else { break };
        if c.is_ascii_digit() {
            let (n, len) = lx.digits()?;
            match lx.peek() {
                Some(b'-')
                    if lx.s.get(lx.i + 1).is_some_and(u8::is_ascii_digit)
                        && it.month_day.is_none() =>
                {
                    // ISO 8601: YYYY-MM-DD[THH:MM[:SS]]
                    lx.i += 1;
                    let (month, _) = lx.digits()?;
                    if lx.peek() != Some(b'-') {
                        return None;
                    }
                    lx.i += 1;
                    let (day, _) = lx.digits()?;
                    it.set_date(
                        Some((n, len)),
                        u32::try_from(month).ok()?,
                        u32::try_from(day).ok()?,
                    )?;
                    if lx.peek() == Some(b't') && lx.s.get(lx.i + 1).is_some_and(u8::is_ascii_digit)
                    {
                        lx.i += 1;
                        let (h, _) = lx.digits()?;
                        if lx.peek() != Some(b':') {
                            return None;
                        }
                        let t = lx.clock(h)?;
                        it.set_time(t)?;
                        lx.time_suffix(&mut it)?;
                    }
                }
                Some(b':') => {
                    let t = lx.clock(n)?;
                    it.set_time(t)?;
                    lx.time_suffix(&mut it)?;
                }
                Some(b'/') => {
                    // US style: MM/DD[/YYYY]
                    lx.i += 1;
                    let (day, _) = lx.digits()?;
                    let mut year = None;
                    if lx.peek() == Some(b'/') {
                        lx.i += 1;
                        year = Some(lx.digits()?);
                    }
                    it.set_date(year, u32::try_from(n).ok()?, u32::try_from(day).ok()?)?;
                }
                _ => {
                    let next = lx.peek_word().to_string();
                    if let Some(unit) = rel_unit(&next) {
                        lx.skip_space();
                        lx.word();
                        it.add_rel(n, unit);
                    } else if let Some(month) = month_number(&next) {
                        // DD Month [YYYY]
                        lx.skip_space();
                        lx.word();
                        lx.skip_space();
                        let save = lx.i;
                        let year = match lx.digits() {
                            Some(y) if lx.peek() != Some(b':') => Some(y),
                            _ => {
                                lx.i = save;
                                None
                            }
                        };
                        it.set_date(year, month, u32::try_from(n).ok()?)?;
                    } else if matches!(next.as_str(), "am" | "pm" | "a" | "p") && len <= 2 {
                        it.set_time((u32::try_from(n).ok()?, 0, 0, 0))?;
                        lx.time_suffix(&mut it)?;
                    } else if it.month_day.is_some()
                        && it.year.is_none()
                        && !it.rels_seen
                        && (it.time.is_some() || len > 2)
                    {
                        it.year = Some((n, len));
                    } else if len > 4 {
                        it.set_date(
                            Some((n / 10000, len - 4)),
                            u32::try_from((n / 100) % 100).ok()?,
                            u32::try_from(n % 100).ok()?,
                        )?;
                    } else {
                        let (h, m) = if len <= 2 { (n, 0) } else { (n / 100, n % 100) };
                        it.set_time((u32::try_from(h).ok()?, u32::try_from(m).ok()?, 0, 0))?;
                    }
                }
            }
        } else if c == b'+' || c == b'-' {
            let save = lx.i;
            if it.time.is_some()
                && it.zone.is_none()
                && let Some(z) = lx.numeric_zone()
                && rel_unit(lx.peek_word()).is_none()
            {
                it.zone = Some(z);
                continue;
            }
            lx.i = save + 1;
            let (n, _) = lx.digits()?;
            let n = if c == b'-' { -n } else { n };
            let unit = rel_unit(lx.peek_word())?;
            lx.skip_space();
            lx.word();
            it.add_rel(n, unit);
        } else if c.is_ascii_alphabetic() {
            let word = lx.word().to_string();
            let word = word.trim_end_matches('.');
            if let Some(month) = month_number(word) {
                // Month DD [[,] YYYY]
                lx.skip_space();
                let (day, _) = lx.digits()?;
                if lx.peek() == Some(b',') {
                    lx.i += 1;
                }
                lx.skip_space();
                let save = lx.i;
                let year = match lx.digits() {
                    Some(y) if lx.peek() != Some(b':') => Some(y),
                    _ => {
                        lx.i = save;
                        None
                    }
                };
                it.set_date(year, month, u32::try_from(day).ok()?)?;
            } else if let Some(day) = day_number(word) {
                if it.weekday.replace((day, 0)).is_some() {
                    return None;
                }
            } else if let Some(&(_, off)) = TZ_NAMES.iter().find(|(name, _)| *name == word) {
                if it.zone.is_some() {
                    return None;
                }
                let correction = lx.numeric_zone().unwrap_or(0);
                it.zone = Some(off + correction);
            } else {
                match word {
                    "now" | "today" => it.rels_seen = true,
                    "yesterday" => it.add_rel(-1, (0, 86400)),
                    "tomorrow" => it.add_rel(1, (0, 86400)),
                    "ago" => {
                        let (m, d, s) = it.last_rel.take()?;
                        it.rel_months -= 2 * m;
                        it.rel_days -= 2 * d;
                        it.rel_secs -= 2 * s;
                    }
                    "next" | "last" | "this" => {
                        let n = match word {
                            "next" => 1,
                            "last" => -1,
                            _ => 0,
                        };
                        let next = lx.peek_word().to_string();
                        if let Some(day) = day_number(&next) {
                            if it.weekday.replace((day, n)).is_some() {
                                return None;
                            }
                        } else {
                            it.add_rel(n, rel_unit(&next)?);
                        }
                        lx.skip_space();
                        lx.word();
                    }
                    w => it.add_rel(1, rel_unit(w)?),
                }
            }
        } else {
            return None;
        }
    }
    resolve_date(&it, now, lower.is_empty())
}

/// `@SECONDS[.frac]`.
fn parse_epoch(s: &str) -> Option<(i64, u32)> {
    let (neg, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let (int, frac) = match digits.find(['.', ',']) {
        Some(i) => (&digits[..i], &digits[i + 1..]),
        None => (digits, ""),
    };
    if int.is_empty()
        || !int.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let secs: i64 = int.parse().ok()?;
    let mut nanos: u32 = 0;
    for k in 0..9 {
        nanos = nanos * 10 + frac.as_bytes().get(k).map_or(0, |d| u32::from(d - b'0'));
    }
    if !neg {
        return Some((secs, nanos));
    }
    if nanos == 0 {
        Some((-secs, 0))
    } else {
        Some((-secs - 1, 1_000_000_000 - nanos))
    }
}

fn resolve_date(it: &DateItems, now: (i64, u32), empty: bool) -> Option<(i64, u32)> {
    let now_local = Local.timestamp_opt(now.0, now.1).single()?;
    let has_date = it.month_day.is_some();
    let (mut year, mut month, mut day) = (
        i64::from(now_local.year()),
        now_local.month(),
        now_local.day(),
    );
    if let Some((y, digits)) = it.year {
        year = if digits == 2 {
            if y < 69 { y + 2000 } else { y + 1900 }
        } else {
            y
        };
    }
    if let Some((m, d)) = it.month_day {
        month = m;
        day = d;
    }
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    let (mut hour, minute, second, nanos) = match it.time {
        Some(t) => t,
        None if has_date || empty || it.weekday.is_some() => (0, 0, 0, 0),
        None => (
            now_local.hour(),
            now_local.minute(),
            now_local.second(),
            now.1,
        ),
    };
    if let Some(pm) = it.meridian {
        if hour == 0 || hour > 12 {
            return None;
        }
        hour = hour % 12 + if pm { 12 } else { 0 };
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }

    // A weekday moves to that day (unless a date was given), as GNU does:
    // `monday` is today or the coming Monday, `next monday` the one after today.
    let mut day = i64::from(day);
    if let Some((wd, ordinal)) = it.weekday.filter(|_| !has_date) {
        let today = NaiveDate::from_ymd_opt(i32::try_from(year).ok()?, month, 1)?
            .checked_add_signed(chrono::Duration::days(day - 1))?
            .weekday()
            .num_days_from_sunday();
        let (wd, today) = (i64::from(wd), i64::from(today));
        day += (wd - today + 7) % 7 + 7 * (ordinal - i64::from(ordinal > 0 && today != wd));
    }
    // Then relative months (with mktime-style day overflow), then days.
    let months = year * 12 + i64::from(month) - 1 + it.rel_months;
    let first = NaiveDate::from_ymd_opt(
        i32::try_from(months.div_euclid(12)).ok()?,
        u32::try_from(months.rem_euclid(12)).ok()? + 1,
        1,
    )?;
    let date = first.checked_add_signed(chrono::Duration::days(day - 1 + it.rel_days))?;
    let secs_of_day = i64::from(hour) * 3600 + i64::from(minute) * 60;
    let midnight_utc = date.and_hms_opt(0, 0, 0)?.and_utc().timestamp();
    let base = match it.zone {
        Some(z) => midnight_utc + secs_of_day - z,
        // Only relative items: like GNU, keep the current UTC offset, so
        // `next week` is exactly 7 * 24 hours away even across a DST change.
        None if !has_date && it.time.is_none() && it.weekday.is_none() && !empty => {
            midnight_utc + secs_of_day - i64::from(now_local.offset().local_minus_utc())
        }
        None => local_to_epoch(date, secs_of_day)?,
    };
    Some((base + i64::from(second) + it.rel_secs, nanos))
}

// ---------------------------------------------------------------------------
// 3. ln
// ---------------------------------------------------------------------------
const LN_HELP: &str = "\
Usage: ln [OPTION]... [-T] TARGET LINK_NAME
  or:  ln [OPTION]... TARGET
  or:  ln [OPTION]... TARGET... DIRECTORY
  or:  ln [OPTION]... -t DIRECTORY TARGET...
Create links to each TARGET; hard links by default, symbolic links with -s.

      --backup[=CONTROL]      make a backup of each existing destination file
  -b                          like --backup but does not accept an argument
  -d, -F, --directory         (hard links to directories are not allowed)
  -f, --force                 remove existing destination files
  -i, --interactive           prompt whether to remove destinations
  -L, --logical               dereference TARGETs that are symbolic links
  -n, --no-dereference        treat LINK_NAME as a normal file if
                                it is a symbolic link to a directory
  -P, --physical              make hard links directly to symbolic links
  -r, --relative              with -s, create links relative to link location
  -s, --symbolic              make symbolic links instead of hard links
  -S, --suffix=SUFFIX         override the usual backup suffix
  -t, --target-directory=DIRECTORY  specify the DIRECTORY in which to create
                                the links
  -T, --no-target-directory   treat LINK_NAME as a normal file always
  -v, --verbose               print name of each linked file
      --help        display this help and exit
      --version     output version information and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum LnOpt {
    Backup,
    BackupArg,
    Directory,
    Force,
    Interactive,
    Logical,
    NoDeref,
    Physical,
    Relative,
    Symbolic,
    Suffix,
    TargetDir,
    NoTargetDir,
    Verbose,
}

struct LnOpts {
    backup: BackupMode,
    suffix: String,
    force: bool,
    interactive: bool,
    logical: bool,
    relative: bool,
    symbolic: bool,
    verbose: bool,
}

pub fn run_ln(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    use LnOpt::*;
    const SPECS: &[ToolOpt<LnOpt>] = &[
        opt("b", "", HasArg::No, Backup),
        opt("", "backup", HasArg::Optional, BackupArg),
        opt("dF", "directory", HasArg::No, Directory),
        opt("f", "force", HasArg::No, Force),
        opt("i", "interactive", HasArg::No, Interactive),
        opt("L", "logical", HasArg::No, Logical),
        opt("n", "no-dereference", HasArg::No, NoDeref),
        opt("P", "physical", HasArg::No, Physical),
        opt("r", "relative", HasArg::No, Relative),
        opt("s", "symbolic", HasArg::No, Symbolic),
        opt("S", "suffix", HasArg::Required, Suffix),
        opt("t", "target-directory", HasArg::Required, TargetDir),
        opt("T", "no-target-directory", HasArg::No, NoTargetDir),
        opt("v", "verbose", HasArg::No, Verbose),
    ];
    let parsed = match parse_tool_args("ln", &args, SPECS, LN_HELP) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let mut o = LnOpts {
        backup: BackupMode::None,
        suffix: String::new(),
        force: false,
        interactive: false,
        logical: false,
        relative: false,
        symbolic: false,
        verbose: false,
    };
    let (mut want_backup, mut backup_arg, mut suffix) = (false, None, None);
    let (mut no_deref, mut target_dir, mut no_target_dir) = (false, None, false);
    for (opt, v) in parsed.opts {
        match opt {
            Backup => want_backup = true,
            BackupArg => {
                want_backup = true;
                backup_arg = v;
            }
            Directory => {}
            Force => {
                o.force = true;
                o.interactive = false;
            }
            Interactive => {
                o.force = false;
                o.interactive = true;
            }
            Logical => o.logical = true,
            Physical => o.logical = false,
            NoDeref => no_deref = true,
            Relative => o.relative = true,
            Symbolic => o.symbolic = true,
            Suffix => {
                want_backup = true;
                suffix = v;
            }
            TargetDir => target_dir = v,
            NoTargetDir => no_target_dir = true,
            Verbose => o.verbose = true,
        }
    }
    if want_backup {
        o.backup = match backup_mode("ln", backup_arg.as_deref()) {
            Ok(m) => m,
            Err(code) => return Ok(code),
        };
    }
    o.suffix = backup_suffix(suffix);
    if o.relative && !o.symbolic {
        eprintln!("ln: cannot do --relative without --symbolic");
        return Ok(1);
    }

    let mut files = parsed.operands;
    let is_dir = |p: &str| {
        let m = if no_deref {
            fs::symlink_metadata(p)
        } else {
            fs::metadata(p)
        };
        m.is_ok_and(|m| m.is_dir())
    };
    let target_dir = match (target_dir, no_target_dir) {
        (Some(_), true) => {
            eprintln!("ln: cannot combine --target-directory and --no-target-directory");
            return Ok(1);
        }
        (Some(d), false) => {
            if let Err(e) = fs::metadata(&d).and_then(|m| {
                if m.is_dir() {
                    Ok(())
                } else {
                    Err(io::Error::from_raw_os_error(libc::ENOTDIR))
                }
            }) {
                eprintln!("ln: target directory {}: {}", qa(&d), gnu_io_error(&e));
                return Ok(1);
            }
            if files.is_empty() {
                return Ok(usage_error("ln", "missing file operand"));
            }
            Some(d)
        }
        (None, true) => {
            match files.len() {
                0 => return Ok(usage_error("ln", "missing file operand")),
                1 => {
                    return Ok(usage_error(
                        "ln",
                        &format!("missing destination file operand after {}", qa(&files[0])),
                    ));
                }
                2 => {}
                _ => {
                    return Ok(usage_error(
                        "ln",
                        &format!("extra operand {}", qa(&files[2])),
                    ));
                }
            }
            None
        }
        (None, false) => match files.len() {
            0 => return Ok(usage_error("ln", "missing file operand")),
            1 => Some(".".to_string()),
            n if is_dir(&files[n - 1]) => files.pop(),
            2 => None,
            n => {
                let last = &files[n - 1];
                let meta = if no_deref {
                    fs::symlink_metadata(last)
                } else {
                    fs::metadata(last)
                };
                let err = meta
                    .err()
                    .unwrap_or_else(|| io::Error::from_raw_os_error(libc::ENOTDIR));
                eprintln!("ln: target {}: {}", qa(last), gnu_io_error(&err));
                return Ok(1);
            }
        },
    };

    let mut status = 0;
    match target_dir {
        Some(dir) => {
            for src in &files {
                let dest = concat_path(&dir, &operand_base(src));
                if !ln_one(src, &dest, &o, ctx) {
                    status = 1;
                }
            }
        }
        None => {
            if !ln_one(&files[0], Path::new(&files[1]), &o, ctx) {
                status = 1;
            }
        }
    }
    Ok(status)
}

/// True when `a` and `b` name the same directory entry (same parent, same name).
fn same_name(a: &Path, b: &Path) -> bool {
    let parent = |p: &Path| match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    a.file_name() == b.file_name() && same_file(&parent(a), &parent(b))
}

fn ln_one(src: &str, dest: &Path, o: &LnOpts, ctx: &DogfishContext) -> bool {
    let src_path = Path::new(src);
    let src_meta = if o.symbolic {
        None
    } else {
        let m = if o.logical {
            fs::metadata(src_path)
        } else {
            fs::symlink_metadata(src_path)
        };
        match m {
            Err(e) => {
                eprintln!("ln: failed to access {}: {}", qa(src), gnu_io_error(&e));
                return false;
            }
            Ok(m) if m.is_dir() => {
                eprintln!("ln: {}: hard link not allowed for directory", qf(src));
                return false;
            }
            Ok(m) => Some(m),
        }
    };
    // With -L, resolve the source before DEST (which may be that symlink) is removed.
    let real_src = if o.logical && !o.symbolic {
        match fs::canonicalize(src_path) {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("ln: failed to access {}: {}", qa(src), gnu_io_error(&e));
                return false;
            }
        }
    } else {
        None
    };
    let target: OsString = if o.relative {
        relative_link_target(src_path, dest)
    } else {
        OsString::from(src)
    };

    let mut backup = None;
    if (o.force || o.interactive || o.backup != BackupMode::None)
        && let Ok(dm) = fs::symlink_metadata(dest)
    {
        if dm.is_dir() {
            eprintln!("ln: {}: cannot overwrite directory", qf(dest));
            return false;
        }
        // Replacing DEST must not delete the file SOURCE names.
        let same = (o.force || o.interactive || (o.backup != BackupMode::None && !o.symbolic))
            && if o.symbolic {
                fs::metadata(src_path).ok()
            } else {
                src_meta.clone()
            }
            .is_some_and(|sm| {
                same_inode(&sm, &dm) && (sm.nlink() == 1 || same_name(src_path, dest))
            });
        // Nor may `ln -sf` turn a file into a link to itself.
        let self_link = o.symbolic && o.force && !dm.is_symlink() && {
            let dir = dest.parent().unwrap_or(Path::new(""));
            same_file(&dir.join(&target), dest)
        };
        let fatal_same = same && (o.force || (o.backup != BackupMode::None && !o.symbolic));
        if same && !fatal_same {
            // `ln -i` replacing a file with itself is a no-op (GNU renames a fresh
            // link over it); never remove it.
            return ask("ln", &format!("replace {}? ", qa(dest)));
        }
        if fatal_same || self_link {
            eprintln!("ln: {} and {} are the same file", qa(src), qa(dest));
            return false;
        }
        if o.interactive && !ask("ln", &format!("replace {}? ", qa(dest))) {
            return false;
        }
        let remove = o.force || o.interactive;
        if o.backup != BackupMode::None {
            let name = backup_name(dest, o.backup, &o.suffix);
            match fs::rename(dest, &name) {
                Ok(()) => backup = Some(name),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    eprintln!("ln: cannot backup {}: {}", qa(dest), gnu_io_error(&e));
                    return false;
                }
            }
        } else if remove
            && let Err(e) = fs::remove_file(dest)
            && e.kind() != io::ErrorKind::NotFound
        {
            eprintln!("ln: cannot remove {}: {}", qa(dest), gnu_io_error(&e));
            return false;
        }
    }

    let res = if o.symbolic {
        symlink(&target, dest)
    } else if let Some(real) = &real_src {
        fs::hard_link(real, dest)
    } else {
        fs::hard_link(src_path, dest)
    };
    let arrow = if o.symbolic { "->" } else { "=>" };
    match res {
        Ok(()) => {
            ctx.telemetry.add_items(1);
            if o.verbose {
                match &backup {
                    Some(b) => println!("{} ~ {} {arrow} {}", qa(b), qa(dest), qa(&target)),
                    None => println!("{} {arrow} {}", qa(dest), qa(&target)),
                }
            }
            true
        }
        Err(e) => {
            let errno = e.raw_os_error().unwrap_or(0);
            let msg = if o.symbolic {
                if errno != libc::ENAMETOOLONG && !target.is_empty() {
                    format!("failed to create symbolic link {}", qa(dest))
                } else {
                    format!(
                        "failed to create symbolic link {} -> {}",
                        qa(dest),
                        qa(&target)
                    )
                }
            } else if errno == libc::EMLINK {
                format!("failed to create hard link to {}", qa(src))
            } else if [libc::EDQUOT, libc::EEXIST, libc::ENOSPC, libc::EROFS].contains(&errno) {
                format!("failed to create hard link {}", qa(dest))
            } else {
                format!("failed to create hard link {} => {}", qa(dest), qa(src))
            };
            eprintln!("ln: {msg}: {}", gnu_io_error(&e));
            if let Some(b) = backup {
                let _ = fs::rename(b, dest);
            }
            false
        }
    }
}

/// `ln -sr`: SOURCE relative to the directory that will contain DEST.
fn relative_link_target(src: &Path, dest: &Path) -> OsString {
    let real_src =
        canonicalize_mode(src, CanMode::Missing, false).unwrap_or_else(|_| src.to_path_buf());
    let real_dest =
        canonicalize_mode(dest, CanMode::Missing, false).unwrap_or_else(|_| dest.to_path_buf());
    let dir = real_dest.parent().unwrap_or(Path::new("/"));
    relpath(&real_src, dir).into_os_string()
}

// ---------------------------------------------------------------------------
// 4. chmod — octal and full symbolic modes, -R, -c/-v/-f, --reference
// ---------------------------------------------------------------------------
const CHMOD_HELP: &str = "\
Usage: chmod [OPTION]... MODE[,MODE]... FILE...
  or:  chmod [OPTION]... OCTAL-MODE FILE...
  or:  chmod [OPTION]... --reference=RFILE FILE...
Change the mode of each FILE to MODE.
With --reference, change the mode of each FILE to that of RFILE.

  -c, --changes          like verbose but report only when a change is made
  -f, --silent, --quiet  suppress most error messages
  -v, --verbose          output a diagnostic for every file processed
      --no-preserve-root  do not treat '/' specially (the default)
      --preserve-root    fail to operate recursively on '/'
      --reference=RFILE  use RFILE's mode instead of specifying MODE values
  -R, --recursive        change files and directories recursively
      --help        display this help and exit
      --version     output version information and exit

Each MODE is of the form '[ugoa]*([-+=]([rwxXst]*|[ugo]))+|[-+=][0-7]+'.
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChmodOpt {
    Changes,
    Quiet,
    Verbose,
    NoPreserveRoot,
    PreserveRoot,
    Reference,
    Recursive,
}

struct ChmodRun {
    changes: Option<Vec<ModeChange>>,
    reference_mode: u32,
    umask: u32,
    recursive: bool,
    report_changes: bool,
    verbose: bool,
    quiet: bool,
    diagnose_surprises: bool,
}

pub fn run_chmod(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    use ChmodOpt::*;
    const SPECS: &[ToolOpt<ChmodOpt>] = &[
        opt("c", "changes", HasArg::No, Changes),
        opt("f", "silent", HasArg::No, Quiet),
        opt("", "quiet", HasArg::No, Quiet),
        opt("v", "verbose", HasArg::No, Verbose),
        opt("", "no-preserve-root", HasArg::No, NoPreserveRoot),
        opt("", "preserve-root", HasArg::No, PreserveRoot),
        opt("", "reference", HasArg::Required, Reference),
        opt("R", "recursive", HasArg::No, Recursive),
    ];
    // Like GNU, accept modes that look like options (`chmod -w file`).
    let mut mode_pieces = Vec::new();
    let mut rest = vec![args.first().cloned().unwrap_or_default()];
    let mut ended = false;
    for a in args.iter().skip(1) {
        ended |= a == "--";
        let mode_like = !ended
            && a.len() > 1
            && a.starts_with('-')
            && a[1..2].chars().all(|c| "rwxXstugoa,+=01234567".contains(c));
        if mode_like {
            mode_pieces.push(a.clone());
        } else {
            rest.push(a.clone());
        }
    }
    let parsed = match parse_tool_args("chmod", &rest, SPECS, CHMOD_HELP) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let mut run = ChmodRun {
        changes: None,
        reference_mode: 0,
        umask: process_umask(),
        recursive: false,
        report_changes: false,
        verbose: false,
        quiet: false,
        diagnose_surprises: !mode_pieces.is_empty(),
    };
    let (mut preserve_root, mut reference) = (false, None);
    for (o, v) in parsed.opts {
        match o {
            Changes => run.report_changes = true,
            Quiet => run.quiet = true,
            Verbose => run.verbose = true,
            NoPreserveRoot => preserve_root = false,
            PreserveRoot => preserve_root = true,
            Reference => reference = v,
            Recursive => run.recursive = true,
        }
    }
    let mut files = parsed.operands;
    let mut mode = (!mode_pieces.is_empty()).then(|| mode_pieces.join(","));
    if reference.is_none() && mode.is_none() {
        if files.is_empty() {
            return Ok(usage_error("chmod", "missing operand"));
        }
        mode = Some(files.remove(0));
    }
    if files.is_empty() {
        return Ok(match (&mode, &reference) {
            (Some(m), None) => usage_error(
                "chmod",
                &format!("missing operand after {}", quote_locale(m)),
            ),
            _ => usage_error("chmod", "missing operand"),
        });
    }
    if let Some(r) = &reference {
        match fs::metadata(r) {
            Ok(m) => run.reference_mode = m.mode() & CHMOD_MODE_BITS,
            Err(e) => {
                eprintln!(
                    "chmod: failed to get attributes of {}: {}",
                    qa(r),
                    gnu_io_error(&e)
                );
                return Ok(1);
            }
        }
    } else if let Some(m) = &mode {
        match mode_compile(m) {
            Some(c) => run.changes = Some(c),
            None => {
                return Ok(usage_error(
                    "chmod",
                    &format!("invalid mode: {}", quote_locale(m)),
                ));
            }
        }
    }

    let mut ok = true;
    for f in &files {
        if run.recursive && preserve_root && is_root_dir(Path::new(f)) {
            refuse_root("chmod", f);
            ok = false;
            continue;
        }
        ok &= chmod_path(Path::new(f), true, &run, ctx);
    }
    Ok(if ok { 0 } else { 1 })
}

fn chmod_path(path: &Path, top_level: bool, run: &ChmodRun, ctx: &DogfishContext) -> bool {
    let lmeta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => {
            if !run.quiet {
                eprintln!("chmod: cannot access {}: {}", qa(path), gnu_io_error(&e));
            }
            if run.verbose {
                println!("{} could not be accessed", qa(path));
            }
            return false;
        }
    };
    let meta = if lmeta.is_symlink() {
        if !top_level {
            // During -R traversal, symbolic links are not followed or changed.
            if run.verbose {
                println!(
                    "neither symbolic link {} nor referent has been changed",
                    qa(path)
                );
            }
            return true;
        }
        match fs::metadata(path) {
            Ok(m) => m,
            Err(_) => {
                if !run.quiet {
                    eprintln!("chmod: cannot operate on dangling symlink {}", qa(path));
                }
                if run.verbose {
                    println!("{} could not be accessed", qa(path));
                }
                return false;
            }
        }
    } else {
        lmeta
    };

    let old = meta.mode() & CHMOD_MODE_BITS;
    let is_dir = meta.is_dir();
    let new = match &run.changes {
        Some(c) => mode_adjust(old, is_dir, run.umask, c).0,
        None => run.reference_mode,
    };
    let mut ok = true;
    let applied = match fs::set_permissions(path, fs::Permissions::from_mode(new)) {
        Ok(()) => {
            ctx.telemetry.add_items(1);
            true
        }
        Err(e) => {
            if !run.quiet {
                eprintln!(
                    "chmod: changing permissions of {}: {}",
                    qa(path),
                    gnu_io_error(&e)
                );
            }
            ok = false;
            false
        }
    };
    let changed = applied && old != new;
    if run.verbose || (run.report_changes && changed) {
        let (o, n) = (strmode9(old), strmode9(new));
        if !applied {
            println!(
                "failed to change mode of {} from {old:04o} ({o}) to {new:04o} ({n})",
                qa(path)
            );
        } else if changed {
            println!(
                "mode of {} changed from {old:04o} ({o}) to {new:04o} ({n})",
                qa(path)
            );
        } else {
            println!("mode of {} retained as {new:04o} ({n})", qa(path));
        }
    }
    if applied
        && run.diagnose_surprises
        && let Some(c) = &run.changes
    {
        let naive = mode_adjust(old, is_dir, 0, c).0;
        if new & !naive != 0 {
            eprintln!(
                "chmod: {}: new permissions are {}, not {}",
                qf(path),
                strmode9(new),
                strmode9(naive)
            );
            ok = false;
        }
    }

    if run.recursive && is_dir {
        match dir_names(path, false) {
            Ok(names) => {
                for name in names {
                    ok &= chmod_path(&path.join(name), false, run, ctx);
                }
            }
            Err(e) => {
                if !run.quiet {
                    eprintln!(
                        "chmod: cannot read directory {}: {}",
                        qa(path),
                        gnu_io_error(&e)
                    );
                }
                ok = false;
            }
        }
    }
    ok
}

// ---------------------------------------------------------------------------
// 5. chown — USER[:GROUP], :GROUP, USER:, numeric ids, -R/-h/-H/-L/-P, --from
// ---------------------------------------------------------------------------
const CHOWN_HELP: &str = "\
Usage: chown [OPTION]... [OWNER][:[GROUP]] FILE...
  or:  chown [OPTION]... --reference=RFILE FILE...
Change the owner and/or group of each FILE to OWNER and/or GROUP.
With --reference, change the owner and group of each FILE to those of RFILE.

  -c, --changes          like verbose but report only when a change is made
  -f, --silent, --quiet  suppress most error messages
  -v, --verbose          output a diagnostic for every file processed
      --dereference      affect the referent of each symbolic link (this is
                         the default), rather than the symbolic link itself
  -h, --no-dereference   affect symbolic links instead of any referenced file
      --from=CURRENT_OWNER:CURRENT_GROUP
                         change the owner and/or group of each file only if
                         its current owner and/or group match those specified
      --no-preserve-root  do not treat '/' specially (the default)
      --preserve-root    fail to operate recursively on '/'
      --reference=RFILE  use RFILE's owner and group rather than specifying
                         OWNER:GROUP values
  -R, --recursive        operate on files and directories recursively

The following options modify how a hierarchy is traversed when the -R
option is also specified.  If more than one is specified, only the final
one takes effect.

  -H                     if a command line argument is a symbolic link
                         to a directory, traverse it
  -L                     traverse every symbolic link to a directory
                         encountered
  -P                     do not traverse any symbolic links (default)

      --help        display this help and exit
      --version     output version information and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChownOpt {
    Changes,
    Quiet,
    Verbose,
    Deref,
    NoDeref,
    From,
    NoPreserveRoot,
    PreserveRoot,
    Reference,
    Recursive,
    TraverseCmdline,
    TraverseAll,
    TraverseNone,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Traverse {
    None,
    CommandLine,
    All,
}

/// A parsed `[OWNER][:[GROUP]]`: ids to set and the names to report.
#[derive(Default)]
struct OwnerSpec {
    uid: Option<u32>,
    gid: Option<u32>,
    user: Option<String>,
    group: Option<String>,
}

struct ChownRun {
    spec: OwnerSpec,
    from_uid: Option<u32>,
    from_gid: Option<u32>,
    recursive: bool,
    traverse: Traverse,
    affect_referent: bool,
    report_changes: bool,
    verbose: bool,
    quiet: bool,
}

pub fn run_chown(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    use ChownOpt::*;
    const SPECS: &[ToolOpt<ChownOpt>] = &[
        opt("c", "changes", HasArg::No, Changes),
        opt("f", "silent", HasArg::No, Quiet),
        opt("", "quiet", HasArg::No, Quiet),
        opt("v", "verbose", HasArg::No, Verbose),
        opt("", "dereference", HasArg::No, Deref),
        opt("h", "no-dereference", HasArg::No, NoDeref),
        opt("", "from", HasArg::Required, From),
        opt("", "no-preserve-root", HasArg::No, NoPreserveRoot),
        opt("", "preserve-root", HasArg::No, PreserveRoot),
        opt("", "reference", HasArg::Required, Reference),
        opt("R", "recursive", HasArg::No, Recursive),
        opt("H", "", HasArg::No, TraverseCmdline),
        opt("L", "", HasArg::No, TraverseAll),
        opt("P", "", HasArg::No, TraverseNone),
    ];
    let parsed = match parse_tool_args("chown", &args, SPECS, CHOWN_HELP) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let mut run = ChownRun {
        spec: OwnerSpec::default(),
        from_uid: None,
        from_gid: None,
        recursive: false,
        traverse: Traverse::None,
        affect_referent: true,
        report_changes: false,
        verbose: false,
        quiet: false,
    };
    let (mut deref, mut preserve_root, mut from, mut reference) = (None, false, None, None);
    for (o, v) in parsed.opts {
        match o {
            Changes => run.report_changes = true,
            Quiet => run.quiet = true,
            Verbose => run.verbose = true,
            Deref => deref = Some(true),
            NoDeref => deref = Some(false),
            From => from = v,
            NoPreserveRoot => preserve_root = false,
            PreserveRoot => preserve_root = true,
            Reference => reference = v,
            Recursive => run.recursive = true,
            TraverseCmdline => run.traverse = Traverse::CommandLine,
            TraverseAll => run.traverse = Traverse::All,
            TraverseNone => run.traverse = Traverse::None,
        }
    }
    if run.recursive && run.traverse == Traverse::None {
        if deref == Some(true) {
            eprintln!("chown: -R --dereference requires either -H or -L");
            return Ok(1);
        }
        deref = Some(false);
    }
    if !run.recursive {
        run.traverse = Traverse::None;
    }
    run.affect_referent = deref != Some(false);

    let mut files = parsed.operands;
    if let Some(r) = &reference {
        match fs::metadata(r) {
            Ok(m) => {
                run.spec = OwnerSpec {
                    uid: Some(m.uid()),
                    gid: Some(m.gid()),
                    user: Some(uid_label(m.uid())),
                    group: Some(gid_label(m.gid())),
                };
            }
            Err(e) => {
                eprintln!(
                    "chown: failed to get attributes of {}: {}",
                    qa(r),
                    gnu_io_error(&e)
                );
                return Ok(1);
            }
        }
    } else {
        if files.is_empty() {
            return Ok(usage_error("chown", "missing operand"));
        }
        let spec = files.remove(0);
        match parse_owner_spec(&spec) {
            Ok(s) => run.spec = s,
            Err(msg) => {
                eprintln!("chown: {msg}");
                return Ok(1);
            }
        }
        if files.is_empty() {
            return Ok(usage_error(
                "chown",
                &format!("missing operand after {}", quote_locale(&spec)),
            ));
        }
    }
    if let Some(f) = &from {
        match parse_owner_spec(f) {
            Ok(s) => {
                run.from_uid = s.uid;
                run.from_gid = s.gid;
            }
            Err(msg) => {
                eprintln!("chown: {msg}");
                return Ok(1);
            }
        }
    }

    let mut ok = true;
    for f in &files {
        if run.recursive && preserve_root && is_root_dir(Path::new(f)) {
            refuse_root("chown", f);
            ok = false;
            continue;
        }
        ok &= chown_path(Path::new(f), true, &run, ctx);
    }
    Ok(if ok { 0 } else { 1 })
}

fn uid_label(uid: u32) -> String {
    get_username_by_uid(uid).unwrap_or_else(|| uid.to_string())
}

fn gid_label(gid: u32) -> String {
    get_groupname_by_gid(gid).unwrap_or_else(|| gid.to_string())
}

/// `getpwnam`: (uid, login gid) of a user name.
fn lookup_user(name: &str) -> Option<(u32, u32)> {
    let c = CString::new(name).ok()?;
    let pw = unsafe { libc::getpwnam(c.as_ptr()) };
    (!pw.is_null()).then(|| unsafe { ((*pw).pw_uid, (*pw).pw_gid) })
}

fn lookup_group(name: &str) -> Option<u32> {
    let c = CString::new(name).ok()?;
    let gr = unsafe { libc::getgrnam(c.as_ptr()) };
    (!gr.is_null()).then(|| unsafe { (*gr).gr_gid })
}

/// A numeric id as GNU accepts it (`-1` is reserved).
fn parse_id(s: &str) -> Option<u32> {
    s.parse::<u32>().ok().filter(|&id| id != u32::MAX)
}

/// Parses `[OWNER][:[GROUP]]` (GNU `parse_user_spec`); `.` is accepted as the
/// separator, with a warning, when the whole spec is not a user name.
fn parse_owner_spec(spec: &str) -> std::result::Result<OwnerSpec, String> {
    if let Some(i) = spec.find(':') {
        return parse_owner_parts(spec, &spec[..i], Some(&spec[i + 1..]));
    }
    let whole = parse_owner_parts(spec, spec, None);
    if whole.is_err()
        && let Some(i) = spec.find('.')
        && let Ok(s) = parse_owner_parts(spec, &spec[..i], Some(&spec[i + 1..]))
    {
        eprintln!("chown: warning: '.' should be ':': {}", quote_locale(spec));
        return Ok(s);
    }
    whole
}

fn parse_owner_parts(
    spec: &str,
    user: &str,
    group: Option<&str>,
) -> std::result::Result<OwnerSpec, String> {
    let mut s = OwnerSpec::default();
    let invalid = |what: &str| format!("invalid {what}: {}", quote_locale(spec));
    if !user.is_empty() {
        match lookup_user(user) {
            Some((uid, login_gid)) => {
                s.uid = Some(uid);
                if group == Some("") {
                    s.gid = Some(login_gid);
                    s.group = Some(gid_label(login_gid));
                }
            }
            None => {
                s.uid = Some(parse_id(user).ok_or_else(|| invalid("user"))?);
                if group == Some("") {
                    return Err(invalid("user"));
                }
            }
        }
        s.user = Some(user.to_string());
    }
    if let Some(g) = group.filter(|g| !g.is_empty()) {
        let by_name = lookup_group(g);
        s.gid = Some(
            by_name
                .or_else(|| parse_id(g))
                .ok_or_else(|| invalid("group"))?,
        );
        s.group = Some(g.to_string());
        if by_name.is_some() && s.user.is_none() {
            // GNU reports `:GROUP` (by name) as an ownership change.
            s.user = Some(String::new());
        }
    }
    Ok(s)
}

fn user_group_str(user: Option<&str>, group: Option<&str>) -> Option<String> {
    match (user, group) {
        (Some(u), Some(g)) => Some(format!("{u}:{g}")),
        (Some(u), None) => Some(u.to_string()),
        (None, Some(g)) => Some(g.to_string()),
        (None, None) => None,
    }
}

fn chown_path(path: &Path, top_level: bool, run: &ChownRun, ctx: &DogfishContext) -> bool {
    let spec = &run.spec;
    let new_spec = user_group_str(spec.user.as_deref(), spec.group.as_deref());
    let describe_failure_without_old = || {
        if run.verbose {
            let what = if spec.user.is_some() {
                "ownership"
            } else {
                "group"
            };
            match &new_spec {
                Some(s) => println!("failed to change {what} of {} to {s}", qa(path)),
                None => println!("failed to change ownership of {}", qa(path)),
            }
        }
    };
    let lmeta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => {
            if !run.quiet {
                eprintln!("chown: cannot access {}: {}", qa(path), gnu_io_error(&e));
            }
            describe_failure_without_old();
            return false;
        }
    };
    let follow = lmeta.is_symlink() && run.affect_referent;
    let meta = if follow {
        match fs::metadata(path) {
            Ok(m) => m,
            Err(e) => {
                if !run.quiet {
                    eprintln!(
                        "chown: cannot dereference {}: {}",
                        qa(path),
                        gnu_io_error(&e)
                    );
                }
                describe_failure_without_old();
                return false;
            }
        }
    } else {
        lmeta.clone()
    };

    let traverse_dir = run.recursive
        && meta.is_dir()
        && (!lmeta.is_symlink()
            || run.traverse == Traverse::All
            || (top_level && run.traverse == Traverse::CommandLine));
    let mut ok = true;
    // Like GNU (fts post-order), a directory's contents are changed before it.
    if traverse_dir {
        match dir_names(path, false) {
            Ok(names) => {
                for name in names {
                    ok &= chown_path(&path.join(name), false, run, ctx);
                }
            }
            Err(e) => {
                if !run.quiet {
                    eprintln!(
                        "chown: cannot read directory {}: {}",
                        qa(path),
                        gnu_io_error(&e)
                    );
                }
                ok = false;
            }
        }
    }

    let (old_uid, old_gid) = (meta.uid(), meta.gid());
    let matches =
        run.from_uid.is_none_or(|u| u == old_uid) && run.from_gid.is_none_or(|g| g == old_gid);
    let old_spec = user_group_str(
        spec.user.as_ref().map(|_| uid_label(old_uid)).as_deref(),
        spec.group.as_ref().map(|_| gid_label(old_gid)).as_deref(),
    );
    let mut status_changed = false;
    let mut failed = false;
    if matches && (spec.uid.is_some() || spec.gid.is_some()) {
        let res = if follow || !lmeta.is_symlink() {
            std::os::unix::fs::chown(path, spec.uid, spec.gid)
        } else {
            std::os::unix::fs::lchown(path, spec.uid, spec.gid)
        };
        match res {
            Ok(()) => {
                ctx.telemetry.add_items(1);
                status_changed = spec.uid.is_some_and(|u| u != old_uid)
                    || spec.gid.is_some_and(|g| g != old_gid);
            }
            Err(e) => {
                failed = true;
                ok = false;
                if !run.quiet {
                    let what = if spec.uid.is_some() {
                        "ownership"
                    } else {
                        "group"
                    };
                    eprintln!(
                        "chown: changing {what} of {}: {}",
                        qa(path),
                        gnu_io_error(&e)
                    );
                }
            }
        }
    }
    if run.verbose || (run.report_changes && status_changed) {
        let what = if spec.user.is_some() {
            "ownership"
        } else if spec.group.is_some() {
            "group"
        } else {
            ""
        };
        let file = qa(path);
        let (old_s, new_s) = (old_spec.unwrap_or_default(), new_spec.unwrap_or_default());
        if failed {
            if what.is_empty() {
                println!("failed to change ownership of {file}");
            } else {
                println!("failed to change {what} of {file} from {old_s} to {new_s}");
            }
        } else if status_changed {
            println!("changed {what} of {file} from {old_s} to {new_s}");
        } else if what.is_empty() {
            println!("ownership of {file} retained");
        } else {
            println!("{what} of {file} retained as {old_s}");
        }
    }
    ok
}

// ---------------------------------------------------------------------------
// 6. stat — Visual inspection card on a TTY, GNU `stat` output otherwise
// ---------------------------------------------------------------------------
const STAT_OPTIONS: &[OptSpec] = &[
    OptSpec::new("dereference", Some('L'), Some("dereference"), OptArg::No),
    OptSpec::new("file-system", Some('f'), Some("file-system"), OptArg::No),
    OptSpec::new("cached", None, Some("cached"), OptArg::Required),
    OptSpec::new("format", Some('c'), Some("format"), OptArg::Required),
    OptSpec::new("printf", None, Some("printf"), OptArg::Required),
    OptSpec::new("terse", Some('t'), Some("terse"), OptArg::No),
    OptSpec::new("json", Some('J'), Some("json"), OptArg::No),
    OptSpec::new("help", None, Some("help"), OptArg::No),
    OptSpec::new("version", None, Some("version"), OptArg::No),
];

const STAT_HELP: &str = "\
Usage: stat [OPTION]... FILE...
Display file or file system status.

  -L, --dereference     follow links
  -f, --file-system     display file system status instead of file status
      --cached=MODE     accepted for compatibility (always, never, default)
  -c  --format=FORMAT   use the specified FORMAT instead of the default;
                          output a newline after each use of FORMAT
      --printf=FORMAT   like --format, but interpret backslash escapes,
                          and do not output a mandatory trailing newline
  -t, --terse           print the information in terse form
  -J, --json            emit NDJSON records (Dogfish extension)
      --help            display this help and exit
      --version         output version information and exit

On a terminal (without -c/--printf/-t/-f) Dogfish shows a visual card; otherwise
the output is identical to GNU stat. FORMAT sequences are those of GNU stat
(%a %A %b %B %C %d %D %f %F %g %G %h %i %m %n %N %o %s %t %T %u %U %w %W %x %X
%y %Y %z %Z, and with -f: %a %b %c %d %f %i %l %n %s %S %t %T).
";

/// GNU's default `stat` format (`default_format` in stat.c).
fn stat_default_format(terse: bool, device: bool, selinux: bool) -> String {
    if terse {
        let ctx = if selinux { " %C" } else { "" };
        return format!("%n %s %b %f %u %g %D %i %h %t %T %X %Y %Z %W %o{ctx}\n");
    }
    let mut f = String::from("  File: %N\n  Size: %-10s\tBlocks: %-10b IO Block: %-6o %F\n");
    if device {
        f.push_str("Device: %Hd,%Ld\tInode: %-11i Links: %-5h Device type: %Hr,%Lr\n");
    } else {
        f.push_str("Device: %Hd,%Ld\tInode: %-11i Links: %h\n");
    }
    f.push_str("Access: (%04a/%10.10A)  Uid: (%5u/%8U)   Gid: (%5g/%8G)\n");
    if selinux {
        f.push_str("Context: %C\n");
    }
    f.push_str("Access: %x\nModify: %y\nChange: %z\n Birth: %w\n");
    f
}

const STAT_FS_FORMAT: &str = "  File: \"%n\"\n    ID: %-8i Namelen: %-7l Type: %T\n\
Block size: %-10s Fundamental block size: %S\n\
Blocks: Total: %-10b Free: %-10f Available: %a\n\
Inodes: Total: %-10c Free: %d\n";
const STAT_FS_TERSE_FORMAT: &str = "%n %i %l %t %s %S %b %f %a %c %d\n";

#[derive(Serialize)]
struct StatRecord {
    name: String,
    file_type: String,
    size: u64,
    size_human: String,
    blocks: u64,
    io_block: u64,
    device: u64,
    inode: u64,
    links: u64,
    mode_octal: String,
    mode_symbolic: String,
    uid: u32,
    user: String,
    gid: u32,
    group: String,
    accessed: String,
    modified: String,
    changed: String,
    modified_relative: String,
    symlink_target: Option<String>,
}

pub fn run_stat(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = match gnu_getopt(STAT_OPTIONS, &args[1..]) {
        Ok(p) => p,
        Err(msg) => return Ok(gnu_usage_error("stat", &msg, 1)),
    };
    let mut dereference = false;
    let mut file_system = false;
    let mut terse = false;
    let mut json = false;
    // (FORMAT, interpret backslash escapes and omit the trailing newline)
    let mut format: Option<(String, bool)> = None;
    for opt in parsed.opts {
        match opt.key {
            "dereference" => dereference = true,
            "file-system" => file_system = true,
            "terse" => terse = true,
            "json" => json = true,
            "format" => format = Some((opt.value.unwrap_or_default(), false)),
            "printf" => format = Some((opt.value.unwrap_or_default(), true)),
            "cached" => {
                let choices = [
                    ("default", "default"),
                    ("never", "never"),
                    ("always", "always"),
                ];
                let value = opt.value.unwrap_or_default();
                if gnu_argmatch("stat", "--cached", &value, &choices).is_none() {
                    return Ok(try_help("stat", 1));
                }
            }
            "help" => return print_help_text(STAT_HELP),
            "version" => {
                print_tool_version("stat");
                return Ok(0);
            }
            _ => unreachable!("unhandled stat option"),
        }
    }
    if parsed.operands.is_empty() {
        return Ok(gnu_usage_error("stat", "missing operand", 1));
    }
    if json || (ctx.stdout_is_tty && format.is_none() && !terse && !file_system) {
        return stat_card(&parsed.operands, dereference, json, ctx);
    }

    let selinux = selinux_enabled();
    // `%N` in a user format honors QUOTING_STYLE (default shell-escape-always);
    // the default format prints names literally.
    let mut name_quoting = Quoting::Literal;
    if format.as_ref().is_some_and(|(f, _)| f.contains("%N")) {
        name_quoting = Quoting::ShellEscapeAlways;
        if let Ok(style) = std::env::var("QUOTING_STYLE") {
            match Quoting::from_name(&style) {
                Some(q) => name_quoting = q,
                None => eprintln!(
                    "stat: ignoring invalid value of environment variable QUOTING_STYLE: {}",
                    quote_bytes(style.as_bytes(), Quoting::Locale)
                ),
            }
        }
    }
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    let mut status = 0;
    for name in &parsed.operands {
        // `-` means standard input, as in GNU.
        let path = if name == "-" {
            Path::new("/dev/stdin")
        } else {
            Path::new(name)
        };
        let (fmt, escapes) = match &format {
            Some((f, printf)) => (f.clone(), *printf),
            None => (String::new(), false),
        };
        let mut buf = Vec::new();
        let expanded = if file_system {
            let info = match FsStats::query(path) {
                Ok(info) => info,
                Err(e) => {
                    eprintln!(
                        "stat: cannot read file system information for {}: {}",
                        quote_af(name),
                        gnu_io_error(&e)
                    );
                    status = 1;
                    continue;
                }
            };
            let fmt = if format.is_some() {
                fmt
            } else if terse {
                STAT_FS_TERSE_FORMAT.to_string()
            } else {
                STAT_FS_FORMAT.to_string()
            };
            expand_stat_format(&mut buf, &fmt, escapes, |d| info.directive(name, d))
        } else {
            let meta = if dereference || name == "-" {
                fs::metadata(path)
            } else {
                fs::symlink_metadata(path)
            };
            let meta = match meta {
                Ok(m) => m,
                Err(e) => {
                    eprintln!(
                        "stat: cannot statx {}: {}",
                        quote_af(name),
                        gnu_io_error(&e)
                    );
                    status = 1;
                    continue;
                }
            };
            let fmt = if format.is_some() {
                fmt
            } else {
                use std::os::unix::fs::FileTypeExt;
                let ft = meta.file_type();
                let device = ft.is_char_device() || ft.is_block_device();
                stat_default_format(terse, device, selinux)
            };
            let file = StatFile {
                name,
                path,
                meta: &meta,
                quoting: name_quoting,
            };
            expand_stat_format(&mut buf, &fmt, escapes, |d| file.directive(d))
        };
        if let Err(msg) = expanded {
            // GNU dies here, after writing what it expanded so far.
            ignore_broken_pipe(out.write_all(&buf))?;
            ignore_broken_pipe(out.flush())?;
            eprintln!("stat: {msg}");
            return Ok(1);
        }
        if format.as_ref().is_some_and(|(_, printf)| !printf) {
            buf.push(b'\n');
        }
        ignore_broken_pipe(out.write_all(&buf))?;
    }
    ignore_broken_pipe(out.flush())?;
    Ok(status)
}

/// One parsed `%` directive of a `stat` format: `%[flags][width][.precision][H|L]conv`.
struct StatDirective<'a> {
    flags: &'a str,
    width: Option<usize>,
    precision: Option<usize>,
    /// A `.` with no digits after it.
    bare_precision: bool,
    /// `H` (major) or `L` (minor) before `d`/`r`.
    modifier: Option<char>,
    conv: char,
}

/// A value produced by a directive; rendered with the C `printf` rules GNU uses.
enum StatValue {
    Str(Vec<u8>),
    Signed(i64),
    Unsigned(u64),
    Octal(u64),
    Hex(u64),
    /// Seconds since the epoch, with an optional `.precision` fraction.
    Epoch(Timespec),
}

/// Expands a `stat` FORMAT into `out`. `lookup` returns `None` for an unknown
/// conversion, which GNU prints as `?`. `Err` is GNU's fatal "invalid
/// directive" diagnostic; `out` then holds the output up to that point.
fn expand_stat_format(
    out: &mut Vec<u8>,
    fmt: &str,
    escapes: bool,
    mut lookup: impl FnMut(&StatDirective) -> Option<StatValue>,
) -> std::result::Result<(), String> {
    let b = fmt.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let start = i + 1;
                let mut j = start;
                while j < b.len() && b"'-+ #0I".contains(&b[j]) {
                    j += 1;
                }
                let flags = &fmt[start..j];
                let w_start = j;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                let width = fmt[w_start..j].parse().ok();
                let mut precision = None;
                let mut bare_precision = false;
                if j < b.len() && b[j] == b'.' {
                    j += 1;
                    let p_start = j;
                    while j < b.len() && b[j].is_ascii_digit() {
                        j += 1;
                    }
                    bare_precision = p_start == j;
                    precision = Some(fmt[p_start..j].parse().unwrap_or(0));
                }
                let spec = &fmt[start..j];
                if j >= b.len() || b[j] == b'%' {
                    if !spec.is_empty() {
                        let directive = format!("%{spec}{}", if j < b.len() { "%" } else { "" });
                        return Err(format!(
                            "{}: invalid directive",
                            quote_bytes(directive.as_bytes(), Quoting::Locale)
                        ));
                    }
                    out.push(b'%');
                    i = j + 1;
                    continue;
                }
                let mut conv = fmt[j..].chars().next().unwrap_or('?');
                let mut modifier = None;
                if (conv == 'H' || conv == 'L') && matches!(b.get(j + 1), Some(b'd') | Some(b'r')) {
                    modifier = Some(conv);
                    j += 1;
                    conv = b[j] as char;
                }
                let directive = StatDirective {
                    flags,
                    width,
                    precision,
                    bare_precision,
                    modifier,
                    conv,
                };
                match lookup(&directive) {
                    Some(value) => render_stat_value(out, &directive, value),
                    None => out.push(b'?'),
                }
                i = j + conv.len_utf8();
            }
            b'\\' if escapes => {
                i += 1;
                if i >= b.len() {
                    eprintln!("stat: warning: backslash at end of format");
                    out.push(b'\\');
                    break;
                }
                let c = b[i];
                if (b'0'..=b'7').contains(&c) {
                    let mut value = u32::from(c - b'0');
                    let mut n = 1;
                    while n < 3 && i + 1 < b.len() && (b'0'..=b'7').contains(&b[i + 1]) {
                        i += 1;
                        value = value * 8 + u32::from(b[i] - b'0');
                        n += 1;
                    }
                    out.push(value as u8);
                } else if c == b'x' && b.get(i + 1).is_some_and(|h| h.is_ascii_hexdigit()) {
                    let mut value = (b[i + 1] as char).to_digit(16).unwrap_or(0);
                    i += 1;
                    if let Some(d) = b.get(i + 1).and_then(|h| (*h as char).to_digit(16)) {
                        value = value * 16 + d;
                        i += 1;
                    }
                    out.push(value as u8);
                } else {
                    let esc = match c {
                        b'a' => 0x07,
                        b'b' => 0x08,
                        b'e' => 0x1b,
                        b'f' => 0x0c,
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        b'v' => 0x0b,
                        b'"' | b'\\' => c,
                        _ => {
                            eprintln!("stat: warning: unrecognized escape '\\{}'", c as char);
                            c
                        }
                    };
                    out.push(esc);
                }
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(())
}

/// Renders `value` like `printf` with the flags GNU's `make_format` keeps for it.
fn render_stat_value(out: &mut Vec<u8>, d: &StatDirective, value: StatValue) {
    let left = d.flags.contains('-');
    let pad = |out: &mut Vec<u8>, body: &[u8], zero_ok: bool, sign_len: usize| {
        let width = d.width.unwrap_or(0);
        if body.len() >= width {
            out.extend_from_slice(body);
        } else if left {
            out.extend_from_slice(body);
            out.resize(out.len() + width - body.len(), b' ');
        } else if zero_ok && d.flags.contains('0') && d.precision.is_none() {
            out.extend_from_slice(&body[..sign_len]);
            out.resize(out.len() + width - body.len(), b'0');
            out.extend_from_slice(&body[sign_len..]);
        } else {
            out.resize(out.len() + width - body.len(), b' ');
            out.extend_from_slice(body);
        }
    };
    let integer =
        |negative: bool, digits: String, prefix: &str, signed: bool| -> (Vec<u8>, usize) {
            let mut digits = digits;
            if let Some(p) = d.precision {
                if p == 0 && digits == "0" {
                    digits.clear();
                } else if digits.len() < p {
                    digits = format!("{}{digits}", "0".repeat(p - digits.len()));
                }
            }
            let sign = if negative {
                "-"
            } else if signed && d.flags.contains('+') {
                "+"
            } else if signed && d.flags.contains(' ') {
                " "
            } else {
                ""
            };
            let head = format!("{sign}{prefix}");
            (format!("{head}{digits}").into_bytes(), head.len())
        };
    match value {
        StatValue::Str(s) => {
            let s = match d.precision {
                Some(p) if p < s.len() => &s[..p],
                _ => &s[..],
            };
            pad(out, s, false, 0);
        }
        StatValue::Signed(v) => {
            let (body, sign_len) = integer(v < 0, v.unsigned_abs().to_string(), "", true);
            pad(out, &body, true, sign_len);
        }
        StatValue::Unsigned(v) => {
            let (body, sign_len) = integer(false, v.to_string(), "", false);
            pad(out, &body, true, sign_len);
        }
        StatValue::Octal(v) => {
            let digits = format!("{v:o}");
            let alt = d.flags.contains('#') && !digits.starts_with('0');
            let digits = if alt { format!("0{digits}") } else { digits };
            let (body, sign_len) = integer(false, digits, "", false);
            pad(out, &body, true, sign_len);
        }
        StatValue::Hex(v) => {
            let prefix = if d.flags.contains('#') && v != 0 {
                "0x"
            } else {
                ""
            };
            let (body, sign_len) = integer(false, format!("{v:x}"), prefix, false);
            pad(out, &body, true, sign_len);
        }
        StatValue::Epoch(t) => render_epoch(out, d, t),
    }
}

/// GNU `out_epoch_sec`: `%X`-style seconds, `printf("%f")`-like with `.precision`.
fn render_epoch(out: &mut Vec<u8>, d: &StatDirective, t: Timespec) {
    // `%.X` without digits means nanoseconds; `%.0X` has no fraction at all.
    let precision = match d.precision {
        Some(_) if d.bare_precision => 9,
        Some(p) => p,
        None => 0,
    };
    if precision == 0 {
        let whole = StatDirective {
            precision: None,
            ..*d
        };
        render_stat_value(out, &whole, StatValue::Signed(t.sec));
        return;
    }
    // A field width covers the whole number: GNU narrows the integer part by
    // the fraction, and a `-` flag moves the padding after the fraction.
    let width = d.width.unwrap_or(0);
    let mut flags = d.flags.to_string();
    let mut int_width = d.width;
    if width > 1 {
        int_width = None;
        let w = width as i64 - 1 - precision as i64;
        if width - 1 > 1 && w > 1 {
            if flags.contains('-') {
                flags.retain(|c| c != '-');
            } else {
                int_width = Some(w as usize);
            }
        }
    }
    let digits = precision.min(9);
    let divisor = 10_i64.pow(9 - digits as u32);
    let mut sec = t.sec;
    let mut frac = t.nsec / divisor;
    if sec < 0 && t.nsec != 0 {
        frac = 1_000_000_000 / divisor - frac - i64::from(t.nsec % divisor != 0);
        sec += i64::from(frac != 0);
    }
    let start = out.len();
    let int_part = StatDirective {
        flags: &flags,
        width: int_width,
        precision: None,
        ..*d
    };
    render_stat_value(out, &int_part, StatValue::Signed(sec));
    let int_len = out.len() - start;
    out.extend_from_slice(format!(".{frac:0digits$}").as_bytes());
    // GNU then prints printf("%-*.*d", trailing_width, precision - 9, 0).
    let trailing_width = if int_len < width && 1 < width - int_len {
        (width - int_len - 1) as i64 - digits as i64
    } else {
        0
    };
    let trailing = "0".repeat(precision - digits);
    out.extend_from_slice(trailing.as_bytes());
    let pad = (trailing_width.unsigned_abs() as usize).saturating_sub(trailing.len());
    out.resize(out.len() + pad, b' ');
}

/// GNU `stat`'s `human_time`: `2024-01-31 12:34:56.123456789 +0100`.
fn stat_human_time(t: Timespec) -> Vec<u8> {
    format_local_time("%Y-%m-%d %H:%M:%S.%N %z", t)
        .unwrap_or_else(|| t.sec.to_string())
        .into_bytes()
}

/// A file being described by `stat` (without `-f`).
struct StatFile<'a> {
    name: &'a str,
    path: &'a Path,
    meta: &'a fs::Metadata,
    /// How `%N` quotes names.
    quoting: Quoting,
}

impl StatFile<'_> {
    fn directive(&self, d: &StatDirective) -> Option<StatValue> {
        let m = self.meta;
        Some(match (d.conv, d.modifier) {
            ('d', Some('H')) => StatValue::Unsigned(dev_major(m.dev())),
            ('d', Some(_)) => StatValue::Unsigned(dev_minor(m.dev())),
            ('r', Some('H')) => StatValue::Unsigned(dev_major(m.rdev())),
            ('r', Some(_)) => StatValue::Unsigned(dev_minor(m.rdev())),
            ('n', _) => StatValue::Str(self.name.as_bytes().to_vec()),
            ('N', _) => {
                let style = self.quoting;
                let mut s = quote_bytes_raw(self.name.as_bytes(), style, b"");
                if m.file_type().is_symlink() {
                    let target = fs::read_link(self.path).ok()?;
                    s.extend_from_slice(b" -> ");
                    s.extend(quote_bytes_raw(path_bytes(&target), style, b""));
                }
                StatValue::Str(s)
            }
            ('a', _) => StatValue::Octal(u64::from(m.mode() & 0o7777)),
            ('A', _) => StatValue::Str(format_mode_rwx(m.mode()).into_bytes()),
            ('b', _) => StatValue::Unsigned(m.blocks()),
            ('B', _) => StatValue::Unsigned(512),
            ('C', _) => StatValue::Str(
                security_context(self.path)
                    .unwrap_or_else(|| "?".into())
                    .into_bytes(),
            ),
            ('d', _) => StatValue::Unsigned(m.dev()),
            ('D', _) => StatValue::Hex(m.dev()),
            ('f', _) => StatValue::Hex(u64::from(m.mode())),
            ('F', _) => StatValue::Str(file_type_description(m).as_bytes().to_vec()),
            ('g', _) => StatValue::Unsigned(u64::from(m.gid())),
            ('G', _) => StatValue::Str(
                group_name(m.gid())
                    .unwrap_or_else(|| "UNKNOWN".into())
                    .into_bytes(),
            ),
            ('h', _) => StatValue::Unsigned(m.nlink()),
            ('i', _) => StatValue::Unsigned(m.ino()),
            ('m', _) => StatValue::Str(path_bytes(&find_mount_point(self.path, m)).to_vec()),
            ('o', _) => StatValue::Unsigned(m.blksize()),
            ('r', _) => StatValue::Unsigned(m.rdev()),
            ('R', _) => StatValue::Hex(m.rdev()),
            ('s', _) => StatValue::Unsigned(m.size()),
            ('t', _) => StatValue::Hex(dev_major(m.rdev())),
            ('T', _) => StatValue::Hex(dev_minor(m.rdev())),
            ('u', _) => StatValue::Unsigned(u64::from(m.uid())),
            ('U', _) => StatValue::Str(
                user_name(m.uid())
                    .unwrap_or_else(|| "UNKNOWN".into())
                    .into_bytes(),
            ),
            ('w', _) => StatValue::Str(match meta_birth(m) {
                Some(t) => stat_human_time(t),
                None => b"-".to_vec(),
            }),
            ('W', _) => StatValue::Epoch(meta_birth(m).unwrap_or_default()),
            ('x', _) => StatValue::Str(stat_human_time(meta_atime(m))),
            ('X', _) => StatValue::Epoch(meta_atime(m)),
            ('y', _) => StatValue::Str(stat_human_time(meta_mtime(m))),
            ('Y', _) => StatValue::Epoch(meta_mtime(m)),
            ('z', _) => StatValue::Str(stat_human_time(meta_ctime(m))),
            ('Z', _) => StatValue::Epoch(meta_ctime(m)),
            _ => return None,
        })
    }
}

/// coreutils `find_mount_point`: the deepest ancestor of `path` (itself, if a
/// directory) on the same device.
fn find_mount_point(path: &Path, meta: &fs::Metadata) -> PathBuf {
    let start = if meta.is_dir() {
        fs::canonicalize(path)
    } else {
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        fs::canonicalize(parent)
    };
    let Ok(mut current) = start else {
        return PathBuf::from("?");
    };
    let dev = if meta.is_dir() {
        meta.dev()
    } else {
        match fs::metadata(&current) {
            Ok(m) => m.dev(),
            Err(_) => return current,
        }
    };
    while let Some(parent) = current.parent() {
        match fs::metadata(parent) {
            Ok(pm) if pm.dev() == dev => current = parent.to_path_buf(),
            _ => break,
        }
    }
    current
}

/// File system statistics for `stat -f` (from `statfs` on Linux, `statvfs` elsewhere).
struct FsStats {
    block_size: u64,
    fragment_size: u64,
    blocks: u64,
    free: u64,
    available: u64,
    files: u64,
    free_files: u64,
    fsid: u64,
    name_max: u64,
    fs_type: u64,
    type_name: String,
}

impl FsStats {
    #[cfg(target_os = "linux")]
    fn query(path: &Path) -> io::Result<Self> {
        let c_path = CString::new(path_bytes(path)).map_err(io::Error::other)?;
        // SAFETY: statfs fills the zeroed struct for a valid C path.
        let mut s: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c_path.as_ptr(), &mut s) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fsid_t is two 32-bit words; read them as GNU stat does.
        let words: [u32; 2] = unsafe { std::mem::transmute(s.f_fsid) };
        let fs_type = s.f_type as u64;
        Ok(Self {
            block_size: s.f_bsize as u64,
            fragment_size: if s.f_frsize != 0 {
                s.f_frsize as u64
            } else {
                s.f_bsize as u64
            },
            blocks: s.f_blocks,
            free: s.f_bfree,
            available: s.f_bavail,
            files: s.f_files,
            free_files: s.f_ffree,
            fsid: (u64::from(words[0]) << 32) | u64::from(words[1]),
            name_max: s.f_namelen as u64,
            fs_type,
            type_name: linux_fs_type_name(fs_type),
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn query(path: &Path) -> io::Result<Self> {
        let c_path = CString::new(path_bytes(path)).map_err(io::Error::other)?;
        // SAFETY: statvfs fills the zeroed struct for a valid C path.
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c_path.as_ptr(), &mut s) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let type_name = statfs_type_name(&c_path).unwrap_or_else(|| "unknown".into());
        Ok(Self {
            block_size: s.f_bsize as u64,
            fragment_size: s.f_frsize as u64,
            blocks: s.f_blocks as u64,
            free: s.f_bfree as u64,
            available: s.f_bavail as u64,
            files: s.f_files as u64,
            free_files: s.f_ffree as u64,
            fsid: s.f_fsid as u64,
            name_max: s.f_namemax as u64,
            fs_type: 0,
            type_name,
        })
    }

    fn directive(&self, name: &str, d: &StatDirective) -> Option<StatValue> {
        if d.modifier.is_some() {
            return None;
        }
        Some(match d.conv {
            'n' => StatValue::Str(name.as_bytes().to_vec()),
            'i' => StatValue::Hex(self.fsid),
            'l' => StatValue::Unsigned(self.name_max),
            't' => StatValue::Hex(self.fs_type),
            'T' => StatValue::Str(self.type_name.clone().into_bytes()),
            'b' => StatValue::Signed(self.blocks as i64),
            'f' => StatValue::Signed(self.free as i64),
            'a' => StatValue::Signed(self.available as i64),
            's' => StatValue::Unsigned(self.block_size),
            'S' => StatValue::Unsigned(self.fragment_size),
            'c' => StatValue::Unsigned(self.files),
            'd' => StatValue::Signed(self.free_files as i64),
            _ => return None,
        })
    }
}

/// The file system type name from `statfs` (BSD/macOS `f_fstypename`).
#[cfg(target_os = "macos")]
fn statfs_type_name(c_path: &CString) -> Option<String> {
    // SAFETY: statfs fills the zeroed struct for a valid C path.
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut s) } != 0 {
        return None;
    }
    // SAFETY: f_fstypename is a NUL-terminated C string.
    let name = unsafe { CStr::from_ptr(s.f_fstypename.as_ptr()) };
    Some(name.to_string_lossy().into_owned())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn statfs_type_name(_c_path: &CString) -> Option<String> {
    None
}

/// gnulib `human_fstype`: the name GNU `stat -f` prints for a Linux `f_type`.
#[cfg(target_os = "linux")]
fn linux_fs_type_name(magic: u64) -> String {
    let name = match magic {
        0xADFF => "affs",
        0x5346414F => "afs",
        0x09041934 => "anon-inode FS",
        0x61756673 => "aufs",
        0x0187 => "autofs",
        0x42465331 => "befs",
        0x62646576 => "bdevfs",
        0x1BADFACE => "bfs",
        0x42494E4D => "binfmt_misc",
        0xCAFE4A11 => "bpf_fs",
        0x9123683E => "btrfs",
        0x73727279 => "btrfs_test",
        0x00C36400 => "ceph",
        0x0027E0EB => "cgroupfs",
        0x63677270 => "cgroup2fs",
        0xFF534D42 => "cifs",
        0x73757245 => "coda",
        0x012FF7B7 => "coh",
        0x62656570 => "configfs",
        0x28CD3D45 => "cramfs",
        0x453DCD28 => "cramfs-wend",
        0x64626720 => "debugfs",
        0x1373 => "devfs",
        0x454D444D => "devmem",
        0x1CD1 => "devpts",
        0xF15F => "ecryptfs",
        0xDE5E81E4 => "efivarfs",
        0x00414A53 => "efs",
        0xE0F5E1E2 => "erofs",
        0x2011BAB0 => "exfat",
        0x45584653 => "exfs",
        0x5DF5 => "exofs",
        0x137D => "ext",
        0xEF53 => "ext2/ext3",
        0xEF51 => "ext2",
        0xF2F52010 => "f2fs",
        0x4006 => "fat",
        0x19830326 => "fhgfs",
        0x65735546 => "fuseblk",
        0x65735543 => "fusectl",
        0x0BAD1DEA => "futexfs",
        0x01161970 => "gfs/gfs2",
        0x47504653 => "gpfs",
        0x4244 => "hfs",
        0x482B => "hfs+",
        0x4858 => "hfsx",
        0x00C0FFEE => "hostfs",
        0xF995E849 => "hpfs",
        0x958458F6 => "hugetlbfs",
        0x11307854 => "inodefs",
        0x013111A8 => "ibrix",
        0x2BAD1DEA => "inotifyfs",
        0x9660 => "isofs",
        0x4004 => "isofs",
        0x4000 => "isofs",
        0x07C0 => "jffs",
        0x72B6 => "jffs2",
        0x3153464A => "jfs",
        0x6B414653 => "k-afs",
        0xC97E8168 => "logfs",
        0x0BD00BD0 => "lustre",
        0x5346314D => "m1fs",
        0x137F => "minix",
        0x138F => "minix (30 char.)",
        0x2468 => "minix v2",
        0x2478 => "minix v2 (30 char.)",
        0x4D5A => "minix3",
        0x19800202 => "mqueue",
        0x4D44 => "msdos",
        0x564C => "novell",
        0x6969 => "nfs",
        0x6E667364 => "nfsd",
        0x3434 => "nilfs",
        0x6E736673 => "nsfs",
        0x5346544E => "ntfs",
        0x9FA1 => "openprom",
        0x7461636F => "ocfs2",
        0x794C7630 => "overlayfs",
        0xAAD7AAEA => "panfs",
        0x50495045 => "pipefs",
        0xC7571590 => "ppc-cmm-fs",
        0x9FA0 => "proc",
        0x6165676C => "pstorefs",
        0x002F => "qnx4",
        0x68191122 => "qnx6",
        0x858458F6 => "ramfs",
        0x07655821 => "rdt",
        0x52654973 => "reiserfs",
        0x7275 => "romfs",
        0x67596969 => "rpc_pipefs",
        0x5DCA2DF5 => "sdcardfs",
        0x73636673 => "securityfs",
        0xF97CFF8C => "selinux",
        0x43415D53 => "smackfs",
        0x517B => "smb",
        0xFE534D42 => "smb2",
        0xBEEFDEAD => "snfs",
        0x534F434B => "sockfs",
        0x73717368 => "squashfs",
        0x62656572 => "sysfs",
        0x012FF7B6 => "sysv2",
        0x012FF7B5 => "sysv4",
        0x01021994 => "tmpfs",
        0x74726163 => "tracefs",
        0x24051905 => "ubifs",
        0x15013346 => "udf",
        0x00011954 => "ufs",
        0x54190100 => "ufs",
        0x9FA2 => "usbdevfs",
        0x01021997 => "v9fs",
        0xBACBACBC => "vmhgfs",
        0xA501FCF5 => "vxfs",
        0x565A4653 => "vzfs",
        0x53464846 => "wslfs",
        0xABBA1974 => "xenfs",
        0x012FF7B4 => "xenix",
        0x58465342 => "xfs",
        0x012FD16D => "xia",
        0x2FC12FC1 => "zfs",
        0x58295829 => "zsmallocfs",
        0x5A3C69F0 => "zonefs",
        _ => return format!("UNKNOWN (0x{magic:x})"),
    };
    name.to_string()
}

/// The Dogfish visual card (TTY) and `--json` records.
fn stat_card(files: &[String], dereference: bool, json: bool, ctx: &DogfishContext) -> Result<i32> {
    let mut records = Vec::with_capacity(files.len());
    let mut exit_code = 0;

    for f in files {
        let p = Path::new(f);
        let meta_res = if dereference {
            fs::metadata(p)
        } else {
            fs::symlink_metadata(p)
        };
        let meta = match meta_res {
            Ok(m) => m,
            Err(e) => {
                eprintln!("stat: cannot statx {}: {}", quote_af(f), gnu_io_error(&e));
                exit_code = 1;
                continue;
            }
        };

        let mode = meta.mode();
        let rwx = format_mode_rwx(mode);
        let ftype = if meta.file_type().is_dir() {
            "directory"
        } else if meta.file_type().is_symlink() {
            "symbolic link"
        } else if meta.file_type().is_file() {
            "regular file"
        } else {
            "special file"
        };

        let uid = meta.uid();
        let gid = meta.gid();
        let user = get_username_by_uid(uid).unwrap_or_else(|| uid.to_string());
        let group = get_groupname_by_gid(gid).unwrap_or_else(|| gid.to_string());
        let mtime_sys = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let atime_sys = meta.accessed().unwrap_or(SystemTime::UNIX_EPOCH);
        let ctime_dt: DateTime<Local> = DateTime::from_timestamp(meta.ctime(), 0)
            .unwrap_or_default()
            .with_timezone(&Local);
        let mtime_dt: DateTime<Local> = mtime_sys.into();
        let atime_dt: DateTime<Local> = atime_sys.into();

        let symlink_target = if meta.file_type().is_symlink() {
            fs::read_link(p)
                .ok()
                .map(|t| t.to_string_lossy().into_owned())
        } else {
            None
        };

        let rec = StatRecord {
            name: f.clone(),
            file_type: ftype.to_string(),
            size: meta.len(),
            size_human: format_size(meta.len(), true),
            blocks: meta.blocks(),
            io_block: meta.blksize(),
            device: meta.dev(),
            inode: meta.ino(),
            links: meta.nlink(),
            mode_octal: format!("{:04o}", mode & 0o7777),
            mode_symbolic: rwx,
            uid,
            user,
            gid,
            group,
            accessed: atime_dt.format("%Y-%m-%d %H:%M:%S %z").to_string(),
            modified: mtime_dt.format("%Y-%m-%d %H:%M:%S %z").to_string(),
            changed: ctime_dt.format("%Y-%m-%d %H:%M:%S %z").to_string(),
            modified_relative: format_relative_time(mtime_sys),
            symlink_target,
        };
        records.push(rec);
        ctx.telemetry.add_items(1);
    }

    if json {
        emit_objects_or_table(&records, ctx, true, false)?;
        return Ok(exit_code);
    }

    let st = ctx.style();
    for rec in &records {
        let header_target = match &rec.symlink_target {
            Some(t) => format!("{} -> {}", st.bold_cyan(&rec.name), st.magenta(t)),
            None => st.bold_cyan(&rec.name),
        };
        println!(
            "  {} {}  {}",
            st.dim("File:"),
            header_target,
            st.dim(&format!("({})", rec.file_type))
        );
        println!(
            "  {} {} ({} B)   {} {}   {} {}",
            st.dim("Size:"),
            st.bold_green(&rec.size_human),
            rec.size,
            st.dim("Blocks:"),
            rec.blocks,
            st.dim("Inode:"),
            rec.inode
        );
        println!(
            "{} {} ({})   {} {}:{} ({}:{})   {} {}",
            st.dim("Access:"),
            st.bold_yellow(&rec.mode_symbolic),
            st.yellow(&rec.mode_octal),
            st.dim("Owner:"),
            st.bold(&rec.user),
            st.bold(&rec.group),
            rec.uid,
            rec.gid,
            st.dim("Links:"),
            rec.links
        );
        println!(
            "{} {} ({})",
            st.dim("Modify:"),
            rec.modified,
            st.cyan(&rec.modified_relative)
        );
        println!("{} {}", st.dim("Access:"), rec.accessed);
        println!("{} {}", st.dim("Change:"), rec.changed);
    }
    Ok(exit_code)
}

// ---------------------------------------------------------------------------
// 7. rm — GNU semantics plus `--safe` (or `--trash`) undoable trash staging
// ---------------------------------------------------------------------------
const RM_HELP: &str = "\
Usage: rm [OPTION]... [FILE]...
Remove (unlink) the FILE(s).

  -f, --force           ignore nonexistent files and arguments, never prompt
  -i                    prompt before every removal
  -I                    prompt once before removing more than three files, or
                          when removing recursively
      --interactive[=WHEN]  prompt according to WHEN: never, once (-I), or
                          always (-i); without WHEN, prompt always
      --one-file-system  when removing a hierarchy recursively, skip any
                          directory that is on a file system different from
                          that of the corresponding command line argument
      --no-preserve-root  do not treat '/' specially
      --preserve-root[=all]  do not remove '/' (default);
                              with 'all', reject any command line argument
                              on a separate device from its parent
  -r, -R, --recursive   remove directories and their contents recursively
  -d, --dir             remove empty directories
  -v, --verbose         explain what is being done
      --safe, --trash   move files to Dogfish's trash instead (undo with
                          `dogfish undo`)
      --help        display this help and exit
      --version     output version information and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum RmOpt {
    Dir,
    Force,
    EveryFile,
    Once,
    InteractiveWhen,
    OneFileSystem,
    NoPreserveRoot,
    PreserveRoot,
    Recursive,
    Verbose,
    Safe,
}

struct RmRun {
    force: bool,
    interactive: Interactive,
    recursive: bool,
    dir: bool,
    verbose: bool,
    one_file_system: bool,
    stdin_tty: bool,
}

/// Outcome of removing one file or tree.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RmStatus {
    Removed,
    Declined,
    /// Declined to descend: the directory stays, so its ancestors are left alone.
    Kept,
    Failed,
}

/// True when `p` names the root directory itself (`/`, `//`, `/.`, `/usr/..`, a bind
/// of the same inode), as `--preserve-root` checks. A symlink pointing at `/` is not
/// the root: removing it only unlinks the link.
fn is_root_dir(p: &Path) -> bool {
    p.symlink_metadata().is_ok_and(|m| m.is_dir()) && same_file(p, Path::new("/"))
}

/// True when the last component of `f` is `.` or `..` (`d/.`, `./`, `..//`).
fn is_dot_or_dotdot(f: &str) -> bool {
    let last = last_component(f.as_bytes());
    let name = &last[..last.iter().position(|&b| b == b'/').unwrap_or(last.len())];
    name == b"." || name == b".."
}

pub fn run_rm(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    use RmOpt::*;
    const SPECS: &[ToolOpt<RmOpt>] = &[
        opt("d", "dir", HasArg::No, Dir),
        opt("f", "force", HasArg::No, Force),
        opt("i", "", HasArg::No, EveryFile),
        opt("I", "", HasArg::No, Once),
        opt("", "interactive", HasArg::Optional, InteractiveWhen),
        opt("", "one-file-system", HasArg::No, OneFileSystem),
        opt("", "no-preserve-root", HasArg::No, NoPreserveRoot),
        opt("", "preserve-root", HasArg::Optional, PreserveRoot),
        opt("rR", "recursive", HasArg::No, Recursive),
        opt("v", "verbose", HasArg::No, Verbose),
        opt("", "safe", HasArg::No, Safe),
        opt("", "trash", HasArg::No, Safe),
    ];
    let parsed = match parse_tool_args("rm", &args, SPECS, RM_HELP) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let mut run = RmRun {
        force: false,
        interactive: Interactive::Sometimes,
        recursive: false,
        dir: false,
        verbose: false,
        one_file_system: false,
        stdin_tty: io::stdin().is_terminal(),
    };
    let (mut prompt_once, mut preserve_root, mut preserve_all, mut safe) =
        (false, true, false, false);
    for (o, v) in parsed.opts {
        match o {
            Dir => run.dir = true,
            Force => {
                run.force = true;
                run.interactive = Interactive::Never;
                prompt_once = false;
            }
            EveryFile => {
                run.force = false;
                run.interactive = Interactive::Always;
                prompt_once = false;
            }
            Once => {
                run.force = false;
                run.interactive = Interactive::Sometimes;
                prompt_once = true;
            }
            InteractiveWhen => {
                #[derive(Clone, Copy, PartialEq)]
                enum When {
                    Never,
                    Once,
                    Always,
                }
                const WHEN: &[(&str, When)] = &[
                    ("never", When::Never),
                    ("no", When::Never),
                    ("none", When::Never),
                    ("once", When::Once),
                    ("always", When::Always),
                    ("yes", When::Always),
                ];
                let when = match v {
                    Some(w) => match argmatch("rm", "--interactive", &w, WHEN) {
                        Ok(w) => w,
                        Err(code) => return Ok(code),
                    },
                    None => When::Always,
                };
                match when {
                    When::Never => {
                        run.interactive = Interactive::Never;
                        prompt_once = false;
                    }
                    When::Once => {
                        run.force = false;
                        run.interactive = Interactive::Sometimes;
                        prompt_once = true;
                    }
                    When::Always => {
                        run.force = false;
                        run.interactive = Interactive::Always;
                        prompt_once = false;
                    }
                }
            }
            OneFileSystem => run.one_file_system = true,
            NoPreserveRoot => preserve_root = false,
            PreserveRoot => {
                match v.as_deref() {
                    None => {}
                    Some("all") => preserve_all = true,
                    Some(other) => {
                        eprintln!(
                            "rm: unrecognized --preserve-root argument: {}",
                            quote_locale(other)
                        );
                        return Ok(1);
                    }
                }
                preserve_root = true;
            }
            Recursive => run.recursive = true,
            Verbose => run.verbose = true,
            Safe => safe = true,
        }
    }
    let files = parsed.operands;
    if files.is_empty() {
        return Ok(if run.force {
            0
        } else {
            usage_error("rm", "missing operand")
        });
    }
    if prompt_once && (files.len() > 3 || run.recursive) {
        let n = files.len();
        let question = format!(
            "remove {n} argument{}{}? ",
            if n == 1 { "" } else { "s" },
            if run.recursive { " recursively" } else { "" }
        );
        if !ask("rm", &question) {
            return Ok(0);
        }
    }

    let journal = if safe {
        Some(UndoJournal::open()?)
    } else {
        None
    };
    ctx.telemetry.set_strategy(if safe {
        "journaled trash staging (undoable via `dogfish undo`)"
    } else {
        "direct unlink"
    });

    let mut exit_code = 0;
    for f in &files {
        let p = Path::new(f);
        let meta = match p.symlink_metadata() {
            Ok(m) => m,
            Err(e) => {
                let missing = matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR));
                if !(run.force && missing) {
                    eprintln!("rm: cannot remove {}: {}", qa(f), gnu_io_error(&e));
                    exit_code = 1;
                }
                continue;
            }
        };
        // GNU checks, in this order: directory without -r, `.`/`..`, then `/`.
        if meta.is_dir() && !run.recursive {
            let empty = fs::read_dir(p).is_ok_and(|mut d| d.next().is_none());
            if !run.dir || !empty {
                let why = if run.dir {
                    "Directory not empty"
                } else {
                    "Is a directory"
                };
                eprintln!("rm: cannot remove {}: {why}", qa(f));
                exit_code = 1;
                continue;
            }
        }
        if is_dot_or_dotdot(f) {
            eprintln!(
                "rm: refusing to remove '.' or '..' directory: skipping {}",
                qa(f)
            );
            exit_code = 1;
            continue;
        }
        if run.recursive && preserve_root && is_root_dir(p) {
            refuse_root("rm", f);
            exit_code = 1;
            continue;
        }
        if preserve_all
            && meta.is_dir()
            && let Ok(parent) = fs::metadata(p.join(".."))
            && parent.dev() != meta.dev()
        {
            eprintln!("rm: skipping {}, since it's on a different device", qa(f));
            eprintln!("rm: and --preserve-root=all is in effect");
            exit_code = 1;
            continue;
        }

        if let Some(j) = &journal {
            // `--safe` refuses exactly what plain `rm` refuses, then moves the whole
            // operand to the trash in one undoable step.
            if run.interactive == Interactive::Always
                && !ask(
                    "rm",
                    &format!("remove {} {}? ", file_type_description(&meta), qa(f)),
                )
            {
                continue;
            }
            match j.stage_to_trash(p) {
                Ok(entry) => {
                    ctx.telemetry.add_items(1);
                    if run.verbose {
                        let st = ctx.style();
                        println!(
                            "{} {} (undo id #{}: run `{}` to restore)",
                            st.yellow("trashed"),
                            qa(f),
                            entry.id,
                            st.bold_cyan("dogfish undo")
                        );
                    }
                }
                Err(e) => {
                    eprintln!(
                        "rm: failed to stage {} to trash: {}",
                        qa(f),
                        gnu_io_error(&e)
                    );
                    exit_code = 1;
                }
            }
            continue;
        }

        let status = if meta.is_dir() {
            if run.recursive {
                rm_tree(p, &meta, meta.dev(), &run, ctx)
            } else {
                rm_empty_dir(p, &meta, &run, ctx)
            }
        } else {
            rm_file(p, &meta, &run, ctx)
        };
        if status == RmStatus::Failed {
            exit_code = 1;
        }
    }
    Ok(exit_code)
}

/// Whether to ask before removing `p`: always with `-i`, and for write-protected
/// files when stdin is a terminal (unless `-f`). Returns the write-protected flag.
fn rm_prompt_needed(p: &Path, meta: &fs::Metadata, run: &RmRun) -> Option<bool> {
    if run.interactive == Interactive::Never {
        return None;
    }
    let protected = !meta.is_symlink() && write_protected(p);
    (run.interactive == Interactive::Always || (protected && run.stdin_tty)).then_some(protected)
}

fn protected_prefix(protected: bool) -> &'static str {
    if protected { "write-protected " } else { "" }
}

fn rm_file(p: &Path, meta: &fs::Metadata, run: &RmRun, ctx: &DogfishContext) -> RmStatus {
    if let Some(protected) = rm_prompt_needed(p, meta, run) {
        let q = format!(
            "remove {}{} {}? ",
            protected_prefix(protected),
            file_type_description(meta),
            qa(p)
        );
        if !ask("rm", &q) {
            return RmStatus::Declined;
        }
    }
    match fs::remove_file(p) {
        Ok(()) => {
            ctx.telemetry.add_items(1);
            if run.verbose {
                println!("removed {}", qa(p));
            }
            RmStatus::Removed
        }
        Err(e) => {
            eprintln!("rm: cannot remove {}: {}", qa(p), gnu_io_error(&e));
            RmStatus::Failed
        }
    }
}

fn rm_empty_dir(p: &Path, meta: &fs::Metadata, run: &RmRun, ctx: &DogfishContext) -> RmStatus {
    if let Some(protected) = rm_prompt_needed(p, meta, run)
        && !ask(
            "rm",
            &format!(
                "remove {}directory {}? ",
                protected_prefix(protected),
                qa(p)
            ),
        )
    {
        return RmStatus::Declined;
    }
    match fs::remove_dir(p) {
        Ok(()) => {
            ctx.telemetry.add_items(1);
            if run.verbose {
                println!("removed directory {}", qa(p));
            }
            RmStatus::Removed
        }
        // `-f` ignores a directory that vanished (or a `link/` that is no directory).
        Err(e) if run.force && matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => {
            RmStatus::Removed
        }
        Err(e) => {
            eprintln!("rm: cannot remove {}: {}", qa(p), gnu_io_error(&e));
            RmStatus::Failed
        }
    }
}

/// Removes a directory tree depth-first in `readdir` order, like GNU's `fts`
/// traversal. Symbolic links are removed, never followed. After an error inside
/// a directory, its ancestors are left alone (GNU reports only the first error).
fn rm_tree(
    p: &Path,
    meta: &fs::Metadata,
    top_dev: u64,
    run: &RmRun,
    ctx: &DogfishContext,
) -> RmStatus {
    let empty = fs::read_dir(p).is_ok_and(|mut d| d.next().is_none());
    if !empty {
        if let Some(protected) = rm_prompt_needed(p, meta, run)
            && !ask(
                "rm",
                &format!(
                    "descend into {}directory {}? ",
                    protected_prefix(protected),
                    qa(p)
                ),
            )
        {
            return RmStatus::Kept;
        }
        let names = match dir_names(p, false) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("rm: cannot remove {}: {}", qa(p), gnu_io_error(&e));
                return RmStatus::Failed;
            }
        };
        let (mut failed, mut kept) = (false, false);
        for name in names {
            let child = p.join(name);
            let cmeta = match child.symlink_metadata() {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("rm: cannot remove {}: {}", qa(&child), gnu_io_error(&e));
                    failed = true;
                    continue;
                }
            };
            let status = if cmeta.is_dir() {
                if run.one_file_system && cmeta.dev() != top_dev {
                    eprintln!(
                        "rm: skipping {}, since it's on a different device",
                        qa(&child)
                    );
                    RmStatus::Failed
                } else {
                    rm_tree(&child, &cmeta, top_dev, run, ctx)
                }
            } else {
                rm_file(&child, &cmeta, run, ctx)
            };
            failed |= status == RmStatus::Failed;
            kept |= status == RmStatus::Kept;
        }
        if failed {
            return RmStatus::Failed;
        }
        if kept {
            return RmStatus::Kept;
        }
    }
    rm_empty_dir(p, meta, run, ctx)
}

// ---------------------------------------------------------------------------
// 8. cp — GNU semantics, CoW reflinks (FICLONE / APFS clonefile) and UndoJournal
// ---------------------------------------------------------------------------
const CP_HELP: &str = "\
Usage: cp [OPTION]... [-T] SOURCE DEST
  or:  cp [OPTION]... SOURCE... DIRECTORY
  or:  cp [OPTION]... -t DIRECTORY SOURCE...
Copy SOURCE to DEST, or multiple SOURCE(s) to DIRECTORY.
Copies use copy-on-write reflinks where the file system supports them, and
are recorded so that `dogfish undo` can reverse them.

  -a, --archive                same as -dR --preserve=all
      --attributes-only        don't copy the file data, just the attributes
      --backup[=CONTROL]       make a backup of each existing destination file
  -b                           like --backup but does not accept an argument
  -d                           same as --no-dereference --preserve=links
  -f, --force                  if an existing destination file cannot be
                                 opened, remove it and try again
  -i, --interactive            prompt before overwrite (overrides a previous -n
                                  option)
  -H                           follow command-line symbolic links in SOURCE
  -l, --link                   hard link files instead of copying
  -L, --dereference            always follow symbolic links in SOURCE
  -n, --no-clobber             do not overwrite an existing file (overrides a
                                 previous -i option)
  -P, --no-dereference         never follow symbolic links in SOURCE
  -p                           same as --preserve=mode,ownership,timestamps
      --preserve[=ATTR_LIST]   preserve the specified attributes
      --no-preserve=ATTR_LIST  don't preserve the specified attributes
      --parents                use full source file name under DIRECTORY
  -R, -r, --recursive          copy directories recursively
      --remove-destination     remove each existing destination file before
                                 attempting to open it (contrast with --force)
  -s, --symbolic-link          make symbolic links instead of copying
  -S, --suffix=SUFFIX          override the usual backup suffix
  -t, --target-directory=DIRECTORY  copy all SOURCE arguments into DIRECTORY
  -T, --no-target-directory    treat DEST as a normal file
      --update[=UPDATE]        control which existing files are updated;
                                 UPDATE={all,none,older(default)}
  -u                           equivalent to --update[=older]
  -v, --verbose                explain what is being done
  -x, --one-file-system        stay on this file system
      --help        display this help and exit
      --version     output version information and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum CpOpt {
    Archive,
    AttributesOnly,
    Backup,
    BackupArg,
    CopyContents,
    Debug,
    NoDerefLinks,
    Dereference,
    Force,
    Interactive,
    CmdlineDeref,
    Link,
    NoClobber,
    NoDereference,
    PreserveDefault,
    Preserve,
    NoPreserve,
    Parents,
    KeepDirSymlink,
    Recursive,
    RemoveDestination,
    Reflink,
    Sparse,
    StripSlashes,
    Suffix,
    Symbolic,
    TargetDir,
    NoTargetDir,
    UpdateOlder,
    Update,
    Verbose,
    OneFileSystem,
    Context,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Deref {
    Never,
    CommandLine,
    Always,
}

/// What to do when the destination exists (GNU's `interactive` setting).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Clobber {
    /// Overwrite (the default).
    Yes,
    /// Ask first (`-i`).
    Ask,
    /// Skip silently (`cp -n`), or with a diagnostic and failure (`mv -n`).
    No,
    /// Skip silently and successfully (`--update=none`).
    Skip,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PreserveAttr {
    Mode,
    Timestamps,
    Ownership,
    Links,
    Context,
    Xattr,
    All,
}

const PRESERVE_ARGS: &[(&str, PreserveAttr)] = &[
    ("mode", PreserveAttr::Mode),
    ("timestamps", PreserveAttr::Timestamps),
    ("ownership", PreserveAttr::Ownership),
    ("links", PreserveAttr::Links),
    ("context", PreserveAttr::Context),
    ("xattr", PreserveAttr::Xattr),
    ("all", PreserveAttr::All),
];

struct CpOpts {
    recursive: bool,
    deref: Deref,
    preserve_mode: bool,
    preserve_ownership: bool,
    preserve_timestamps: bool,
    preserve_links: bool,
    /// `--no-preserve=mode`: new files get default permissions, not the source's.
    no_preserve_mode: bool,
    clobber: Clobber,
    update_older: bool,
    backup: BackupMode,
    suffix: String,
    force: bool,
    remove_destination: bool,
    hard_link: bool,
    symbolic_link: bool,
    parents: bool,
    one_file_system: bool,
    verbose: bool,
    attributes_only: bool,
    keep_dir_symlink: bool,
}

/// What the undo journal needs to restore an overwritten destination.
struct UndoPlan {
    created: PathBuf,
    backup: Option<String>,
}

/// State for one `cp` invocation.
struct Copier<'a> {
    o: CpOpts,
    journal: Option<UndoJournal>,
    umask: u32,
    ctx: &'a DogfishContext,
    /// Command-line sources already copied, by name and inode (GNU warns about repeats).
    seen_sources: HashSet<(OsString, u64, u64)>,
    /// Destination of each multiply-linked source inode, for `--preserve=links`.
    linked: HashMap<(u64, u64), PathBuf>,
    top_dev: u64,
}

fn apply_preserve(o: &mut CpOpts, list: &str, on: bool) -> std::result::Result<(), i32> {
    for item in list.split(',') {
        let attr = argmatch(
            "cp",
            if on { "--preserve" } else { "--no-preserve" },
            item,
            PRESERVE_ARGS,
        )?;
        if matches!(attr, PreserveAttr::Mode | PreserveAttr::All) {
            o.no_preserve_mode = !on;
        }
        match attr {
            PreserveAttr::Mode => o.preserve_mode = on,
            PreserveAttr::Timestamps => o.preserve_timestamps = on,
            PreserveAttr::Ownership => o.preserve_ownership = on,
            PreserveAttr::Links => o.preserve_links = on,
            PreserveAttr::Context | PreserveAttr::Xattr => {}
            PreserveAttr::All => {
                o.preserve_mode = on;
                o.preserve_timestamps = on;
                o.preserve_ownership = on;
                o.preserve_links = on;
            }
        }
    }
    Ok(())
}

/// Splits operands into sources and a destination the way GNU `cp` and `mv` do.
/// `Ok(None)` means each source goes into the returned directory.
enum Targets {
    Into(String, Vec<String>),
    Single(String, String),
}

fn resolve_targets(
    tool: &str,
    mut files: Vec<String>,
    target_dir: Option<String>,
    no_target_dir: bool,
) -> std::result::Result<Targets, i32> {
    if let Some(dir) = target_dir {
        if no_target_dir {
            eprintln!(
                "{tool}: cannot combine --target-directory (-t) and --no-target-directory (-T)"
            );
            return Err(1);
        }
        let check = fs::metadata(&dir).and_then(|m| {
            if m.is_dir() {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(libc::ENOTDIR))
            }
        });
        if let Err(e) = check {
            eprintln!(
                "{tool}: target directory {}: {}",
                qa(&dir),
                gnu_io_error(&e)
            );
            return Err(1);
        }
        if files.is_empty() {
            return Err(usage_error(tool, "missing file operand"));
        }
        return Ok(Targets::Into(dir, files));
    }
    match files.len() {
        0 => return Err(usage_error(tool, "missing file operand")),
        1 => {
            return Err(usage_error(
                tool,
                &format!("missing destination file operand after {}", qa(&files[0])),
            ));
        }
        _ => {}
    }
    if no_target_dir {
        if files.len() > 2 {
            return Err(usage_error(
                tool,
                &format!("extra operand {}", qa(&files[2])),
            ));
        }
        let dst = files.pop().unwrap_or_default();
        return Ok(Targets::Single(files.remove(0), dst));
    }
    let last = files.last().cloned().unwrap_or_default();
    match fs::metadata(&last) {
        Ok(m) if m.is_dir() => {
            files.pop();
            Ok(Targets::Into(last, files))
        }
        res if files.len() > 2 => {
            let err = res
                .err()
                .unwrap_or_else(|| io::Error::from_raw_os_error(libc::ENOTDIR));
            eprintln!("{tool}: target {}: {}", qa(&last), gnu_io_error(&err));
            Err(1)
        }
        _ => {
            let dst = files.pop().unwrap_or_default();
            Ok(Targets::Single(files.remove(0), dst))
        }
    }
}

pub fn run_cp(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    use CpOpt::*;
    const SPECS: &[ToolOpt<CpOpt>] = &[
        opt("a", "archive", HasArg::No, Archive),
        opt("", "attributes-only", HasArg::No, AttributesOnly),
        opt("b", "", HasArg::No, Backup),
        opt("", "backup", HasArg::Optional, BackupArg),
        opt("", "copy-contents", HasArg::No, CopyContents),
        opt("", "debug", HasArg::No, Debug),
        opt("d", "", HasArg::No, NoDerefLinks),
        opt("L", "dereference", HasArg::No, Dereference),
        opt("f", "force", HasArg::No, Force),
        opt("i", "interactive", HasArg::No, Interactive),
        opt("H", "", HasArg::No, CmdlineDeref),
        opt("l", "link", HasArg::No, Link),
        opt("n", "no-clobber", HasArg::No, NoClobber),
        opt("P", "no-dereference", HasArg::No, NoDereference),
        opt("", "no-preserve", HasArg::Required, NoPreserve),
        opt("", "no-target-directory", HasArg::No, NoTargetDir),
        opt("x", "one-file-system", HasArg::No, OneFileSystem),
        opt("", "parents", HasArg::No, Parents),
        opt("", "path", HasArg::No, Parents),
        opt("", "keep-directory-symlink", HasArg::No, KeepDirSymlink),
        opt("p", "", HasArg::No, PreserveDefault),
        opt("", "preserve", HasArg::Optional, Preserve),
        opt("rR", "recursive", HasArg::No, Recursive),
        opt("", "remove-destination", HasArg::No, RemoveDestination),
        opt("", "reflink", HasArg::Optional, Reflink),
        opt("", "sparse", HasArg::Required, Sparse),
        opt("", "strip-trailing-slashes", HasArg::No, StripSlashes),
        opt("S", "suffix", HasArg::Required, Suffix),
        opt("s", "symbolic-link", HasArg::No, Symbolic),
        opt("t", "target-directory", HasArg::Required, TargetDir),
        opt("T", "", HasArg::No, NoTargetDir),
        opt("u", "", HasArg::No, UpdateOlder),
        opt("", "update", HasArg::Optional, Update),
        opt("v", "verbose", HasArg::No, Verbose),
        opt("Z", "", HasArg::No, Context),
        opt("", "context", HasArg::Optional, Context),
    ];
    let parsed = match parse_tool_args("cp", &args, SPECS, CP_HELP) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let mut o = CpOpts {
        recursive: false,
        deref: Deref::Always,
        preserve_mode: false,
        preserve_ownership: false,
        preserve_timestamps: false,
        preserve_links: false,
        no_preserve_mode: false,
        clobber: Clobber::Yes,
        update_older: false,
        backup: BackupMode::None,
        suffix: String::new(),
        force: false,
        remove_destination: false,
        hard_link: false,
        symbolic_link: false,
        parents: false,
        one_file_system: false,
        verbose: false,
        attributes_only: false,
        keep_dir_symlink: false,
    };
    let mut deref = None;
    let (mut want_backup, mut backup_arg, mut suffix) = (false, None, None);
    let (mut target_dir, mut no_target_dir, mut strip_slashes) = (None, false, false);
    for (opt, v) in parsed.opts {
        let v = v.unwrap_or_default();
        match opt {
            Archive => {
                deref = Some(Deref::Never);
                o.recursive = true;
                let _ = apply_preserve(&mut o, "all", true);
            }
            AttributesOnly => o.attributes_only = true,
            Backup => want_backup = true,
            BackupArg => {
                want_backup = true;
                backup_arg = (!v.is_empty()).then_some(v);
            }
            CopyContents | Debug | Reflink | Context => {}
            Sparse => {
                const SPARSE: &[(&str, ())] = &[("never", ()), ("auto", ()), ("always", ())];
                if let Err(code) = argmatch("cp", "--sparse", &v, SPARSE) {
                    return Ok(code);
                }
            }
            NoDerefLinks => {
                deref = Some(Deref::Never);
                o.preserve_links = true;
            }
            Dereference => deref = Some(Deref::Always),
            NoDereference => deref = Some(Deref::Never),
            CmdlineDeref => deref = Some(Deref::CommandLine),
            Force => o.force = true,
            Interactive => o.clobber = Clobber::Ask,
            NoClobber => {
                o.clobber = Clobber::No;
                eprintln!(
                    "cp: warning: behavior of -n is non-portable and may change in future; use --update=none instead"
                );
            }
            Link => o.hard_link = true,
            PreserveDefault => {
                o.preserve_mode = true;
                o.preserve_ownership = true;
                o.preserve_timestamps = true;
            }
            Preserve => {
                let list = if v.is_empty() {
                    "mode,ownership,timestamps"
                } else {
                    &v
                };
                if let Err(code) = apply_preserve(&mut o, list, true) {
                    return Ok(code);
                }
            }
            NoPreserve => {
                if let Err(code) = apply_preserve(&mut o, &v, false) {
                    return Ok(code);
                }
            }
            Parents => o.parents = true,
            KeepDirSymlink => o.keep_dir_symlink = true,
            Recursive => o.recursive = true,
            RemoveDestination => o.remove_destination = true,
            StripSlashes => strip_slashes = true,
            Suffix => {
                want_backup = true;
                suffix = Some(v);
            }
            Symbolic => o.symbolic_link = true,
            TargetDir => target_dir = Some(v),
            NoTargetDir => no_target_dir = true,
            UpdateOlder => o.update_older = true,
            Update => {
                #[derive(Clone, Copy, PartialEq)]
                enum Upd {
                    All,
                    None,
                    Older,
                }
                const UPDATE: &[(&str, Upd)] = &[
                    ("all", Upd::All),
                    ("none", Upd::None),
                    ("older", Upd::Older),
                ];
                let which = if v.is_empty() {
                    Upd::Older
                } else {
                    match argmatch("cp", "--update", &v, UPDATE) {
                        Ok(u) => u,
                        Err(code) => return Ok(code),
                    }
                };
                o.update_older = which == Upd::Older;
                if which == Upd::None {
                    o.clobber = Clobber::Skip;
                }
            }
            Verbose => o.verbose = true,
            OneFileSystem => o.one_file_system = true,
        }
    }
    if o.hard_link && o.symbolic_link {
        return Ok(usage_error(
            "cp",
            "cannot make both hard and symbolic links",
        ));
    }
    if want_backup {
        o.backup = match backup_mode("cp", backup_arg.as_deref()) {
            Ok(m) => m,
            Err(code) => return Ok(code),
        };
    }
    o.suffix = backup_suffix(suffix);
    o.deref = deref.unwrap_or(if o.recursive && !o.hard_link {
        Deref::Never
    } else {
        Deref::Always
    });
    let targets = match resolve_targets("cp", parsed.operands, target_dir, no_target_dir) {
        Ok(t) => t,
        Err(code) => return Ok(code),
    };
    if o.parents && matches!(targets, Targets::Single(..)) {
        return Ok(usage_error(
            "cp",
            "with --parents, the destination must be a directory",
        ));
    }
    let mut copier = Copier {
        o,
        journal: UndoJournal::open_for_recording(),
        umask: process_umask(),
        ctx,
        seen_sources: HashSet::new(),
        linked: HashMap::new(),
        top_dev: 0,
    };
    let strip = |s: &str| {
        if strip_slashes {
            strip_trailing_slashes(s)
        } else {
            s.to_string()
        }
    };
    let mut ok = true;
    match targets {
        Targets::Into(dir, sources) => {
            for src in &sources {
                let src = strip(src);
                let dst = if copier.o.parents {
                    let rel = src.trim_start_matches('/');
                    if !copier.make_parent_dirs(&dir, rel) {
                        ok = false;
                        continue;
                    }
                    concat_path(&dir, OsStr::new(rel))
                } else {
                    concat_path(&dir, &operand_base(&src))
                };
                ok &= copier.copy_top(Path::new(&src), &dst, true);
            }
        }
        Targets::Single(src, dst) => {
            // Like GNU, --strip-trailing-slashes only shapes names formed in a directory.
            ok &= copier.copy_top(Path::new(&src), Path::new(&dst), false);
        }
    }
    Ok(if ok { 0 } else { 1 })
}

impl Copier<'_> {
    /// `--parents`: creates the directories leading to `rel` under `dir`, copying
    /// the source directories' modes; prints `src -> dst` for each one created.
    fn make_parent_dirs(&mut self, dir: &str, rel: &str) -> bool {
        let ends = ancestor_ends(rel);
        for end in ends {
            let src_dir = &rel[..end];
            let dst_dir = concat_path(dir, OsStr::new(src_dir));
            match fs::metadata(&dst_dir) {
                Ok(m) if m.is_dir() => continue,
                Ok(_) => {
                    eprintln!("cp: {} exists but is not a directory", qa(&dst_dir));
                    return false;
                }
                Err(_) => {}
            }
            let src_meta = match fs::metadata(src_dir) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!(
                        "cp: failed to get attributes of {}: {}",
                        qa(src_dir),
                        gnu_io_error(&e)
                    );
                    return false;
                }
            };
            let mode = if self.o.preserve_mode {
                src_meta.mode() & CHMOD_MODE_BITS
            } else {
                src_meta.mode() & S_IRWXUGO & !self.umask
            };
            if let Err(e) = fs::create_dir(&dst_dir) {
                eprintln!(
                    "cp: cannot make directory {}: {}",
                    qa(&dst_dir),
                    gnu_io_error(&e)
                );
                return false;
            }
            let _ = fs::set_permissions(&dst_dir, fs::Permissions::from_mode(mode));
            if self.o.verbose {
                println!("{src_dir} -> {}", dst_dir.display());
            }
        }
        true
    }

    fn src_metadata(&self, src: &Path, command_line: bool) -> io::Result<fs::Metadata> {
        let follow = match self.o.deref {
            Deref::Always => true,
            Deref::CommandLine => command_line,
            Deref::Never => false,
        };
        if follow {
            fs::metadata(src)
        } else {
            fs::symlink_metadata(src)
        }
    }

    /// Copies one command-line operand; `into_dir` is set when the destination was
    /// formed inside a target directory.
    fn copy_top(&mut self, src: &Path, dst: &Path, into_dir: bool) -> bool {
        let src_meta = match self.src_metadata(src, true) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("cp: cannot stat {}: {}", qa(src), gnu_io_error(&e));
                return false;
            }
        };
        if src_meta.is_dir() {
            if !self.o.recursive {
                eprintln!("cp: -r not specified; omitting directory {}", qa(src));
                return false;
            }
            let dst_lmeta = fs::symlink_metadata(dst).ok();
            if let Some(dm) = &dst_lmeta
                && same_inode(&src_meta, dm)
            {
                eprintln!("cp: {} and {} are the same file", qa(src), qa(dst));
                return false;
            }
            // Refuse up front (GNU notices only once it has copied part of the tree).
            // An existing non-directory is reported by `copy_entry` instead.
            if dst_lmeta.as_ref().is_none_or(|m| m.is_dir())
                && let Ok(abs_src) = fs::canonicalize(src)
                && resolve_for_containment(dst).starts_with(&abs_src)
            {
                eprintln!(
                    "cp: cannot copy a directory, {}, into itself, {}",
                    qa(src),
                    qa(dst)
                );
                return false;
            }
        } else if into_dir && self.o.backup == BackupMode::None {
            let key = (
                src.as_os_str().to_os_string(),
                src_meta.dev(),
                src_meta.ino(),
            );
            if !self.seen_sources.insert(key) {
                eprintln!(
                    "cp: warning: source file {} specified more than once",
                    qa(src)
                );
                return true;
            }
        }
        self.top_dev = src_meta.dev();
        self.copy_entry(src, dst, &src_meta, false, true)
    }

    /// Saves what `created` currently holds so `dogfish undo` can restore it.
    /// `by_copy` keeps it in place (it is written into, not replaced).
    fn journal_prepare(&self, created: &Path, by_copy: bool) -> std::result::Result<UndoPlan, ()> {
        let backup = match &self.journal {
            Some(j) => j.backup_existing(created, by_copy).map_err(|e| {
                eprintln!(
                    "cp: cannot back up {} before overwriting: {}",
                    qa(created),
                    gnu_io_error(&e)
                );
            })?,
            None => None,
        };
        Ok(UndoPlan {
            created: created.to_path_buf(),
            backup,
        })
    }

    fn journal_commit(&self, src: &Path, plan: UndoPlan) {
        if let Some(j) = &self.journal {
            let _ = j.record(
                "cp",
                JournalAction::CopyCreate {
                    src: absolute_lossy(src),
                    created_path: absolute_lossy(&plan.created),
                    backup_path: plan.backup,
                },
            );
        }
    }

    /// Removes an existing destination before it is replaced; with a journal it is
    /// moved to the trash instead, so the replacement can be undone.
    fn remove_existing(
        &self,
        dst: &Path,
        plan: &mut Option<UndoPlan>,
        top: bool,
    ) -> io::Result<()> {
        if top && self.journal.is_some() && plan.as_ref().is_none_or(|p| p.backup.is_none()) {
            if let Some(j) = &self.journal
                && let Some(b) = j.backup_existing(dst, false)?
            {
                *plan = Some(UndoPlan {
                    created: dst.to_path_buf(),
                    backup: Some(b),
                });
            }
            return Ok(());
        }
        match fs::remove_file(dst) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    fn emit_verbose(&self, src: &Path, dst: &Path, backup: Option<&Path>) {
        if !self.o.verbose {
            return;
        }
        match backup {
            Some(b) => println!("{} -> {} (backup: {})", qa(src), qa(dst), qa(b)),
            None => println!("{} -> {}", qa(src), qa(dst)),
        }
    }

    /// Copies `src` (already stat'ed as `src_meta`) to `dst`, printing GNU's
    /// diagnostics. `new_dst` means `dst` is known not to exist (inside a freshly
    /// created directory); `top` marks a command-line operand.
    fn copy_entry(
        &mut self,
        src: &Path,
        dst: &Path,
        src_meta: &fs::Metadata,
        new_dst: bool,
        top: bool,
    ) -> bool {
        let src_is_dir = src_meta.is_dir();
        let src_is_link = src_meta.is_symlink();
        let dst_lmeta = if new_dst {
            None
        } else {
            match fs::symlink_metadata(dst) {
                Ok(m) => Some(m),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => {
                    eprintln!("cp: cannot stat {}: {}", qa(dst), gnu_io_error(&e));
                    return false;
                }
            }
        };
        let dst_meta = dst_lmeta.as_ref().and_then(|_| fs::metadata(dst).ok());
        let mut backup_made: Option<PathBuf> = None;
        let mut plan: Option<UndoPlan> = None;

        if let Some(dl) = &dst_lmeta {
            // GNU skips with -n / --update=none before any other check.
            let exists = src_is_link || dst_meta.is_some();
            if !src_is_dir && exists && matches!(self.o.clobber, Clobber::No | Clobber::Skip) {
                return true;
            }
            // Same-file check: never truncate or unlink the source itself.
            let same = if src_is_link {
                same_inode(src_meta, dl)
            } else {
                dst_meta.as_ref().is_some_and(|dm| same_inode(src_meta, dm))
            };
            // Removing a destination symlink first is fine (`--remove-destination`).
            if self.o.hard_link && !src_is_dir && same_inode(src_meta, dl) {
                // Already a link to the source: nothing to do.
                return true;
            }
            if same && !(dl.is_symlink() && self.o.remove_destination) {
                eprintln!("cp: {} and {} are the same file", qa(src), qa(dst));
                return false;
            }
            // A symlink to a directory only counts as one with --keep-directory-symlink.
            let dst_is_dir = dl.is_dir()
                || (dl.is_symlink()
                    && dst_meta.as_ref().is_some_and(|m| m.is_dir())
                    && (self.o.keep_dir_symlink || !src_is_dir));
            if src_is_dir {
                if !dst_is_dir {
                    eprintln!(
                        "cp: cannot overwrite non-directory {} with directory {}",
                        qa(dst),
                        qa(src)
                    );
                    return false;
                }
            } else {
                if dst_is_dir {
                    eprintln!(
                        "cp: cannot overwrite directory {} with non-directory",
                        qa(dst)
                    );
                    return false;
                }
                if self.o.update_older
                    && let Some(dm) = dst_meta.as_ref().or(src_is_link.then_some(dl))
                    && (dm.mtime(), dm.mtime_nsec()) >= (src_meta.mtime(), src_meta.mtime_nsec())
                {
                    return true;
                }
                match self.o.clobber {
                    Clobber::No | Clobber::Skip => return true,
                    Clobber::Ask => {
                        let protected = !dl.is_symlink() && write_protected(dst);
                        let q = if protected {
                            let m = dl.mode() & CHMOD_MODE_BITS;
                            format!(
                                "replace {}, overriding mode {m:04o} ({})? ",
                                qa(dst),
                                strmode9(m)
                            )
                        } else {
                            format!("overwrite {}? ", qa(dst))
                        };
                        if !ask("cp", &q) {
                            return false;
                        }
                    }
                    Clobber::Yes => {}
                }
                if self.o.backup != BackupMode::None {
                    match make_backup(
                        dst,
                        self.o.backup,
                        &self.o.suffix,
                        self.journal.as_ref(),
                        "cp",
                    ) {
                        Ok(b) => backup_made = Some(b),
                        Err(e) => {
                            eprintln!("cp: cannot backup {}: {}", qa(dst), gnu_io_error(&e));
                            return false;
                        }
                    }
                } else if self.o.remove_destination
                    || (self.o.preserve_links && dl.nlink() > 1)
                    || (self.o.deref == Deref::Never && !src_meta.is_file())
                {
                    if let Err(e) = self.remove_existing(dst, &mut plan, top) {
                        eprintln!("cp: cannot remove {}: {}", qa(dst), gnu_io_error(&e));
                        return false;
                    }
                    if self.o.verbose {
                        println!("removed {}", qa(dst));
                    }
                }
            }
        }
        // Whatever still exists at `dst` now is about to be written to.
        let dst_exists = backup_made.is_none() && fs::symlink_metadata(dst).is_ok();

        if !src_is_dir
            && !(self.ctx.stdout_is_tty
                && src_meta.is_file()
                && !self.o.hard_link
                && !self.o.symbolic_link)
        {
            self.emit_verbose(src, dst, backup_made.as_deref());
        }

        // --preserve=links: a source inode seen before becomes a hard link.
        if self.o.preserve_links && !src_is_dir && src_meta.nlink() > 1 {
            let key = (src_meta.dev(), src_meta.ino());
            if let Some(first) = self.linked.get(&key).cloned() {
                if dst_exists && let Err(e) = self.remove_existing(dst, &mut plan, top) {
                    eprintln!("cp: cannot remove {}: {}", qa(dst), gnu_io_error(&e));
                    return false;
                }
                return match fs::hard_link(&first, dst) {
                    Ok(()) => true,
                    Err(e) => {
                        eprintln!(
                            "cp: cannot create hard link {} to {}: {}",
                            qa(dst),
                            qa(&first),
                            gnu_io_error(&e)
                        );
                        false
                    }
                };
            }
            self.linked.insert(key, dst.to_path_buf());
        }

        if self.o.symbolic_link && !src_is_dir {
            return self.make_link(src, dst, dst_exists, top, &mut plan, true);
        }
        if self.o.hard_link && !src_is_dir {
            return self.make_link(src, dst, dst_exists, top, &mut plan, false);
        }
        if src_is_dir {
            return self.copy_dir(src, dst, src_meta, dst_lmeta.is_some(), top);
        }
        if src_is_link {
            let target = match fs::read_link(src) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!(
                        "cp: cannot read symbolic link {}: {}",
                        qa(src),
                        gnu_io_error(&e)
                    );
                    return false;
                }
            };
            if top && plan.is_none() {
                plan = Some(UndoPlan {
                    created: dst.to_path_buf(),
                    backup: None,
                });
            }
            if let Err(e) = symlink(&target, dst) {
                eprintln!(
                    "cp: cannot create symbolic link {}: {}",
                    qa(dst),
                    gnu_io_error(&e)
                );
                return false;
            }
            self.preserve_attrs(dst, src_meta, true);
            if let Some(p) = plan {
                self.journal_commit(src, p);
            }
            return true;
        }
        self.copy_file(src, dst, src_meta, dst_exists, top, plan)
    }

    /// `cp -s` / `cp -l`.
    fn make_link(
        &self,
        src: &Path,
        dst: &Path,
        dst_exists: bool,
        top: bool,
        plan: &mut Option<UndoPlan>,
        symbolic: bool,
    ) -> bool {
        if symbolic && !src.is_absolute() {
            let parent = match dst.parent() {
                Some(p) if !p.as_os_str().is_empty() => p,
                _ => Path::new("."),
            };
            if !same_file(parent, Path::new(".")) {
                eprintln!(
                    "cp: {}: can make relative symbolic links only in current directory",
                    qf(dst)
                );
                return false;
            }
        }
        let create = || {
            if symbolic {
                symlink(src, dst)
            } else if self.o.deref == Deref::Always {
                fs::canonicalize(src).and_then(|real| fs::hard_link(real, dst))
            } else {
                fs::hard_link(src, dst)
            }
        };
        let mut res = create();
        if dst_exists
            && self.o.force
            && res
                .as_ref()
                .is_err_and(|e| e.kind() == io::ErrorKind::AlreadyExists)
        {
            res = self.remove_existing(dst, plan, top).and_then(|()| create());
        }
        match res {
            Ok(()) => {
                if let Some(p) = plan.take() {
                    self.journal_commit(src, p);
                }
                true
            }
            Err(e) => {
                let kind = if symbolic { "symbolic" } else { "hard" };
                eprintln!(
                    "cp: cannot create {kind} link {} to {}: {}",
                    qa(dst),
                    qa(src),
                    gnu_io_error(&e)
                );
                false
            }
        }
    }

    fn copy_dir(
        &mut self,
        src: &Path,
        dst: &Path,
        src_meta: &fs::Metadata,
        dst_existed: bool,
        top: bool,
    ) -> bool {
        let mut plan = None;
        let created = !dst_existed;
        if created {
            if let Err(e) = fs::DirBuilder::new().mode(0o700).create(dst) {
                eprintln!(
                    "cp: cannot create directory {}: {}",
                    qa(dst),
                    gnu_io_error(&e)
                );
                return false;
            }
            if self.o.verbose {
                println!("{} -> {}", qa(src), qa(dst));
            }
            if top {
                plan = Some(UndoPlan {
                    created: dst.to_path_buf(),
                    backup: None,
                });
            }
        } else if top {
            // Merging into an existing tree: keep a copy of it for `dogfish undo`.
            match self.journal_prepare(dst, true) {
                Ok(p) => plan = Some(p),
                Err(()) => return false,
            }
        }

        let mut ok = true;
        let descend = !(self.o.one_file_system && !top && src_meta.dev() != self.top_dev);
        if descend {
            match dir_names(src, true) {
                Ok(names) => {
                    for name in names {
                        let (s, d) = (src.join(&name), dst.join(&name));
                        match self.src_metadata(&s, false) {
                            Ok(m) => ok &= self.copy_entry(&s, &d, &m, created, false),
                            Err(e) => {
                                eprintln!("cp: cannot stat {}: {}", qa(&s), gnu_io_error(&e));
                                ok = false;
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("cp: cannot access {}: {}", qa(src), gnu_io_error(&e));
                    ok = false;
                }
            }
        }

        if created || self.o.preserve_mode {
            let mode = if self.o.preserve_mode {
                src_meta.mode() & CHMOD_MODE_BITS
            } else if self.o.no_preserve_mode {
                S_IRWXUGO & !self.umask
            } else {
                src_meta.mode() & (S_IRWXUGO | S_ISVTX) & !self.umask
            };
            if let Err(e) = fs::set_permissions(dst, fs::Permissions::from_mode(mode)) {
                eprintln!(
                    "cp: setting permissions for {}: {}",
                    qa(dst),
                    gnu_io_error(&e)
                );
                ok = false;
            }
        }
        self.preserve_attrs(dst, src_meta, false);
        if let Some(p) = plan {
            // Recorded even after a partial failure, so undo can restore the tree.
            self.journal_commit(src, p);
        }
        ok
    }

    fn copy_file(
        &self,
        src: &Path,
        dst: &Path,
        src_meta: &fs::Metadata,
        dst_exists: bool,
        top: bool,
        mut plan: Option<UndoPlan>,
    ) -> bool {
        use std::os::unix::fs::FileTypeExt;
        if !src_meta.is_file() && !(src_meta.file_type().is_fifo() && !self.o.recursive) {
            return self.copy_special(dst, src_meta, dst_exists);
        }
        let mut input = match fs::File::open(src) {
            Ok(f) => f,
            Err(e) => {
                eprintln!(
                    "cp: cannot open {} for reading: {}",
                    qa(src),
                    gnu_io_error(&e)
                );
                return false;
            }
        };
        // Writing through a symlink changes its referent: that is what to back up.
        let dangling = dst_exists
            && fs::symlink_metadata(dst).is_ok_and(|m| m.is_symlink())
            && fs::metadata(dst).is_err();
        let mut existing = dst_exists;
        if dangling {
            if !self.o.force {
                eprintln!("cp: not writing through dangling symlink {}", qa(dst));
                return false;
            }
            if let Err(e) = self.remove_existing(dst, &mut plan, top) {
                eprintln!("cp: cannot remove {}: {}", qa(dst), gnu_io_error(&e));
                return false;
            }
            existing = false;
        }
        if top && existing && plan.is_none() {
            let real = fs::canonicalize(dst).unwrap_or_else(|_| dst.to_path_buf());
            match self.journal_prepare(&real, true) {
                Ok(p) => plan = Some(p),
                Err(()) => return false,
            }
        }

        // APFS: a new file can be a zero-copy clone.
        #[cfg(target_os = "macos")]
        if !existing && !self.o.attributes_only && clone_new_file(src, dst) {
            if !self.o.preserve_timestamps {
                // clonefile(2) copies the source's times, but a copy is a new file.
                let _ = set_file_times(dst, Stamp::Now, Stamp::Now, false);
            }
            let len = src_meta.len();
            return self.finish_copy(
                src,
                dst,
                src_meta,
                (top, existing),
                plan,
                (len, CopyMethod::ClonefileCoW),
            );
        }

        let opened = if existing {
            let truncate = !self.o.attributes_only;
            match fs::OpenOptions::new()
                .write(true)
                .truncate(truncate)
                .open(dst)
            {
                Ok(f) => Ok(f),
                Err(e) if self.o.force => match self.remove_existing(dst, &mut plan, top) {
                    Ok(()) => {
                        if self.o.verbose {
                            println!("removed {}", qa(dst));
                        }
                        existing = false;
                        self.create_new(dst, src_meta)
                    }
                    Err(_) => Err(format!(
                        "cannot create regular file {}: {}",
                        qa(dst),
                        gnu_io_error(&e)
                    )),
                },
                Err(e) => Err(format!(
                    "cannot create regular file {}: {}",
                    qa(dst),
                    gnu_io_error(&e)
                )),
            }
        } else {
            self.create_new(dst, src_meta)
        };
        let output = match opened {
            Ok(f) => f,
            Err(msg) => {
                eprintln!("cp: {msg}");
                return false;
            }
        };
        let copied = if self.o.attributes_only {
            Ok((0, CopyMethod::StandardCopy))
        } else {
            copy_data(&mut input, &output)
        };
        match copied {
            Ok(stats) => {
                drop(output);
                self.finish_copy(src, dst, src_meta, (top, existing), plan, stats)
            }
            Err(e) => {
                eprintln!(
                    "cp: error copying {} to {}: {}",
                    qa(src),
                    qa(dst),
                    gnu_io_error(&e)
                );
                drop(output);
                // Put the saved original back so a failed copy never loses it.
                if let Some(UndoPlan {
                    created,
                    backup: Some(b),
                }) = &plan
                {
                    let _ = fs::remove_file(created);
                    let _ = move_path(Path::new(b), created);
                }
                false
            }
        }
    }

    /// After the data of a regular file is in place: telemetry, permissions,
    /// preserved attributes and the undo journal. `(top, existing)` says whether
    /// `dst` is a command-line operand and whether it existed before.
    fn finish_copy(
        &self,
        src: &Path,
        dst: &Path,
        src_meta: &fs::Metadata,
        (top, existing): (bool, bool),
        plan: Option<UndoPlan>,
        (bytes, method): (u64, CopyMethod),
    ) -> bool {
        let t = &self.ctx.telemetry;
        t.set_strategy(method.as_str());
        t.add_read(bytes);
        t.add_written(bytes);
        t.add_items(1);
        if self.o.verbose && self.ctx.stdout_is_tty {
            // Dogfish extra, only on a terminal: how the data was copied.
            println!("{} -> {} ({})", qa(src), qa(dst), method.as_str());
        }
        if !existing && !self.o.preserve_mode {
            // New files get the source's permissions minus the umask (as with open(2)).
            let base = if self.o.no_preserve_mode {
                0o666
            } else {
                src_meta.mode() & S_IRWXUGO
            };
            let _ = fs::set_permissions(dst, fs::Permissions::from_mode(base & !self.umask));
        }
        self.preserve_attrs(dst, src_meta, false);
        let plan = plan.or_else(|| {
            top.then(|| UndoPlan {
                created: dst.to_path_buf(),
                backup: None,
            })
        });
        if let Some(p) = plan {
            self.journal_commit(src, p);
        }
        true
    }

    fn create_new(
        &self,
        dst: &Path,
        src_meta: &fs::Metadata,
    ) -> std::result::Result<fs::File, String> {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(src_meta.mode() & S_IRWXUGO)
            .open(dst)
            .map_err(|e| {
                format!(
                    "cannot create regular file {}: {}",
                    qa(dst),
                    gnu_io_error(&e)
                )
            })
    }

    /// FIFOs (with -R); other special files are not supported.
    fn copy_special(&self, dst: &Path, src_meta: &fs::Metadata, dst_exists: bool) -> bool {
        use std::os::unix::fs::FileTypeExt;
        if dst_exists && let Err(e) = fs::remove_file(dst) {
            eprintln!("cp: cannot remove {}: {}", qa(dst), gnu_io_error(&e));
            return false;
        }
        let res = if src_meta.file_type().is_fifo() {
            c_path(dst).and_then(|c| {
                let mode = u32_to_mode_t(src_meta.mode() & S_IRWXUGO);
                if unsafe { libc::mkfifo(c.as_ptr(), mode) } == 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            })
        } else {
            Err(io::Error::from_raw_os_error(libc::ENOTSUP))
        };
        match res {
            Ok(()) => {
                self.preserve_attrs(dst, src_meta, false);
                true
            }
            Err(e) => {
                let what = if src_meta.file_type().is_fifo() {
                    "fifo"
                } else {
                    "special file"
                };
                eprintln!("cp: cannot create {what} {}: {}", qa(dst), gnu_io_error(&e));
                false
            }
        }
    }

    /// Applies `-p` / `--preserve` attributes (ownership, then mode, then times).
    fn preserve_attrs(&self, dst: &Path, src_meta: &fs::Metadata, is_link: bool) {
        if self.o.preserve_ownership {
            // Like GNU, failing to change ownership as a regular user is not an error.
            let _ = std::os::unix::fs::lchown(dst, Some(src_meta.uid()), Some(src_meta.gid()));
        }
        if self.o.preserve_mode && !is_link {
            let _ = fs::set_permissions(
                dst,
                fs::Permissions::from_mode(src_meta.mode() & CHMOD_MODE_BITS),
            );
        }
        if self.o.preserve_timestamps {
            let ns = |n: i64| u32::try_from(n).unwrap_or(0);
            let _ = set_file_times(
                dst,
                Stamp::At(src_meta.atime(), ns(src_meta.atime_nsec())),
                Stamp::At(src_meta.mtime(), ns(src_meta.mtime_nsec())),
                false,
            );
        }
    }
}

/// `clonefile(2)`: an instant copy-on-write clone on APFS (`dst` must not exist).
#[cfg(target_os = "macos")]
fn clone_new_file(src: &Path, dst: &Path) -> bool {
    match (c_path(src), c_path(dst)) {
        (Ok(s), Ok(d)) => unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) == 0 },
        _ => false,
    }
}

/// Copies all of `src` into the empty `dst`, as a CoW reflink where the file
/// system supports it.
fn copy_data(src: &mut fs::File, dst: &fs::File) -> io::Result<(u64, CopyMethod)> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        // FICLONE: instant copy-on-write on Btrfs, XFS, bcachefs.
        const FICLONE: libc::c_ulong = 0x40049409;
        if unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE as _, src.as_raw_fd()) } == 0 {
            return Ok((src.metadata()?.len(), CopyMethod::FicloneCoW));
        }
    }
    // std specializes File-to-File `io::copy` to copy_file_range(2) / fcopyfile(3).
    let bytes = io::copy(src, &mut &*dst)?;
    Ok((bytes, CopyMethod::StandardCopy))
}

/// Absolute form of `target` with its parent resolved through symlinks, for
/// "is this inside that directory?" checks on paths that may not exist yet.
fn resolve_for_containment(target: &Path) -> PathBuf {
    let parent = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    match (fs::canonicalize(parent), target.file_name()) {
        (Ok(p), Some(name)) => p.join(name),
        _ => std::path::absolute(target).unwrap_or_else(|_| target.to_path_buf()),
    }
}

// ---------------------------------------------------------------------------
// 9. mv — GNU semantics, recorded to the UndoJournal (`dogfish undo` reverses it)
// ---------------------------------------------------------------------------
const MV_HELP: &str = "\
Usage: mv [OPTION]... [-T] SOURCE DEST
  or:  mv [OPTION]... SOURCE... DIRECTORY
  or:  mv [OPTION]... -t DIRECTORY SOURCE...
Rename SOURCE to DEST, or move SOURCE(s) to DIRECTORY.
Moves are recorded so that `dogfish undo` can reverse them.

      --backup[=CONTROL]       make a backup of each existing destination file
  -b                           like --backup but does not accept an argument
  -f, --force                  do not prompt before overwriting
  -i, --interactive            prompt before overwrite
  -n, --no-clobber             do not overwrite an existing file
If you specify more than one of -i, -f, -n, only the final one takes effect.
      --strip-trailing-slashes  remove any trailing slashes from each SOURCE
                                 argument
  -S, --suffix=SUFFIX          override the usual backup suffix
  -t, --target-directory=DIRECTORY  move all SOURCE arguments into DIRECTORY
  -T, --no-target-directory    treat DEST as a normal file
      --update[=UPDATE]        control which existing files are updated;
                                 UPDATE={all,none,older(default)}
  -u                           equivalent to --update[=older]
  -v, --verbose                explain what is being done
      --help        display this help and exit
      --version     output version information and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum MvOpt {
    Backup,
    BackupArg,
    Force,
    Interactive,
    NoClobber,
    NoCopy,
    StripSlashes,
    Suffix,
    TargetDir,
    NoTargetDir,
    UpdateOlder,
    Update,
    Verbose,
    Context,
}

struct MvOpts {
    clobber: Clobber,
    update_older: bool,
    backup: BackupMode,
    suffix: String,
    verbose: bool,
    stdin_tty: bool,
}

pub fn run_mv(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    use MvOpt::*;
    const SPECS: &[ToolOpt<MvOpt>] = &[
        opt("b", "", HasArg::No, Backup),
        opt("", "backup", HasArg::Optional, BackupArg),
        opt("f", "force", HasArg::No, Force),
        opt("i", "interactive", HasArg::No, Interactive),
        opt("n", "no-clobber", HasArg::No, NoClobber),
        opt("", "no-copy", HasArg::No, NoCopy),
        opt("", "strip-trailing-slashes", HasArg::No, StripSlashes),
        opt("S", "suffix", HasArg::Required, Suffix),
        opt("t", "target-directory", HasArg::Required, TargetDir),
        opt("T", "no-target-directory", HasArg::No, NoTargetDir),
        opt("u", "", HasArg::No, UpdateOlder),
        opt("", "update", HasArg::Optional, Update),
        opt("v", "verbose", HasArg::No, Verbose),
        opt("Z", "", HasArg::No, Context),
        opt("", "context", HasArg::No, Context),
    ];
    let parsed = match parse_tool_args("mv", &args, SPECS, MV_HELP) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let mut o = MvOpts {
        clobber: Clobber::Yes,
        update_older: false,
        backup: BackupMode::None,
        suffix: String::new(),
        verbose: false,
        stdin_tty: io::stdin().is_terminal(),
    };
    let (mut want_backup, mut backup_arg, mut suffix) = (false, None, None);
    let (mut target_dir, mut no_target_dir, mut strip_slashes) = (None, false, false);
    for (opt, v) in parsed.opts {
        match opt {
            Backup => want_backup = true,
            BackupArg => {
                want_backup = true;
                backup_arg = v;
            }
            Force => o.clobber = Clobber::Yes,
            Interactive => o.clobber = Clobber::Ask,
            NoClobber => o.clobber = Clobber::No,
            NoCopy | Context => {}
            StripSlashes => strip_slashes = true,
            Suffix => {
                want_backup = true;
                suffix = v;
            }
            TargetDir => target_dir = v,
            NoTargetDir => no_target_dir = true,
            UpdateOlder => o.update_older = true,
            Update => {
                const UPDATE: &[(&str, u8)] = &[("all", 0), ("none", 1), ("older", 2)];
                let which = match v {
                    None => 2,
                    Some(w) => match argmatch("mv", "--update", &w, UPDATE) {
                        Ok(u) => u,
                        Err(code) => return Ok(code),
                    },
                };
                o.update_older = which == 2;
                if which == 1 && o.clobber != Clobber::No {
                    o.clobber = Clobber::Skip;
                }
            }
            Verbose => o.verbose = true,
        }
    }
    if want_backup {
        o.backup = match backup_mode("mv", backup_arg.as_deref()) {
            Ok(m) => m,
            Err(code) => return Ok(code),
        };
    }
    o.suffix = backup_suffix(suffix);

    let targets = match resolve_targets("mv", parsed.operands, target_dir, no_target_dir) {
        Ok(t) => t,
        Err(code) => return Ok(code),
    };
    if want_backup && o.clobber == Clobber::No {
        return Ok(usage_error(
            "mv",
            "options --backup and --no-clobber are mutually exclusive",
        ));
    }
    let journal = UndoJournal::open_for_recording();
    let strip = |s: &str| {
        if strip_slashes {
            strip_trailing_slashes(s)
        } else {
            s.to_string()
        }
    };
    let pairs: Vec<(String, PathBuf)> = match targets {
        Targets::Into(dir, sources) => sources
            .iter()
            .map(|s| {
                let s = strip(s);
                let d = concat_path(&dir, &operand_base(&s));
                (s, d)
            })
            .collect(),
        Targets::Single(s, d) => vec![(strip(&s), PathBuf::from(d))],
    };
    let mut ok = true;
    for (src, dst) in &pairs {
        ok &= mv_one(Path::new(src), dst, &o, journal.as_ref(), ctx);
    }
    Ok(if ok { 0 } else { 1 })
}

fn mv_one(
    src: &Path,
    dst: &Path,
    o: &MvOpts,
    journal: Option<&UndoJournal>,
    ctx: &DogfishContext,
) -> bool {
    let src_meta = match src.symlink_metadata() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("mv: cannot stat {}: {}", qa(src), gnu_io_error(&e));
            return false;
        }
    };
    let mut backup_made = None;
    let dst_meta = match dst.symlink_metadata() {
        Ok(m) => Some(m),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            eprintln!("mv: cannot stat {}: {}", qa(dst), gnu_io_error(&e));
            return false;
        }
    };
    // GNU's order: the no-clobber and update checks come first, then same-file
    // and prompts; type conflicts only matter when no backup is made.
    if let Some(dm) = &dst_meta {
        match o.clobber {
            Clobber::No => {
                eprintln!("mv: not replacing {}", qa(dst));
                return false;
            }
            Clobber::Skip => return true,
            _ => {}
        }
        if same_inode(&src_meta, dm) {
            eprintln!("mv: {} and {} are the same file", qa(src), qa(dst));
            return false;
        }
        // -u only ever skips non-directories.
        if o.update_older
            && !src_meta.is_dir()
            && (dm.mtime(), dm.mtime_nsec()) >= (src_meta.mtime(), src_meta.mtime_nsec())
        {
            return true;
        }
        let question = match o.clobber {
            Clobber::Ask => Some(format!("overwrite {}? ", qa(dst))),
            _ if o.stdin_tty && !dm.is_symlink() && write_protected(dst) => {
                let m = dm.mode() & CHMOD_MODE_BITS;
                Some(format!(
                    "replace {}, overriding mode {m:04o} ({})? ",
                    qa(dst),
                    strmode9(m)
                ))
            }
            _ => None,
        };
        if let Some(q) = question
            && !ask("mv", &q)
        {
            return false;
        }
        if o.backup == BackupMode::None {
            if src_meta.is_dir() && !dm.is_dir() {
                eprintln!(
                    "mv: cannot overwrite non-directory {} with directory {}",
                    qa(dst),
                    qa(src)
                );
                return false;
            }
            if !src_meta.is_dir() && dm.is_dir() {
                eprintln!(
                    "mv: cannot overwrite directory {} with non-directory",
                    qa(dst)
                );
                return false;
            }
        }
    }
    if src_meta.is_dir()
        && let Ok(abs_src) = fs::canonicalize(src)
        && resolve_for_containment(dst).starts_with(&abs_src)
    {
        eprintln!(
            "mv: cannot move {} to a subdirectory of itself, {}",
            qa(src),
            qa(dst)
        );
        return false;
    }
    if let Some(dm) = &dst_meta {
        if o.backup != BackupMode::None {
            match make_backup(dst, o.backup, &o.suffix, journal, "mv") {
                Ok(b) => backup_made = Some(b),
                Err(e) => {
                    eprintln!("mv: cannot backup {}: {}", qa(dst), gnu_io_error(&e));
                    return false;
                }
            }
        } else if dm.is_dir() && fs::read_dir(dst).is_ok_and(|mut d| d.next().is_some()) {
            // Checked up front: the journal moves the old target aside before the
            // rename, which must never make a non-empty directory replaceable.
            eprintln!("mv: cannot overwrite {}: Directory not empty", qa(dst));
            return false;
        }
    }

    let abs_src = absolute_lossy(src);
    // Move an existing target aside first so `dogfish undo` can restore it.
    let saved = match journal {
        Some(j) => match j.backup_existing(dst, false) {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "mv: cannot back up {} before overwriting: {}",
                    qa(dst),
                    gnu_io_error(&e)
                );
                return false;
            }
        },
        None => None,
    };
    if let Err(e) = move_path(src, dst) {
        if let Some(b) = &saved {
            let _ = move_path(Path::new(b), dst);
        }
        eprintln!(
            "mv: cannot move {} to {}: {}",
            qa(src),
            qa(dst),
            gnu_io_error(&e)
        );
        return false;
    }
    if let Some(j) = journal {
        let _ = j.record(
            "mv",
            JournalAction::Move {
                src: abs_src,
                dst: absolute_lossy(dst),
                backup_path: saved,
            },
        );
    }
    ctx.telemetry.add_items(1);
    if o.verbose {
        match &backup_made {
            Some(b) => println!("renamed {} -> {} (backup: {})", qa(src), qa(dst), qa(b)),
            None => println!("renamed {} -> {}", qa(src), qa(dst)),
        }
    }
    true
}

// ---------------------------------------------------------------------------
// 10. df — Sparkline table on a TTY, GNU `df` output otherwise
// ---------------------------------------------------------------------------
const DF_OPTIONS: &[OptSpec] = &[
    OptSpec::new("all", Some('a'), Some("all"), OptArg::No),
    OptSpec::new(
        "block-size",
        Some('B'),
        Some("block-size"),
        OptArg::Required,
    ),
    OptSpec::new(
        "human-readable",
        Some('h'),
        Some("human-readable"),
        OptArg::No,
    ),
    OptSpec::new("si", Some('H'), Some("si"), OptArg::No),
    OptSpec::new("inodes", Some('i'), Some("inodes"), OptArg::No),
    OptSpec::new("kilobytes", Some('k'), None, OptArg::No),
    OptSpec::new("megabytes", Some('m'), None, OptArg::No),
    OptSpec::new("type", Some('F'), None, OptArg::Required),
    OptSpec::new("local", Some('l'), Some("local"), OptArg::No),
    OptSpec::new("no-sync", None, Some("no-sync"), OptArg::No),
    OptSpec::new("output", None, Some("output"), OptArg::Optional),
    OptSpec::new("portability", Some('P'), Some("portability"), OptArg::No),
    OptSpec::new("sync", None, Some("sync"), OptArg::No),
    OptSpec::new("total", None, Some("total"), OptArg::No),
    OptSpec::new("type", Some('t'), Some("type"), OptArg::Required),
    OptSpec::new("print-type", Some('T'), Some("print-type"), OptArg::No),
    OptSpec::new(
        "exclude-type",
        Some('x'),
        Some("exclude-type"),
        OptArg::Required,
    ),
    OptSpec::new("ignored", Some('v'), None, OptArg::No),
    OptSpec::new("json", Some('J'), Some("json"), OptArg::No),
    OptSpec::new("help", None, Some("help"), OptArg::No),
    OptSpec::new("version", None, Some("version"), OptArg::No),
];

const DF_HELP: &str = "\
Usage: df [OPTION]... [FILE]...
Show information about the file system on which each FILE resides,
or all file systems by default.

  -a, --all             include pseudo, duplicate, inaccessible file systems
  -B, --block-size=SIZE  scale sizes by SIZE before printing them
  -h, --human-readable  print sizes in powers of 1024 (e.g., 1023M)
  -H, --si              print sizes in powers of 1000 (e.g., 1.1G)
  -i, --inodes          list inode information instead of block usage
  -k                    like --block-size=1K
  -l, --local           limit listing to local file systems
      --output[=FIELD_LIST]  use the output format defined by FIELD_LIST,
                               or print all fields if FIELD_LIST is omitted
  -P, --portability     use the POSIX output format
      --total           elide all entries insignificant to available space,
                          and produce a grand total
  -t, --type=TYPE       limit listing to file systems of type TYPE
  -T, --print-type      print file system type
  -x, --exclude-type=TYPE   limit listing to file systems not of type TYPE
  -J, --json            emit NDJSON records (Dogfish extension)
      --help            display this help and exit
      --version         output version information and exit

FIELD_LIST is a comma-separated list of columns to be included.  Valid
field names are: 'source', 'fstype', 'itotal', 'iused', 'iavail', 'ipcent',
'size', 'used', 'avail', 'pcent', 'file' and 'target'.
On a terminal Dogfish draws usage bars unless -P, -i or --output is given.
";

#[derive(Serialize)]
struct DfRecord {
    filesystem: String,
    fs_type: String,
    total_bytes: u64,
    used_bytes: u64,
    available_bytes: u64,
    use_percent: f64,
    mount_point: String,
}

/// The columns `df` can print, in `--output` order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DfField {
    Source,
    FsType,
    ITotal,
    IUsed,
    IAvail,
    IPcent,
    Size,
    Used,
    Avail,
    Pcent,
    File,
    Target,
}

impl DfField {
    const ALL: [DfField; 12] = [
        Self::Source,
        Self::FsType,
        Self::ITotal,
        Self::IUsed,
        Self::IAvail,
        Self::IPcent,
        Self::Size,
        Self::Used,
        Self::Avail,
        Self::Pcent,
        Self::File,
        Self::Target,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::FsType => "fstype",
            Self::ITotal => "itotal",
            Self::IUsed => "iused",
            Self::IAvail => "iavail",
            Self::IPcent => "ipcent",
            Self::Size => "size",
            Self::Used => "used",
            Self::Avail => "avail",
            Self::Pcent => "pcent",
            Self::File => "file",
            Self::Target => "target",
        }
    }

    /// GNU's column title and minimum width.
    fn header(self) -> (&'static str, usize) {
        match self {
            Self::Source => ("Filesystem", 14),
            Self::FsType => ("Type", 4),
            Self::ITotal => ("Inodes", 5),
            Self::IUsed => ("IUsed", 5),
            Self::IAvail => ("IFree", 5),
            Self::IPcent => ("IUse%", 5),
            Self::Size => ("Size", 5),
            Self::Used => ("Used", 5),
            Self::Avail => ("Avail", 5),
            Self::Pcent => ("Use%", 4),
            Self::File => ("File", 4),
            Self::Target => ("Mounted on", 0),
        }
    }

    fn left_aligned(self) -> bool {
        matches!(
            self,
            Self::Source | Self::FsType | Self::File | Self::Target
        )
    }
}

/// One entry of the mount table (gnulib `struct mount_entry`).
#[derive(Clone)]
struct MountEntry {
    device: String,
    mount_dir: String,
    fs_type: String,
    /// Root of the mount within its file system (`/proc/self/mountinfo`).
    mount_root: Option<String>,
    dummy: bool,
    remote: bool,
    dev: Option<u64>,
}

impl MountEntry {
    fn new(device: String, mount_dir: String, fs_type: String) -> Self {
        let dummy = is_dummy_fs(&fs_type);
        let remote = is_remote_fs(&device, &fs_type);
        Self {
            device,
            mount_dir,
            fs_type,
            mount_root: None,
            dummy,
            remote,
            dev: None,
        }
    }
}

/// gnulib `ME_DUMMY` (mountlist.c): file systems hidden unless `-a`.
///
/// gnulib's list plus `devtmpfs` and `squashfs`, which Debian/Ubuntu's coreutils
/// patch in so snap mounts stay out of `df` (upstream GNU lists them). Every other
/// pseudo file system (cgroup2, tracefs, securityfs, ...) disappears from `df` only
/// because it reports zero blocks, which `df` checks separately; ones that do report
/// blocks, like efivarfs, are shown, as GNU shows them.
fn is_dummy_fs(fs_type: &str) -> bool {
    matches!(
        fs_type,
        "autofs"
            | "proc"
            | "subfs"
            | "debugfs"
            | "devpts"
            | "fusectl"
            | "fuse.portal"
            | "mqueue"
            | "rpc_pipefs"
            | "sysfs"
            | "devfs"
            | "kernfs"
            | "ignore"
            | "none"
            | "rootfs"
            | "devtmpfs"
            | "squashfs"
    )
}

/// gnulib `ME_REMOTE`: network file systems (excluded by `-l`).
fn is_remote_fs(device: &str, fs_type: &str) -> bool {
    device.contains(':')
        || (device.starts_with("//") && matches!(fs_type, "smbfs" | "smb3" | "cifs"))
        || matches!(
            fs_type,
            "acfs" | "afs" | "coda" | "auristorfs" | "fhgfs" | "gpfs" | "ibrix" | "ocfs2" | "vxfs"
        )
        || device == "-hosts"
}

/// Undoes the octal escapes (`\040`) the kernel uses in mount tables.
#[cfg(target_os = "linux")]
fn unescape_mount_field(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 4 <= b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(target_os = "linux")]
fn read_mount_table() -> Vec<MountEntry> {
    if let Ok(info) = fs::read_to_string("/proc/self/mountinfo") {
        let mut entries = Vec::new();
        for line in info.lines() {
            let fields: Vec<&str> = line.split(' ').collect();
            let Some(sep) = fields.iter().position(|f| *f == "-") else {
                continue;
            };
            if fields.len() < sep + 3 || sep < 5 {
                continue;
            }
            let dev = fields[2].split_once(':').and_then(|(ma, mi)| {
                let (ma, mi) = (ma.parse::<u64>().ok()?, mi.parse::<u64>().ok()?);
                Some(
                    ((ma & 0xfff) << 8)
                        | ((ma & !0xfff) << 32)
                        | (mi & 0xff)
                        | ((mi & !0xff) << 12),
                )
            });
            let mut entry = MountEntry::new(
                unescape_mount_field(fields[sep + 2]),
                unescape_mount_field(fields[4]),
                unescape_mount_field(fields[sep + 1]),
            );
            entry.mount_root = Some(unescape_mount_field(fields[3]));
            entry.dev = dev;
            entries.push(entry);
        }
        return entries;
    }
    let mounts = fs::read_to_string("/proc/mounts")
        .or_else(|_| fs::read_to_string("/etc/mtab"))
        .unwrap_or_default();
    mounts
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let (device, dir, fs_type) = (parts.next()?, parts.next()?, parts.next()?);
            Some(MountEntry::new(
                unescape_mount_field(device),
                unescape_mount_field(dir),
                unescape_mount_field(fs_type),
            ))
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn read_mount_table() -> Vec<MountEntry> {
    let mut mntbuf: *mut libc::statfs = std::ptr::null_mut();
    // SAFETY: getmntinfo points mntbuf at a static array of `count` entries.
    let count = unsafe { libc::getmntinfo(&mut mntbuf, libc::MNT_NOWAIT) };
    if count <= 0 || mntbuf.is_null() {
        return Vec::new();
    }
    let slice = unsafe { std::slice::from_raw_parts(mntbuf, count as usize) };
    let cstr =
        |ptr: *const libc::c_char| unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() };
    slice
        .iter()
        .map(|s| {
            let mut entry = MountEntry::new(
                cstr(s.f_mntfromname.as_ptr()),
                cstr(s.f_mntonname.as_ptr()),
                cstr(s.f_fstypename.as_ptr()),
            );
            entry.dummy |= entry.fs_type == "devfs" || entry.fs_type == "autofs";
            entry
        })
        .collect()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn read_mount_table() -> Vec<MountEntry> {
    Vec::new()
}

/// gnulib `fs_usage`, from `statvfs`.
#[derive(Clone, Copy, Default)]
struct FsUsage {
    block_size: u64,
    blocks: u64,
    free: u64,
    available: u64,
    files: u64,
    free_files: u64,
}

impl FsUsage {
    fn query(path: &str) -> io::Result<Self> {
        let c_path = CString::new(path).map_err(io::Error::other)?;
        // SAFETY: statvfs fills the zeroed struct for a valid C path.
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c_path.as_ptr(), &mut s) } != 0 {
            return Err(io::Error::last_os_error());
        }
        #[allow(clippy::unnecessary_cast)]
        let block_size = if s.f_frsize != 0 {
            s.f_frsize as u64
        } else {
            s.f_bsize as u64
        };
        #[allow(clippy::unnecessary_cast)]
        Ok(Self {
            block_size,
            blocks: s.f_blocks as u64,
            free: s.f_bfree as u64,
            available: s.f_bavail as u64,
            files: s.f_files as u64,
            free_files: s.f_ffree as u64,
        })
    }
}

/// Options that shape `df` output.
struct DfConfig {
    show_all: bool,
    local_only: bool,
    inodes: bool,
    human_opts: u32,
    block_size: u64,
    posix: bool,
    fields: Vec<DfField>,
    output_mode: bool,
    select_types: Vec<String>,
    exclude_types: Vec<String>,
}

impl DfConfig {
    fn type_selected(&self, fs_type: &str) -> bool {
        (self.select_types.is_empty() || self.select_types.iter().any(|t| t == fs_type))
            && !self.exclude_types.iter().any(|t| t == fs_type)
    }
}

/// One output row before formatting.
struct DfRow {
    source: String,
    fs_type: String,
    file: String,
    target: String,
    usage: FsUsage,
    /// Placeholder row: every number prints as `-`.
    unknown: bool,
}

pub fn run_df(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = match gnu_getopt(DF_OPTIONS, &args[1..]) {
        Ok(p) => p,
        Err(msg) => return Ok(gnu_usage_error("df", &msg, 1)),
    };
    let (mut human_opts, mut block_size) = env_block_size("DF_BLOCK_SIZE");
    let mut cfg = DfConfig {
        show_all: false,
        local_only: false,
        inodes: false,
        human_opts: 0,
        block_size: 0,
        posix: false,
        fields: Vec::new(),
        output_mode: false,
        select_types: Vec::new(),
        exclude_types: Vec::new(),
    };
    let mut explicit_size = false;
    let mut print_type = false;
    let mut total = false;
    let mut json = false;
    for opt in parsed.opts {
        match opt.key {
            "all" => cfg.show_all = true,
            "block-size" => {
                let value = opt.value.unwrap_or_default();
                match parse_block_size(&value) {
                    Ok((o, b)) => (human_opts, block_size) = (o, b),
                    Err(e) => {
                        eprintln!("df: {}", e.message(&opt.spelled, &value));
                        return Ok(1);
                    }
                }
                explicit_size = true;
            }
            "human-readable" => {
                (human_opts, block_size) = (HUMAN_AUTOSCALE | HUMAN_SI | HUMAN_BASE_1024, 1);
                explicit_size = true;
            }
            "si" => {
                (human_opts, block_size) = (HUMAN_AUTOSCALE | HUMAN_SI, 1);
                explicit_size = true;
            }
            "inodes" => {
                if cfg.output_mode {
                    return Ok(df_output_conflict("-i"));
                }
                cfg.inodes = true;
            }
            "kilobytes" | "megabytes" => {
                let size = if opt.key == "kilobytes" {
                    1024
                } else {
                    1024 * 1024
                };
                (human_opts, block_size) = (0, size);
                explicit_size = true;
            }
            "local" => cfg.local_only = true,
            "portability" => {
                if cfg.output_mode {
                    return Ok(df_output_conflict("-P"));
                }
                cfg.posix = true;
            }
            "print-type" => {
                if cfg.output_mode {
                    return Ok(df_output_conflict("-T"));
                }
                print_type = true;
            }
            "output" => {
                if cfg.inodes {
                    return Ok(df_output_conflict("-i"));
                }
                if cfg.posix {
                    return Ok(df_output_conflict("-P"));
                }
                if print_type {
                    return Ok(df_output_conflict("-T"));
                }
                let spec = opt
                    .value
                    .unwrap_or_else(|| DfField::ALL.map(DfField::name).join(","));
                for name in spec.split(',') {
                    let Some(field) = DfField::ALL.iter().find(|f| f.name() == name) else {
                        let msg = format!(
                            "option --output: field {} unknown",
                            quote_bytes(name.as_bytes(), Quoting::Locale)
                        );
                        return Ok(gnu_usage_error("df", &msg, 1));
                    };
                    if cfg.fields.contains(field) {
                        let msg = format!(
                            "option --output: field {} used more than once",
                            quote_bytes(name.as_bytes(), Quoting::Locale)
                        );
                        return Ok(gnu_usage_error("df", &msg, 1));
                    }
                    cfg.fields.push(*field);
                }
                cfg.output_mode = true;
            }
            "total" => total = true,
            "type" => cfg.select_types.push(opt.value.unwrap_or_default()),
            "exclude-type" => cfg.exclude_types.push(opt.value.unwrap_or_default()),
            "json" => json = true,
            "sync" | "no-sync" | "ignored" => {}
            "help" => return print_help_text(DF_HELP),
            "version" => {
                print_tool_version("df");
                return Ok(0);
            }
            _ => unreachable!("unhandled df option"),
        }
    }
    for t in &cfg.select_types {
        if cfg.exclude_types.contains(t) {
            eprintln!(
                "df: file system type {} both selected and excluded",
                quote_bytes(t.as_bytes(), Quoting::Locale)
            );
            return Ok(1);
        }
    }
    // -P ignores DF_BLOCK_SIZE & co.: POSIX blocks unless sizes were requested.
    if cfg.posix && !explicit_size {
        human_opts = 0;
        block_size = if std::env::var_os("POSIXLY_CORRECT").is_some() {
            512
        } else {
            1024
        };
    }
    cfg.human_opts = human_opts;
    cfg.block_size = block_size;
    if !cfg.output_mode {
        cfg.fields = df_default_fields(&cfg, print_type);
    }

    let mounts = read_mount_table();
    let mut status = 0;
    let mut rows = Vec::new();
    if parsed.operands.is_empty() {
        df_all_rows(&mounts, &cfg, &mut rows, &mut status);
    } else {
        for name in &parsed.operands {
            df_file_row(name, &mounts, &cfg, &mut rows, &mut status);
        }
    }

    if rows.is_empty() {
        if status == 0 {
            eprintln!("df: no file systems processed");
        }
        return Ok(1);
    }

    if total {
        let mut sum = FsUsage {
            block_size: 1,
            ..FsUsage::default()
        };
        for r in rows.iter().filter(|r| !r.unknown) {
            let u = &r.usage;
            sum.blocks += u.blocks * u.block_size;
            sum.free += u.free * u.block_size;
            sum.available += u.available * u.block_size;
            sum.files += u.files;
            sum.free_files += u.free_files;
        }
        rows.push(DfRow {
            source: "total".into(),
            fs_type: "-".into(),
            file: "-".into(),
            target: "-".into(),
            usage: sum,
            unknown: false,
        });
    }

    if json {
        let records: Vec<DfRecord> = rows.iter().map(df_record).collect();
        emit_objects_or_table(&records, ctx, true, false)?;
        return Ok(status);
    }
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    if ctx.stdout_is_tty && !cfg.posix && !cfg.inodes && !cfg.output_mode {
        df_render_rich(&rows, &cfg, &mut out, ctx.style())?;
    } else {
        df_render_gnu(&rows, &cfg, &mut out)?;
    }
    ignore_broken_pipe(out.flush())?;
    Ok(status)
}

fn df_output_conflict(option: &str) -> i32 {
    let msg = format!("options {option} and --output are mutually exclusive");
    gnu_usage_error("df", &msg, 1)
}

/// The columns of each GNU header mode (`get_field_list`).
fn df_default_fields(cfg: &DfConfig, print_type: bool) -> Vec<DfField> {
    let mut fields = vec![DfField::Source];
    if print_type {
        fields.push(DfField::FsType);
    }
    if cfg.inodes {
        fields.extend([
            DfField::ITotal,
            DfField::IUsed,
            DfField::IAvail,
            DfField::IPcent,
        ]);
    } else {
        fields.extend([DfField::Size, DfField::Used, DfField::Avail, DfField::Pcent]);
    }
    fields.push(DfField::Target);
    fields
}

/// All mounted file systems, filtered and de-duplicated like GNU
/// `filter_mount_list` + `get_dev`.
fn df_all_rows(mounts: &[MountEntry], cfg: &DfConfig, rows: &mut Vec<DfRow>, status: &mut i32) {
    let filtered = |me: &MountEntry| {
        (me.remote && cfg.local_only)
            || (me.dummy && !cfg.show_all)
            || !cfg.type_selected(&me.fs_type)
    };
    // (device number, index of the entry chosen for it), in first-seen order.
    // Filtered or unreachable entries keep their mount-table device number and
    // never displace anything, exactly like GNU's device list.
    let mut chosen: Vec<(u64, usize)> = Vec::new();
    for (idx, me) in mounts.iter().enumerate() {
        let stat_dev = if filtered(me) {
            None
        } else {
            fs::metadata(&me.mount_dir).ok().map(|m| m.dev())
        };
        let Some(dev) = stat_dev else {
            let dev = me.dev.unwrap_or(u64::MAX - idx as u64);
            chosen.push((dev, idx));
            continue;
        };
        if let Some(slot) = chosen.iter_mut().find(|(d, _)| *d == dev) {
            let seen = &mounts[slot.1];
            let target_nearer_root = seen.mount_dir.len() > me.mount_dir.len();
            let source_below_root = match (&seen.mount_root, &me.mount_root) {
                (Some(a), Some(b)) => a.len() < b.len(),
                _ => false,
            };
            let replace = (me.device.contains('/') && !seen.device.contains('/'))
                || (target_nearer_root && !source_below_root)
                || (seen.device != me.device && seen.mount_dir == me.mount_dir);
            let keep_remote_pair = me.remote && seen.remote && seen.device != me.device;
            if replace && !keep_remote_pair {
                slot.1 = idx;
            }
        } else {
            chosen.push((dev, idx));
        }
    }
    // `-a` lists every entry, duplicates included.
    let order: Vec<usize> = if cfg.show_all {
        (0..mounts.len()).collect()
    } else {
        chosen.iter().map(|(_, idx)| *idx).collect()
    };
    for idx in order {
        let me = &mounts[idx];
        if filtered(me) || !me.mount_dir.starts_with('/') {
            continue;
        }
        let (usage, mut unknown, mut fs_type) = match FsUsage::query(&me.mount_dir) {
            Ok(u) => (u, false, me.fs_type.clone()),
            Err(e) => {
                let benign = matches!(e.raw_os_error(), Some(libc::EACCES) | Some(libc::ENOENT));
                if benign {
                    if !cfg.show_all {
                        continue;
                    }
                    (FsUsage::default(), true, "-".to_string())
                } else {
                    eprintln!("df: {}: {}", quote_f(&me.mount_dir), gnu_io_error(&e));
                    *status = 1;
                    continue;
                }
            }
        };
        // With -a, an over-mounted duplicate of another device's entry shows
        // placeholders instead of repeating that file system's figures.
        if cfg.show_all
            && !unknown
            && let Ok(m) = fs::metadata(&me.mount_dir)
            && let Some((_, owner)) = chosen.iter().find(|(d, _)| *d == m.dev())
            && mounts[*owner].device != me.device
        {
            unknown = true;
            fs_type = "-".into();
        }
        if usage.blocks == 0 && !unknown && !cfg.show_all {
            continue;
        }
        rows.push(DfRow {
            source: me.device.clone(),
            fs_type,
            file: "-".into(),
            target: me.mount_dir.clone(),
            usage,
            unknown,
        });
    }
}

/// The row for the file system containing `name` (GNU `get_entry`/`get_point`).
fn df_file_row(
    name: &str,
    mounts: &[MountEntry],
    cfg: &DfConfig,
    rows: &mut Vec<DfRow>,
    status: &mut i32,
) {
    let meta = match fs::metadata(name) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("df: {}: {}", quote_f(name), gnu_io_error(&e));
            *status = 1;
            return;
        }
    };
    let mut best: Option<&MountEntry> = None;

    // A mounted block device names its own file system.
    {
        use std::os::unix::fs::FileTypeExt;
        if meta.file_type().is_block_device()
            && let Ok(real) = fs::canonicalize(name)
        {
            best = mounts
                .iter()
                .rev()
                .find(|me| fs::canonicalize(&me.device).is_ok_and(|d| d == real));
        }
    }
    if best.is_none()
        && let Ok(resolved) = fs::canonicalize(name)
    {
        let resolved = resolved.to_string_lossy().into_owned();
        let mut best_len = 0;
        for me in mounts {
            if me.fs_type == "lofs" || !(best.is_none_or(|b| b.dummy) || !me.dummy) {
                continue;
            }
            let len = me.mount_dir.len();
            if best_len <= len
                && len <= resolved.len()
                && (len == 1
                    || ((len == resolved.len() || resolved.as_bytes()[len] == b'/')
                        && resolved.starts_with(&me.mount_dir)))
            {
                best = Some(me);
                best_len = len;
            }
        }
        if let Some(b) = best
            && fs::metadata(&b.mount_dir).map(|m| m.dev()).ok() != Some(meta.dev())
        {
            best = None;
        }
    }
    if best.is_none() {
        for me in mounts {
            if me.fs_type == "lofs" || !(best.is_none_or(|b| b.dummy) || !me.dummy) {
                continue;
            }
            if fs::metadata(&me.mount_dir).is_ok_and(|m| m.dev() == meta.dev()) {
                best = Some(me);
            }
        }
    }
    let (device, mount_dir, fs_type, remote) = match best {
        Some(me) => (
            me.device.clone(),
            me.mount_dir.clone(),
            me.fs_type.clone(),
            me.remote,
        ),
        None => {
            let mount_dir = find_mount_point(Path::new(name), &meta);
            (
                "-".to_string(),
                mount_dir.to_string_lossy().into_owned(),
                "-".to_string(),
                false,
            )
        }
    };
    if (remote && cfg.local_only) || !cfg.type_selected(&fs_type) {
        return;
    }
    match FsUsage::query(name) {
        Ok(usage) => rows.push(DfRow {
            source: device,
            fs_type,
            file: name.to_string(),
            target: mount_dir,
            usage,
            unknown: false,
        }),
        Err(e) => {
            eprintln!("df: {}: {}", quote_f(name), gnu_io_error(&e));
            *status = 1;
        }
    }
}

/// Block and inode figures of a row in the units GNU prints them.
struct DfFigures {
    total: u64,
    used: u64,
    used_negative: bool,
    available: u64,
    input_units: u64,
    output_units: u64,
}

fn df_figures(row: &DfRow, cfg: &DfConfig, inodes: bool) -> DfFigures {
    let u = &row.usage;
    let (total, available, to_root, input_units, output_units) = if inodes {
        (u.files, u.free_files, u.free_files, 1, 1)
    } else {
        (u.blocks, u.available, u.free, u.block_size, cfg.block_size)
    };
    DfFigures {
        total,
        used: total.abs_diff(to_root),
        used_negative: total < to_root,
        available,
        input_units,
        output_units,
    }
}

/// GNU's `Use%`: used / (used + available), rounded up.
fn df_percent(f: &DfFigures) -> Option<u64> {
    if f.used_negative {
        return None;
    }
    let denom = f.used.checked_add(f.available)?;
    if denom == 0 {
        return None;
    }
    let used100 = u128::from(f.used) * 100;
    let denom = u128::from(denom);
    Some((used100 / denom + u128::from(used100 % denom != 0)) as u64)
}

fn df_cell(row: &DfRow, cfg: &DfConfig, field: DfField) -> String {
    let inodes = matches!(
        field,
        DfField::ITotal | DfField::IUsed | DfField::IAvail | DfField::IPcent
    );
    let f = df_figures(row, cfg, inodes);
    let num = |n: u64, negative: bool| -> String {
        if row.unknown {
            return "-".into();
        }
        let s = human_readable(n, cfg.human_opts, f.input_units, f.output_units);
        if negative { format!("-{s}") } else { s }
    };
    match field {
        DfField::Source => row.source.clone(),
        DfField::FsType => row.fs_type.clone(),
        DfField::File => row.file.clone(),
        DfField::Target => row.target.clone(),
        DfField::ITotal | DfField::Size => num(f.total, false),
        DfField::IUsed | DfField::Used => num(f.used, f.used_negative),
        DfField::IAvail | DfField::Avail => num(f.available, false),
        DfField::IPcent | DfField::Pcent => match df_percent(&f).filter(|_| !row.unknown) {
            Some(p) => format!("{p}%"),
            None => "-".into(),
        },
    }
}

/// GNU's header cell for `field` (`get_header`).
fn df_header(cfg: &DfConfig, field: DfField) -> String {
    let human = cfg.human_opts & HUMAN_AUTOSCALE != 0;
    let posix_mode = cfg.posix && !human && !cfg.inodes && !cfg.output_mode;
    match field {
        DfField::Size if posix_mode => format!("{}-blocks", cfg.block_size),
        DfField::Size if !human => {
            let mut opts = HUMAN_SUPPRESS_POINT_ZERO
                | HUMAN_AUTOSCALE
                | HUMAN_SI
                | (cfg.human_opts & (HUMAN_BASE_1024 | HUMAN_B));
            let (mut q1000, mut q1024) = (cfg.block_size, cfg.block_size);
            let (mut div1000, mut div1024);
            loop {
                div1000 = q1000 % 1000 == 0;
                q1000 /= 1000;
                div1024 = q1024 % 1024 == 0;
                q1024 /= 1024;
                if !(div1000 && div1024) {
                    break;
                }
            }
            if !div1000 && div1024 {
                opts |= HUMAN_BASE_1024;
            }
            if div1000 && !div1024 {
                opts &= !HUMAN_BASE_1024;
            }
            if opts & HUMAN_BASE_1024 == 0 {
                opts |= HUMAN_B;
            }
            format!("{}-blocks", human_readable(cfg.block_size, opts, 1, 1))
        }
        DfField::Avail if !human && !cfg.output_mode => "Available".into(),
        DfField::Pcent if posix_mode => "Capacity".into(),
        _ => field.header().0.into(),
    }
}

fn df_render_gnu<W: Write>(rows: &[DfRow], cfg: &DfConfig, out: &mut W) -> io::Result<()> {
    let mut table: Vec<Vec<String>> = vec![cfg.fields.iter().map(|f| df_header(cfg, *f)).collect()];
    table.extend(
        rows.iter()
            .map(|r| cfg.fields.iter().map(|f| df_cell(r, cfg, *f)).collect()),
    );
    let widths: Vec<usize> = cfg
        .fields
        .iter()
        .enumerate()
        .map(|(col, f)| {
            table
                .iter()
                .map(|row| display_width(row[col].as_bytes()))
                .max()
                .unwrap_or(0)
                .max(f.header().1)
        })
        .collect();
    for row in &table {
        let mut line = String::new();
        for (col, cell) in row.iter().enumerate() {
            if col > 0 {
                line.push(' ');
            }
            let pad = widths[col].saturating_sub(display_width(cell.as_bytes()));
            let last = col + 1 == row.len();
            if cfg.fields[col].left_aligned() {
                line.push_str(cell);
                if !last {
                    line.push_str(&" ".repeat(pad));
                }
            } else {
                line.push_str(&" ".repeat(pad));
                line.push_str(cell);
            }
        }
        line.push('\n');
        ignore_broken_pipe(out.write_all(line.as_bytes()))?;
    }
    Ok(())
}

fn df_record(row: &DfRow) -> DfRecord {
    let u = &row.usage;
    let total = u.blocks.saturating_mul(u.block_size);
    let free = u.free.saturating_mul(u.block_size);
    let available = u.available.saturating_mul(u.block_size);
    let used = total.saturating_sub(free);
    let denom = used + available;
    let pct = if denom > 0 {
        used as f64 / denom as f64 * 100.0
    } else {
        0.0
    };
    DfRecord {
        filesystem: row.source.clone(),
        fs_type: row.fs_type.clone(),
        total_bytes: total,
        used_bytes: used,
        available_bytes: available,
        use_percent: (pct * 10.0).round() / 10.0,
        mount_point: row.target.clone(),
    }
}

/// The Dogfish sparkline table shown on a terminal.
fn df_render_rich<W: Write>(
    rows: &[DfRow],
    cfg: &DfConfig,
    out: &mut W,
    st: Style,
) -> io::Result<()> {
    let explicit_units = cfg.human_opts & HUMAN_AUTOSCALE == 0 && cfg.block_size != 1024;
    let size = |bytes: u64| {
        if explicit_units {
            human_readable(bytes, cfg.human_opts, 1, cfg.block_size)
        } else {
            format_size(bytes, true)
        }
    };
    let mut table = Table::new(vec![
        ("FILESYSTEM", ColumnAlign::Left),
        ("TYPE", ColumnAlign::Left),
        ("SIZE", ColumnAlign::Right),
        ("USED", ColumnAlign::Right),
        ("AVAIL", ColumnAlign::Right),
        ("USAGE", ColumnAlign::Left),
        ("MOUNTED ON", ColumnAlign::Left),
    ]);
    for m in rows.iter().map(df_record) {
        let ratio = (m.use_percent / 100.0).clamp(0.0, 1.0);
        let usage_col = format!("{} {:>5.1}%", sparkline_bar(ratio, 10, st), m.use_percent);
        table.add_row(vec![
            st.bold(&m.filesystem),
            st.dim(&m.fs_type),
            size(m.total_bytes),
            st.yellow(&size(m.used_bytes)),
            st.green(&size(m.available_bytes)),
            usage_col,
            st.bold_cyan(&m.mount_point),
        ]);
    }
    table.render(out, st)
}

// ---------------------------------------------------------------------------
// 11. du — Parallel disk usage with sparklines on a TTY, GNU `du` otherwise
// ---------------------------------------------------------------------------
const DU_OPTIONS: &[OptSpec] = &[
    OptSpec::new("null", Some('0'), Some("null"), OptArg::No),
    OptSpec::new("all", Some('a'), Some("all"), OptArg::No),
    OptSpec::new("apparent-size", None, Some("apparent-size"), OptArg::No),
    OptSpec::new(
        "block-size",
        Some('B'),
        Some("block-size"),
        OptArg::Required,
    ),
    OptSpec::new("bytes", Some('b'), Some("bytes"), OptArg::No),
    OptSpec::new("total", Some('c'), Some("total"), OptArg::No),
    OptSpec::new(
        "dereference-args",
        Some('D'),
        Some("dereference-args"),
        OptArg::No,
    ),
    OptSpec::new("dereference-args", Some('H'), None, OptArg::No),
    OptSpec::new("max-depth", Some('d'), Some("max-depth"), OptArg::Required),
    OptSpec::new(
        "human-readable",
        Some('h'),
        Some("human-readable"),
        OptArg::No,
    ),
    OptSpec::new("inodes", None, Some("inodes"), OptArg::No),
    OptSpec::new("kilobytes", Some('k'), None, OptArg::No),
    OptSpec::new("dereference", Some('L'), Some("dereference"), OptArg::No),
    OptSpec::new("count-links", Some('l'), Some("count-links"), OptArg::No),
    OptSpec::new("megabytes", Some('m'), None, OptArg::No),
    OptSpec::new(
        "no-dereference",
        Some('P'),
        Some("no-dereference"),
        OptArg::No,
    ),
    OptSpec::new(
        "separate-dirs",
        Some('S'),
        Some("separate-dirs"),
        OptArg::No,
    ),
    OptSpec::new("si", None, Some("si"), OptArg::No),
    OptSpec::new("summarize", Some('s'), Some("summarize"), OptArg::No),
    OptSpec::new("threshold", Some('t'), Some("threshold"), OptArg::Required),
    OptSpec::new("exclude", None, Some("exclude"), OptArg::Required),
    OptSpec::new(
        "exclude-from",
        Some('X'),
        Some("exclude-from"),
        OptArg::Required,
    ),
    OptSpec::new(
        "one-file-system",
        Some('x'),
        Some("one-file-system"),
        OptArg::No,
    ),
    OptSpec::new("json", Some('J'), Some("json"), OptArg::No),
    OptSpec::new("help", None, Some("help"), OptArg::No),
    OptSpec::new("version", None, Some("version"), OptArg::No),
];

const DU_HELP: &str = "\
Usage: du [OPTION]... [FILE]...
Summarize device usage of the set of FILEs, recursively for directories.

  -0, --null            end each output line with NUL, not newline
  -a, --all             write counts for all files, not just directories
      --apparent-size   print apparent sizes rather than device usage
  -B, --block-size=SIZE  scale sizes by SIZE before printing them
  -b, --bytes           equivalent to '--apparent-size --block-size=1'
  -c, --total           produce a grand total
  -D, --dereference-args  dereference only symlinks that are listed on the
                          command line (also -H)
  -d, --max-depth=N     print the total for a directory only if it is N or
                          fewer levels below the command line argument
  -h, --human-readable  print sizes in human readable format (e.g., 1K 234M 2G)
      --inodes          list inode usage information instead of block usage
  -k                    like --block-size=1K
  -L, --dereference     dereference all symbolic links
  -l, --count-links     count sizes many times if hard linked
  -m                    like --block-size=1M
  -P, --no-dereference  don't follow any symbolic links (this is the default)
  -S, --separate-dirs   for directories do not include size of subdirectories
      --si              like -h, but use powers of 1000 not 1024
  -s, --summarize       display only a total for each argument
  -t, --threshold=SIZE  exclude entries smaller than SIZE if positive,
                          or entries greater than SIZE if negative
  -X, --exclude-from=FILE  exclude files that match any pattern in FILE
      --exclude=PATTERN    exclude files that match PATTERN
  -x, --one-file-system    skip directories on different file systems
  -J, --json            emit NDJSON records (Dogfish extension)
      --help            display this help and exit
      --version         output version information and exit

Directory trees are read in parallel; output order matches GNU du. On a
terminal (without -a/-d/-s/-S) Dogfish shows the top level sorted by size
with usage bars.
";

#[derive(Serialize, Clone)]
struct DuRecord {
    path: String,
    size: u64,
    size_human: String,
    files: u64,
}

/// What `du` measures and how it walks (shared by the parallel collectors).
struct DuConfig {
    all: bool,
    apparent: bool,
    inodes: bool,
    count_links: bool,
    /// Directories are de-duplicated too (several operands, or `-L`).
    hash_all: bool,
    follow_all: bool,
    one_file_system: bool,
    excludes: Vec<Vec<u8>>,
}

impl DuConfig {
    fn size_of(&self, meta: &fs::Metadata) -> u64 {
        if self.inodes {
            1
        } else if self.apparent {
            let ft = meta.file_type();
            if ft.is_file() || ft.is_symlink() {
                meta.size()
            } else {
                0
            }
        } else {
            meta.blocks() * 512
        }
    }

    /// The (dev, ino) key under which a hard-linked entry is counted once.
    fn dedup_key(&self, meta: &fs::Metadata) -> Option<(u64, u64)> {
        let linked = if meta.is_dir() {
            self.hash_all
        } else {
            self.hash_all || meta.nlink() > 1
        };
        (linked && !self.count_links).then(|| (meta.dev(), meta.ino()))
    }

    /// GNU `exclude` matching without anchoring: the pattern may match the
    /// whole path or any part of it that starts after a `/`.
    fn excluded(&self, path: &[u8]) -> bool {
        self.excludes.iter().any(|pat| {
            fnmatch(pat, path, false)
                || path.iter().enumerate().any(|(i, c)| {
                    *c == b'/'
                        && path.get(i + 1) != Some(&b'/')
                        && fnmatch(pat, &path[i + 1..], false)
                })
        })
    }
}

/// A directory read during the parallel collection phase.
struct DuDir {
    meta: fs::Metadata,
    /// Combined size and count of entries that need neither printing nor
    /// de-duplication (single-link files without `-a`).
    plain_size: u64,
    plain_files: u64,
    entries: Vec<DuEntry>,
    read_error: Option<io::Error>,
}

impl DuDir {
    fn empty(meta: fs::Metadata) -> Self {
        Self {
            meta,
            plain_size: 0,
            plain_files: 0,
            entries: Vec::new(),
            read_error: None,
        }
    }
}

enum DuEntry {
    File {
        name: std::ffi::OsString,
        meta: fs::Metadata,
    },
    Dir {
        name: std::ffi::OsString,
        dir: Box<DuDir>,
    },
    /// Could not be stat'ed; `None` is a dangling symlink under `-L`, which GNU
    /// reports without an error string.
    Unreadable {
        name: std::ffi::OsString,
        error: Option<io::Error>,
    },
}

fn du_access_error(path: &str, error: Option<&io::Error>) {
    match error {
        Some(e) => eprintln!("du: cannot access {}: {}", quote_af(path), gnu_io_error(e)),
        None => eprintln!("du: cannot access {}", quote_af(path)),
    }
}

/// `stat` for `-L`: `Err(None)` marks a dangling symlink.
fn du_stat_follow(path: &Path) -> std::result::Result<fs::Metadata, Option<io::Error>> {
    fs::metadata(path).map_err(|e| match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => None,
        _ => Some(e),
    })
}

/// GNU fts path joining: `dir` + `/` + `name`, without doubling a trailing `/`.
fn fts_join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = dir.to_vec();
    if path.last() != Some(&b'/') {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

/// Reads `path` (whose printed name is `display`) and all its subdirectories,
/// fanning subdirectories out across the rayon pool. Entry order is kept, so
/// the sequential pass sees the same readdir order GNU's fts does.
fn du_collect(
    path: &Path,
    display: &[u8],
    meta: fs::Metadata,
    cfg: &DuConfig,
    root_dev: u64,
    ancestors: &[(u64, u64)],
) -> DuDir {
    use std::os::unix::ffi::OsStrExt;
    let mut dir = DuDir::empty(meta);
    let reader = match fs::read_dir(path) {
        Ok(r) => r,
        Err(e) => {
            dir.read_error = Some(e);
            return dir;
        }
    };
    let mut lineage = ancestors.to_vec();
    lineage.push((dir.meta.dev(), dir.meta.ino()));
    let mut pending: Vec<(usize, PathBuf, Vec<u8>, fs::Metadata)> = Vec::new();
    for entry in reader {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                dir.read_error = Some(e);
                break;
            }
        };
        let name = entry.file_name();
        let child = entry.path();
        let meta = if cfg.follow_all {
            du_stat_follow(&child)
        } else {
            entry.metadata().map_err(Some)
        };
        let meta = match meta {
            Ok(m) => m,
            Err(error) => {
                dir.entries.push(DuEntry::Unreadable { name, error });
                continue;
            }
        };
        if cfg.one_file_system && meta.dev() != root_dev {
            continue;
        }
        let needs_display = !cfg.excludes.is_empty() || meta.is_dir();
        let child_display = if needs_display {
            fts_join(display, name.as_bytes())
        } else {
            Vec::new()
        };
        if !cfg.excludes.is_empty() && cfg.excluded(&child_display) {
            continue;
        }
        if meta.is_dir() {
            // A directory that is its own ancestor (a symlink loop under -L)
            // stays empty; the walk then skips it as already counted.
            if !lineage.contains(&(meta.dev(), meta.ino())) {
                pending.push((dir.entries.len(), child, child_display, meta.clone()));
            }
            dir.entries.push(DuEntry::Dir {
                name,
                dir: Box::new(DuDir::empty(meta)),
            });
        } else if !cfg.all && cfg.dedup_key(&meta).is_none() {
            dir.plain_size += cfg.size_of(&meta);
            dir.plain_files += 1;
        } else {
            dir.entries.push(DuEntry::File { name, meta });
        }
    }
    let collected: Vec<(usize, DuDir)> = pending
        .into_par_iter()
        .map(|(index, child, child_display, meta)| {
            let sub = du_collect(&child, &child_display, meta, cfg, root_dev, &lineage);
            (index, sub)
        })
        .collect();
    for (index, sub) in collected {
        if let DuEntry::Dir { dir: slot, .. } = &mut dir.entries[index] {
            **slot = sub;
        }
    }
    dir
}

/// One line of output.
struct DuLine {
    path: Vec<u8>,
    size: u64,
    files: u64,
}

/// Where streamed `du` lines go.
type DuSink<'a> = &'a mut dyn FnMut(&DuLine) -> io::Result<()>;

/// The sequential, GNU-ordered pass: de-duplication, depth and threshold
/// filtering, and output.
struct DuWalker<'a> {
    cfg: &'a DuConfig,
    seen: std::collections::HashSet<(u64, u64)>,
    max_depth: usize,
    separate_dirs: bool,
    threshold: i128,
    lines: Vec<DuLine>,
    sink: Option<DuSink<'a>>,
    status: i32,
}

impl DuWalker<'_> {
    fn emit(&mut self, path: Vec<u8>, size: u64, files: u64) -> io::Result<()> {
        let size_i = i128::from(size);
        let keep = if self.threshold >= 0 {
            size_i >= self.threshold
        } else {
            size_i <= -self.threshold
        };
        if !keep {
            return Ok(());
        }
        let line = DuLine { path, size, files };
        match self.sink.as_mut() {
            Some(sink) => sink(&line),
            None => {
                self.lines.push(line);
                Ok(())
            }
        }
    }

    /// First visit of an entry: false if it was already counted.
    fn first_visit(&mut self, meta: &fs::Metadata) -> bool {
        match self.cfg.dedup_key(meta) {
            Some(key) => self.seen.insert(key),
            None => true,
        }
    }

    /// Walks a collected directory; returns (total size, file count) of the subtree.
    fn walk_dir(&mut self, dir: &DuDir, display: Vec<u8>, depth: usize) -> io::Result<(u64, u64)> {
        use std::os::unix::ffi::OsStrExt;
        if let Some(e) = &dir.read_error {
            eprintln!(
                "du: cannot read directory {}: {}",
                quote_af(&String::from_utf8_lossy(&display)),
                gnu_io_error(e)
            );
            self.status = 1;
        }
        let mut own = self.cfg.size_of(&dir.meta) + dir.plain_size;
        let mut files = dir.plain_files;
        let mut sub_size = 0;
        for entry in &dir.entries {
            match entry {
                DuEntry::File { name, meta } => {
                    if !self.first_visit(meta) {
                        continue;
                    }
                    let size = self.cfg.size_of(meta);
                    own += size;
                    files += 1;
                    if self.cfg.all && depth < self.max_depth {
                        self.emit(fts_join(&display, name.as_bytes()), size, 1)?;
                    }
                }
                DuEntry::Dir { name, dir: sub } => {
                    if !self.first_visit(&sub.meta) {
                        continue;
                    }
                    let (size, n) =
                        self.walk_dir(sub, fts_join(&display, name.as_bytes()), depth + 1)?;
                    sub_size += size;
                    files += n;
                }
                DuEntry::Unreadable { name, error } => {
                    let path = fts_join(&display, name.as_bytes());
                    du_access_error(&String::from_utf8_lossy(&path), error.as_ref());
                    self.status = 1;
                }
            }
        }
        let printed = if self.separate_dirs {
            own
        } else {
            own + sub_size
        };
        if depth <= self.max_depth {
            self.emit(display, printed, files)?;
        }
        Ok((own + sub_size, files))
    }
}

pub fn run_du(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = match gnu_getopt(DU_OPTIONS, &args[1..]) {
        Ok(p) => p,
        Err(msg) => return Ok(gnu_usage_error("du", &msg, 1)),
    };
    let (mut human_opts, mut block_size) = env_block_size("DU_BLOCK_SIZE");
    let mut explicit_units = false;
    let mut cfg = DuConfig {
        all: false,
        apparent: false,
        inodes: false,
        count_links: false,
        hash_all: false,
        follow_all: false,
        one_file_system: false,
        excludes: Vec::new(),
    };
    let mut null = false;
    let mut total = false;
    let mut deref_args = false;
    let mut max_depth: Option<usize> = None;
    let mut summarize = false;
    let mut separate_dirs = false;
    let mut threshold: i128 = 0;
    let mut json = false;
    for opt in parsed.opts {
        let value = opt.value.clone().unwrap_or_default();
        match opt.key {
            "null" => null = true,
            "all" => cfg.all = true,
            "apparent-size" => cfg.apparent = true,
            "block-size" => match parse_block_size(&value) {
                Ok(v) => {
                    (human_opts, block_size) = v;
                    explicit_units = true;
                }
                Err(e) => {
                    eprintln!("du: {}", e.message(&opt.spelled, &value));
                    return Ok(1);
                }
            },
            "bytes" => {
                cfg.apparent = true;
                (human_opts, block_size) = (0, 1);
                explicit_units = true;
            }
            "total" => total = true,
            "dereference-args" => deref_args = true,
            "max-depth" => match value.parse::<usize>() {
                Ok(d) if value.bytes().all(|b| b.is_ascii_digit()) => max_depth = Some(d),
                _ => {
                    let msg = format!(
                        "invalid maximum depth {}",
                        quote_bytes(value.as_bytes(), Quoting::Locale)
                    );
                    return Ok(gnu_usage_error("du", &msg, 1));
                }
            },
            "human-readable" => {
                (human_opts, block_size) = (HUMAN_AUTOSCALE | HUMAN_SI | HUMAN_BASE_1024, 1);
                explicit_units = false;
            }
            "si" => {
                (human_opts, block_size) = (HUMAN_AUTOSCALE | HUMAN_SI, 1);
                explicit_units = true;
            }
            "inodes" => cfg.inodes = true,
            "kilobytes" => {
                (human_opts, block_size) = (0, 1024);
                explicit_units = true;
            }
            "megabytes" => {
                (human_opts, block_size) = (0, 1024 * 1024);
                explicit_units = true;
            }
            "dereference" => cfg.follow_all = true,
            "no-dereference" => {
                cfg.follow_all = false;
                deref_args = false;
            }
            "count-links" => cfg.count_links = true,
            "separate-dirs" => separate_dirs = true,
            "summarize" => summarize = true,
            "threshold" => {
                let (negative, digits) = match value.strip_prefix('-') {
                    Some(rest) => (true, rest),
                    None => (false, value.as_str()),
                };
                match parse_size_suffixed(digits) {
                    Ok(0) if negative => {
                        let msg = SizeError::Invalid.message(&opt.spelled, &value);
                        eprintln!("du: {msg}");
                        return Ok(1);
                    }
                    Ok(n) => {
                        threshold = if negative {
                            -i128::from(n)
                        } else {
                            i128::from(n)
                        }
                    }
                    Err(e) => {
                        eprintln!("du: {}", e.message(&opt.spelled, &value));
                        return Ok(1);
                    }
                }
            }
            "exclude" => cfg.excludes.push(value.into_bytes()),
            "exclude-from" => {
                let data = if value == "-" {
                    let mut buf = Vec::new();
                    io::Read::read_to_end(&mut io::stdin(), &mut buf).map(|_| buf)
                } else {
                    fs::read(&value)
                };
                match data {
                    Ok(data) => cfg.excludes.extend(
                        data.split(|b| *b == b'\n')
                            .filter(|l| !l.is_empty())
                            .map(<[u8]>::to_vec),
                    ),
                    Err(e) => {
                        eprintln!("du: {}: {}", quote_f(&value), gnu_io_error(&e));
                        return Ok(1);
                    }
                }
            }
            "one-file-system" => cfg.one_file_system = true,
            "json" => json = true,
            "help" => return print_help_text(DU_HELP),
            "version" => {
                print_tool_version("du");
                return Ok(0);
            }
            _ => unreachable!("unhandled du option"),
        }
    }

    if cfg.all && summarize {
        return Ok(gnu_usage_error(
            "du",
            "cannot both summarize and show all entries",
            1,
        ));
    }
    if summarize && max_depth == Some(0) {
        eprintln!("du: warning: summarizing is the same as using --max-depth=0");
    }
    if let (true, Some(d)) = (summarize, max_depth)
        && d != 0
    {
        let msg = format!("warning: summarizing conflicts with --max-depth={d}");
        return Ok(gnu_usage_error("du", &msg, 1));
    }
    if cfg.inodes {
        if cfg.apparent {
            eprintln!("du: warning: options --apparent-size and -b are ineffective with --inodes");
        }
        block_size = 1;
    }
    let targets = if parsed.operands.is_empty() {
        vec![".".to_string()]
    } else {
        parsed.operands
    };
    cfg.hash_all = targets.len() > 1 || cfg.follow_all;

    // On a terminal, the default view is the Dogfish top-level breakdown.
    let rich = ctx.stdout_is_tty && !json;
    let rich_overview = rich && !cfg.all && !summarize && max_depth.is_none() && !separate_dirs;
    let max_depth = if summarize {
        0
    } else if rich_overview {
        1
    } else {
        max_depth.unwrap_or(usize::MAX)
    };

    ctx.telemetry
        .set_strategy("rayon parallel traversal, GNU-ordered accounting");
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    let eol = if null { b'\0' } else { b'\n' };
    let format_size_gnu = |n: u64| human_readable(n, human_opts, 1, block_size);
    let mut write_line = |line: &DuLine| -> io::Result<()> {
        out.write_all(format_size_gnu(line.size).as_bytes())?;
        out.write_all(b"\t")?;
        out.write_all(&line.path)?;
        out.write_all(&[eol])
    };
    let streaming = !rich && !json;
    let mut walker = DuWalker {
        cfg: &cfg,
        seen: std::collections::HashSet::new(),
        max_depth,
        separate_dirs,
        threshold,
        lines: Vec::new(),
        sink: if streaming {
            Some(&mut write_line)
        } else {
            None
        },
        status: 0,
    };

    let mut grand_total = 0;
    let mut grand_files = 0;
    let mut rich_groups: Vec<usize> = Vec::new();
    for target in &targets {
        // fts keeps at most one of several trailing slashes.
        let mut display = target.as_bytes().to_vec();
        while display.len() > 2 && display.ends_with(b"//") {
            display.pop();
        }
        let path = Path::new(target);
        let meta = if cfg.follow_all {
            du_stat_follow(path)
        } else if deref_args {
            fs::metadata(path)
                .or_else(|_| fs::symlink_metadata(path))
                .map_err(Some)
        } else {
            fs::symlink_metadata(path).map_err(Some)
        };
        let meta = match meta {
            Ok(m) => m,
            Err(e) => {
                du_access_error(target, e.as_ref());
                walker.status = 1;
                continue;
            }
        };
        if !cfg.excludes.is_empty() && cfg.excluded(&display) {
            continue;
        }
        if !walker.first_visit(&meta) {
            continue;
        }
        let res = if meta.is_dir() {
            let root_dev = meta.dev();
            let tree = du_collect(path, &display, meta, &cfg, root_dev, &[]);
            walker.walk_dir(&tree, display, 0)
        } else {
            let size = cfg.size_of(&meta);
            walker.emit(display, size, 1).map(|_| (size, 1))
        };
        match res {
            Ok((size, files)) => {
                grand_total += size;
                grand_files += files;
            }
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(0),
            Err(e) => return Err(e.into()),
        }
        rich_groups.push(walker.lines.len());
    }
    if total {
        let res = walker.emit(b"total".to_vec(), grand_total, grand_files);
        ignore_broken_pipe(res)?;
    }
    let status = walker.status;
    let mut lines = std::mem::take(&mut walker.lines);
    drop(walker);
    ctx.telemetry.add_items(grand_files);

    if json {
        let records: Vec<DuRecord> = lines
            .iter()
            .map(|l| DuRecord {
                path: String::from_utf8_lossy(&l.path).into_owned(),
                size: l.size,
                size_human: format_size(l.size, true),
                files: l.files,
            })
            .collect();
        emit_objects_or_table(&records, ctx, true, false)?;
        return Ok(status);
    }
    if rich {
        if rich_overview {
            // Largest subdirectories first within each operand; its total last.
            let mut start = 0;
            for end in rich_groups {
                if end > start + 1 {
                    lines[start..end - 1].sort_by_key(|l| std::cmp::Reverse(l.size));
                }
                start = end;
            }
        }
        let st = ctx.style();
        let max = lines.iter().map(|l| l.size).max().unwrap_or(1).max(1);
        for l in &lines {
            let size = if explicit_units || cfg.inodes {
                format_size_gnu(l.size)
            } else {
                format_size(l.size, true)
            };
            let row = format!(
                "{}  {}  {}\n",
                st.bold_green(&format!("{size:>10}")),
                sparkline_bar(l.size as f64 / max as f64, 10, st),
                st.bold(&String::from_utf8_lossy(&l.path))
            );
            ignore_broken_pipe(out.write_all(row.as_bytes()))?;
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(status)
}

// ---------------------------------------------------------------------------
// 12. ls — Colors, grid, `--tree` and `--json` on a TTY; GNU `ls` otherwise
// ---------------------------------------------------------------------------

/// GNU `ls` options. `-L` means `--dereference`, except with `--tree`, where it
/// keeps its Dogfish meaning of `--level` (see [`ls_option_table`]).
const LS_OPTIONS: &[OptSpec] = &[
    // Long options in GNU's table order (it decides ambiguity diagnostics).
    OptSpec::new("all", Some('a'), Some("all"), OptArg::No),
    OptSpec::new("escape", Some('b'), Some("escape"), OptArg::No),
    OptSpec::new("directory", Some('d'), Some("directory"), OptArg::No),
    OptSpec::new("full-time", None, Some("full-time"), OptArg::No),
    OptSpec::new(
        "group-directories-first",
        None,
        Some("group-directories-first"),
        OptArg::No,
    ),
    OptSpec::new(
        "human-readable",
        Some('h'),
        Some("human-readable"),
        OptArg::No,
    ),
    OptSpec::new("inode", Some('i'), Some("inode"), OptArg::No),
    OptSpec::new("kibibytes", Some('k'), Some("kibibytes"), OptArg::No),
    OptSpec::new(
        "numeric-uid-gid",
        Some('n'),
        Some("numeric-uid-gid"),
        OptArg::No,
    ),
    OptSpec::new("no-group", Some('G'), Some("no-group"), OptArg::No),
    OptSpec::new(
        "hide-control-chars",
        Some('q'),
        Some("hide-control-chars"),
        OptArg::No,
    ),
    OptSpec::new("reverse", Some('r'), Some("reverse"), OptArg::No),
    OptSpec::new("size", Some('s'), Some("size"), OptArg::No),
    OptSpec::new("width", Some('w'), Some("width"), OptArg::Required),
    OptSpec::new("almost-all", Some('A'), Some("almost-all"), OptArg::No),
    OptSpec::new(
        "ignore-backups",
        Some('B'),
        Some("ignore-backups"),
        OptArg::No,
    ),
    OptSpec::new("classify", Some('F'), Some("classify"), OptArg::Optional),
    OptSpec::new("file-type", None, Some("file-type"), OptArg::No),
    OptSpec::new("si", None, Some("si"), OptArg::No),
    OptSpec::new(
        "dereference-command-line",
        Some('H'),
        Some("dereference-command-line"),
        OptArg::No,
    ),
    OptSpec::new(
        "dereference-command-line-symlink-to-dir",
        None,
        Some("dereference-command-line-symlink-to-dir"),
        OptArg::No,
    ),
    OptSpec::new("hide", None, Some("hide"), OptArg::Required),
    OptSpec::new("ignore", Some('I'), Some("ignore"), OptArg::Required),
    OptSpec::new(
        "indicator-style",
        None,
        Some("indicator-style"),
        OptArg::Required,
    ),
    OptSpec::new("dereference", Some('L'), Some("dereference"), OptArg::No),
    OptSpec::new("literal", Some('N'), Some("literal"), OptArg::No),
    OptSpec::new("quote-name", Some('Q'), Some("quote-name"), OptArg::No),
    OptSpec::new(
        "quoting-style",
        None,
        Some("quoting-style"),
        OptArg::Required,
    ),
    OptSpec::new("recursive", Some('R'), Some("recursive"), OptArg::No),
    OptSpec::new("format", None, Some("format"), OptArg::Required),
    OptSpec::new(
        "show-control-chars",
        None,
        Some("show-control-chars"),
        OptArg::No,
    ),
    OptSpec::new("sort", None, Some("sort"), OptArg::Required),
    OptSpec::new("tabsize", Some('T'), Some("tabsize"), OptArg::Required),
    OptSpec::new("time", None, Some("time"), OptArg::Required),
    OptSpec::new("time-style", None, Some("time-style"), OptArg::Required),
    OptSpec::new("zero", None, Some("zero"), OptArg::No),
    OptSpec::new("color", None, Some("color"), OptArg::Optional),
    OptSpec::new("hyperlink", None, Some("hyperlink"), OptArg::Optional),
    OptSpec::new("block-size", None, Some("block-size"), OptArg::Required),
    OptSpec::new("context", Some('Z'), Some("context"), OptArg::No),
    OptSpec::new("author", None, Some("author"), OptArg::No),
    OptSpec::new("help", None, Some("help"), OptArg::No),
    OptSpec::new("version", None, Some("version"), OptArg::No),
    // Dogfish extensions.
    OptSpec::new("tree", None, Some("tree"), OptArg::No),
    OptSpec::new("level", None, Some("level"), OptArg::Required),
    OptSpec::new("json", Some('J'), Some("json"), OptArg::No),
    // Short-only options.
    OptSpec::new("ctime", Some('c'), None, OptArg::No),
    OptSpec::new("vertical", Some('C'), None, OptArg::No),
    OptSpec::new("unsorted-all", Some('f'), None, OptArg::No),
    OptSpec::new("no-owner", Some('g'), None, OptArg::No),
    OptSpec::new("long", Some('l'), None, OptArg::No),
    OptSpec::new("commas", Some('m'), None, OptArg::No),
    OptSpec::new("long-no-group", Some('o'), None, OptArg::No),
    OptSpec::new("slash", Some('p'), None, OptArg::No),
    OptSpec::new("sort-size", Some('S'), None, OptArg::No),
    OptSpec::new("sort-time", Some('t'), None, OptArg::No),
    OptSpec::new("atime", Some('u'), None, OptArg::No),
    OptSpec::new("unsorted", Some('U'), None, OptArg::No),
    OptSpec::new("sort-version", Some('v'), None, OptArg::No),
    OptSpec::new("across", Some('x'), None, OptArg::No),
    OptSpec::new("sort-extension", Some('X'), None, OptArg::No),
    OptSpec::new("one-per-line", Some('1'), None, OptArg::No),
];

/// With `--tree`, `-L N` is the tree depth (`dogfish ls --tree -L 2`).
fn ls_option_table(args: &[String]) -> Vec<OptSpec> {
    let tree = args
        .iter()
        .take_while(|a| *a != "--")
        .any(|a| a == "--tree" || (a.len() > 3 && "--tree".starts_with(a.as_str())));
    LS_OPTIONS
        .iter()
        .map(|s| {
            if tree && s.key == "dereference" {
                OptSpec::new("level", Some('L'), None, OptArg::Required)
            } else {
                OptSpec::new(s.key, s.short, s.long, s.arg)
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

#[derive(Serialize, Clone)]
pub struct LsEntry {
    pub name: String,
    pub path: String,
    pub kind: String,
    pub size: u64,
    pub mode: String,
    pub mode_octal: String,
    pub user: String,
    pub group: String,
    pub modified_unix: i64,
    pub modified_relative: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LsFormat {
    Long,
    OnePerLine,
    Columns,
    Across,
    Commas,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LsSort {
    Name,
    None,
    Time,
    Size,
    Extension,
    Version,
    Width,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LsTime {
    Mtime,
    Atime,
    Ctime,
    Birth,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Indicator {
    None,
    Slash,
    FileType,
    Classify,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LsDeref {
    Never,
    CommandLineSymlinkToDir,
    CommandLine,
    Always,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LsIgnore {
    /// Hide names starting with `.`.
    Default,
    /// `-A`: hide only `.` and `..`.
    DotAndDotDot,
    /// `-a`: hide nothing.
    Minimal,
}

struct LsOpts {
    ignore: LsIgnore,
    ignore_patterns: Vec<Vec<u8>>,
    hide_patterns: Vec<Vec<u8>>,
    immediate_dirs: bool,
    recursive: bool,
    format: LsFormat,
    sort: LsSort,
    reverse: bool,
    dirs_first: bool,
    time: LsTime,
    indicator: Indicator,
    deref: LsDeref,
    inode: bool,
    blocks: bool,
    owner: bool,
    group: bool,
    author: bool,
    numeric: bool,
    context: bool,
    quoting: Quoting,
    hide_control: bool,
    human_opts: u32,
    block_size: u64,
    file_human_opts: u32,
    file_block_size: u64,
    /// `[non-recent, recent]` strftime formats for `-l`.
    time_formats: [String; 2],
    line_length: usize,
    tab_size: usize,
    eol: u8,
    tree: bool,
    level: usize,
    json: bool,
    now: std::cell::Cell<Timespec>,
}

/// One listed file.
struct LsFile {
    /// The name as printed: the operand, or the entry name inside a directory.
    name: Vec<u8>,
    path: PathBuf,
    meta: Option<fs::Metadata>,
    /// `ls -l` file-type letter from `readdir` when the file could not be stat'ed.
    type_letter: u8,
    link_target: Option<Vec<u8>>,
    target_meta: Option<fs::Metadata>,
    /// A command-line directory to list, or a subdirectory to recurse into.
    list_as_dir: bool,
}

impl LsFile {
    fn is_dir(&self) -> bool {
        match &self.meta {
            Some(m) => m.is_dir(),
            None => self.type_letter == b'd',
        }
    }

    fn time(&self, which: LsTime) -> Option<Timespec> {
        let m = self.meta.as_ref()?;
        match which {
            LsTime::Mtime => Some(meta_mtime(m)),
            LsTime::Atime => Some(meta_atime(m)),
            LsTime::Ctime => Some(meta_ctime(m)),
            LsTime::Birth => meta_birth(m),
        }
    }

    fn size(&self) -> u64 {
        self.meta.as_ref().map_or(0, |m| m.size())
    }
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
            Err(try_help("ls", 2))
        }
    }
}

/// Parses `ls` arguments; `Err` carries the exit status after a diagnostic.
/// Like GNU, the default format is columns on a terminal, one per line otherwise.
fn parse_ls_args(
    args: &[String],
    stdout_is_tty: bool,
) -> std::result::Result<(LsOpts, Vec<String>), i32> {
    let specs = ls_option_table(args);
    let parsed = gnu_getopt(&specs, args).map_err(|msg| gnu_usage_error("ls", &msg, 2))?;

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
        tree: false,
        level: 3,
        json: false,
        now: std::cell::Cell::new(Timespec::now()),
    };
    if let Some(cols) = std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse::<usize>().ok())
        .filter(|c| *c > 0)
    {
        o.line_length = cols;
    }
    if let Some(tab) = std::env::var("TABSIZE")
        .ok()
        .and_then(|t| t.parse::<usize>().ok())
    {
        o.tab_size = tab;
    }
    let mut deref: Option<LsDeref> = None;
    let mut sort_specified = false;
    let mut time_style: Option<String> = std::env::var("TIME_STYLE").ok();
    let mut kibibytes = false;
    let mut explicit_sizes = false;
    let mut time_explicit = false;
    let mut quoting: Option<Quoting> = None;
    let mut hide_control: Option<bool> = None;
    let width_arg = |value: &str, what: &str| -> std::result::Result<usize, i32> {
        value.parse::<usize>().map_err(|_| {
            eprintln!(
                "ls: invalid {what}: {}",
                quote_bytes(value.as_bytes(), Quoting::Locale)
            );
            2
        })
    };
    for opt in parsed.opts {
        let value = opt.value.clone().unwrap_or_default();
        match opt.key {
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
                    eprintln!("ls: {}", e.message(&opt.spelled, &value));
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
            "color" | "hyperlink" => {}
            "directory" => o.immediate_dirs = true,
            "unsorted-all" => {
                o.ignore = LsIgnore::Minimal;
                o.sort = LsSort::None;
                sort_specified = true;
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
                let when = match &opt.value {
                    None => "always",
                    Some(v) => gnu_argmatch("ls", "--classify", v, &when)
                        .ok_or_else(|| try_help("ls", 1))?,
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
                o.indicator = match gnu_argmatch("ls", "--indicator-style", &value, &choices) {
                    Some("none") => Indicator::None,
                    Some("slash") => Indicator::Slash,
                    Some("file-type") => Indicator::FileType,
                    Some(_) => Indicator::Classify,
                    None => return Err(try_help("ls", 1)),
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
                o.format = match gnu_argmatch("ls", "--format", &value, &choices) {
                    Some("long") => LsFormat::Long,
                    Some("commas") => LsFormat::Commas,
                    Some("across") => LsFormat::Across,
                    Some("vertical") => LsFormat::Columns,
                    Some(_) => LsFormat::OnePerLine,
                    None => return Err(try_help("ls", 1)),
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
                let opts = if opt.key == "si" {
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
            "hide" => o.hide_patterns.push(value.into_bytes()),
            "ignore" => o.ignore_patterns.push(value.into_bytes()),
            "inode" => o.inode = true,
            "kibibytes" => kibibytes = true,
            "literal" => quoting = Some(Quoting::Literal),
            "hide-control-chars" => hide_control = Some(true),
            "show-control-chars" => hide_control = Some(false),
            "quote-name" => quoting = Some(Quoting::C),
            "quoting-style" => {
                let choices: Vec<(&str, &str)> = Quoting::NAMES.iter().map(|n| (*n, *n)).collect();
                let name = gnu_argmatch("ls", "--quoting-style", &value, &choices)
                    .ok_or_else(|| try_help("ls", 1))?;
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
                o.sort = match gnu_argmatch("ls", "--sort", &value, &choices) {
                    Some("none") => LsSort::None,
                    Some("time") => LsSort::Time,
                    Some("size") => LsSort::Size,
                    Some("extension") => LsSort::Extension,
                    Some("version") => LsSort::Version,
                    Some(_) => LsSort::Width,
                    None => return Err(try_help("ls", 1)),
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
                o.time = match gnu_argmatch("ls", "--time", &value, &choices) {
                    Some("atime") => LsTime::Atime,
                    Some("ctime") => LsTime::Ctime,
                    Some("mtime") => LsTime::Mtime,
                    Some(_) => LsTime::Birth,
                    None => return Err(try_help("ls", 1)),
                };
                time_explicit = o.time != LsTime::Mtime;
            }
            "time-style" => time_style = Some(value),
            "tabsize" => o.tab_size = width_arg(&value, "tab size")?,
            "width" => o.line_length = width_arg(&value, "line width")?,
            "context" => o.context = true,
            "zero" => o.eol = b'\0',
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
            "help" => {
                let _ = print_help_text(LS_HELP);
                return Err(0);
            }
            "version" => {
                print_tool_version("ls");
                return Err(0);
            }
            _ => unreachable!("unhandled ls option"),
        }
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
    o.quoting = quoting.unwrap_or_else(|| {
        std::env::var("QUOTING_STYLE")
            .ok()
            .and_then(|s| Quoting::from_name(&s))
            .unwrap_or(Quoting::Literal)
    });
    o.hide_control = hide_control.unwrap_or(false);
    if o.eol == b'\0' && o.format != LsFormat::Long {
        o.format = LsFormat::OnePerLine;
    }
    Ok((o, parsed.operands))
}

pub fn run_ls(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let (opts, operands) = match parse_ls_args(&args[1..], ctx.stdout_is_tty) {
        Ok(v) => v,
        Err(status) => return Ok(status),
    };
    let targets = if operands.is_empty() {
        vec![".".to_string()]
    } else {
        operands
    };
    let st = ctx.style();

    if opts.tree {
        ctx.telemetry.set_strategy("recursive visual tree renderer");
        let stdout = io::stdout();
        let mut out = io::BufWriter::new(stdout.lock());
        for t in &targets {
            writeln!(out, "{}", st.bold_blue(t))?;
            render_tree(
                Path::new(t),
                "",
                0,
                opts.level,
                opts.ignore != LsIgnore::Default,
                &mut out,
                st,
                ctx,
            )?;
        }
        ignore_broken_pipe(out.flush())?;
        return Ok(0);
    }

    let rich = ctx.stdout_is_tty
        && !opts.json
        && matches!(
            opts.format,
            LsFormat::Long | LsFormat::OnePerLine | LsFormat::Columns
        );
    let stdout = io::stdout();
    let mut lister = Lister {
        opts: &opts,
        out: io::BufWriter::new(stdout.lock()),
        status: 0,
        first_header: true,
        rich: rich.then_some(st),
        json: Vec::new(),
        active: Vec::new(),
    };
    match lister.run(&targets) {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(lister.status),
        res => res?,
    }
    ctx.telemetry.add_items(lister.json.len() as u64);
    if opts.json {
        ctx.telemetry
            .set_strategy("NDJSON structured object stream");
        let records = std::mem::take(&mut lister.json);
        drop(lister);
        emit_objects_or_table(&records, ctx, true, false)?;
        return Ok(0);
    }
    let status = lister.status;
    ignore_broken_pipe(lister.out.flush())?;
    Ok(status)
}

/// Raw `readdir` (keeps `.`/`..` and directory order): `(name, file-type letter)`.
fn read_dir_entries(path: &Path) -> io::Result<Vec<(Vec<u8>, u8)>> {
    let c_path = CString::new(path_bytes(path)).map_err(io::Error::other)?;
    // SAFETY: opendir/readdir/closedir on a valid C path; each dirent is
    // copied out before the next readdir call.
    unsafe {
        let dir = libc::opendir(c_path.as_ptr());
        if dir.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut entries = Vec::new();
        loop {
            let ent = libc::readdir(dir);
            if ent.is_null() {
                break;
            }
            let name = std::ffi::CStr::from_ptr((*ent).d_name.as_ptr())
                .to_bytes()
                .to_vec();
            let letter = match (*ent).d_type {
                libc::DT_DIR => b'd',
                libc::DT_LNK => b'l',
                libc::DT_REG => b'-',
                libc::DT_FIFO => b'p',
                libc::DT_SOCK => b's',
                libc::DT_CHR => b'c',
                libc::DT_BLK => b'b',
                _ => b'?',
            };
            entries.push((name, letter));
        }
        libc::closedir(dir);
        Ok(entries)
    }
}

/// Column widths of one listing (GNU computes them per directory).
#[derive(Default)]
struct LsWidths {
    inode: usize,
    blocks: usize,
    nlink: usize,
    owner: usize,
    group: usize,
    author: usize,
    context: usize,
    size: usize,
    major: usize,
    minor: usize,
    any_acl: bool,
    some_quoted: bool,
}

/// Walks operands and directories, printing them as GNU `ls` does.
struct Lister<'a, W: Write> {
    opts: &'a LsOpts,
    out: W,
    status: i32,
    first_header: bool,
    /// Dogfish styling on a terminal.
    rich: Option<Style>,
    json: Vec<LsEntry>,
    /// Directories being listed (for `-R` loop detection).
    active: Vec<(u64, u64)>,
}

impl<W: Write> Lister<'_, W> {
    fn run(&mut self, targets: &[String]) -> io::Result<()> {
        let o = self.opts;
        let mut files = Vec::new();
        for t in targets {
            if let Some(f) = self.gobble_operand(t) {
                files.push(f);
            }
        }
        self.sort(&mut files);
        let (dirs, plain): (Vec<&LsFile>, Vec<&LsFile>) = files.iter().partition(|f| f.list_as_dir);
        // GNU names each directory unless a lone directory operand is listed.
        let print_names =
            o.recursive || !(plain.is_empty() && targets.len() <= 1 && dirs.len() == 1);
        if !plain.is_empty() {
            self.print_files(&plain, &files.iter().collect::<Vec<_>>(), false)?;
            if !dirs.is_empty() {
                self.out.write_all(&[o.eol])?;
            }
        }
        for d in dirs {
            self.list_dir(&d.path, &d.name, print_names, true)?;
        }
        Ok(())
    }

    fn fail(&mut self, msg: String, serious: bool) {
        eprintln!("ls: {msg}");
        self.status = self.status.max(if serious { 2 } else { 1 });
    }

    fn gobble_operand(&mut self, name: &str) -> Option<LsFile> {
        let o = self.opts;
        let path = PathBuf::from(name);
        let meta = match o.deref {
            LsDeref::Always | LsDeref::CommandLine => fs::metadata(&path),
            LsDeref::CommandLineSymlinkToDir => match fs::metadata(&path) {
                Ok(m) if m.is_dir() => Ok(m),
                _ => fs::symlink_metadata(&path),
            },
            LsDeref::Never => fs::symlink_metadata(&path),
        };
        let meta = match meta {
            Ok(m) => m,
            Err(e) => {
                self.fail(
                    format!("cannot access {}: {}", quote_af(name), gnu_io_error(&e)),
                    true,
                );
                return None;
            }
        };
        let list_as_dir = meta.is_dir() && !o.immediate_dirs;
        let mut f = LsFile {
            name: name.as_bytes().to_vec(),
            path,
            meta: Some(meta),
            type_letter: b'?',
            link_target: None,
            target_meta: None,
            list_as_dir,
        };
        self.inspect_link(&mut f);
        Some(f)
    }

    fn gobble_entry(&mut self, dir: &Path, name: Vec<u8>, letter: u8) -> LsFile {
        let o = self.opts;
        let path = if dir == Path::new(".") {
            bytes_path(&name).to_path_buf()
        } else {
            dir.join(bytes_path(&name))
        };
        // Like GNU, only follow a link (and report a dangling one) when the
        // output needs the target's details.
        let needs_stat = o.format == LsFormat::Long
            || o.blocks
            || o.context
            || matches!(o.sort, LsSort::Time | LsSort::Size);
        let needs_type = o.recursive || o.indicator != Indicator::None || o.dirs_first;
        let follow = o.deref == LsDeref::Always
            && (needs_stat || ((needs_type || o.inode) && matches!(letter, b'l' | b'?')));
        let meta = if follow {
            match fs::metadata(&path) {
                Ok(m) => Some(m),
                Err(e) => {
                    let shown = String::from_utf8_lossy(path_bytes(&path)).into_owned();
                    self.fail(
                        format!("cannot access {}: {}", quote_af(&shown), gnu_io_error(&e)),
                        false,
                    );
                    None
                }
            }
        } else {
            fs::symlink_metadata(&path).ok()
        };
        let is_dot = name == b"." || name == b"..";
        let list_as_dir = o.recursive && !is_dot && meta.as_ref().is_some_and(|m| m.is_dir());
        let mut f = LsFile {
            name,
            path,
            meta,
            type_letter: letter,
            link_target: None,
            target_meta: None,
            list_as_dir,
        };
        self.inspect_link(&mut f);
        f
    }

    /// Reads a symlink's target (for `-l`) and stats it (for `-F` indicators).
    fn inspect_link(&self, f: &mut LsFile) {
        let o = self.opts;
        let is_link = f.meta.as_ref().is_some_and(|m| m.file_type().is_symlink());
        // GNU also checks link targets for --group-directories-first.
        let check_target = o.indicator >= Indicator::FileType || o.dirs_first;
        if !is_link || !(o.format == LsFormat::Long || check_target || self.rich.is_some()) {
            return;
        }
        if let Ok(target) = fs::read_link(&f.path) {
            f.link_target = Some(path_bytes(&target).to_vec());
            if check_target {
                f.target_meta = fs::metadata(&f.path).ok();
            }
        }
    }

    fn ignored(&self, name: &[u8]) -> bool {
        let o = self.opts;
        let dot = name.first() == Some(&b'.');
        let dot_or_dotdot = name == b"." || name == b"..";
        (o.ignore == LsIgnore::Default && dot)
            || (o.ignore == LsIgnore::DotAndDotDot && dot_or_dotdot)
            || (o.ignore == LsIgnore::Default
                && o.hide_patterns.iter().any(|p| fnmatch(p, name, true)))
            || o.ignore_patterns.iter().any(|p| fnmatch(p, name, true))
    }

    fn list_dir(
        &mut self,
        path: &Path,
        name: &[u8],
        print_name: bool,
        command_line: bool,
    ) -> io::Result<()> {
        let o = self.opts;
        let id = fs::metadata(path).ok().map(|m| (m.dev(), m.ino()));
        if let Some(id) = id
            && self.active.contains(&id)
        {
            let shown = String::from_utf8_lossy(name).into_owned();
            self.fail(
                format!("{}: not listing already-listed directory", quote_f(&shown)),
                true,
            );
            return Ok(());
        }
        let entries = match read_dir_entries(path) {
            Ok(e) => e,
            Err(e) => {
                let shown = String::from_utf8_lossy(name).into_owned();
                self.fail(
                    format!(
                        "cannot open directory {}: {}",
                        quote_af(&shown),
                        gnu_io_error(&e)
                    ),
                    command_line,
                );
                return Ok(());
            }
        };
        if print_name {
            if !self.first_header {
                self.out.write_all(&[o.eol])?;
            }
            let mut header = quote_bytes_raw(name, o.quoting, b":");
            if let Some(st) = self.rich {
                header = st.bold_blue(&String::from_utf8_lossy(&header)).into_bytes();
            }
            self.out.write_all(&header)?;
            self.out.write_all(b":")?;
            self.out.write_all(&[o.eol])?;
        }
        self.first_header = false;

        let mut files: Vec<LsFile> = Vec::with_capacity(entries.len());
        for (entry, letter) in entries {
            if !self.ignored(&entry) {
                let f = self.gobble_entry(path, entry, letter);
                files.push(f);
            }
        }
        self.sort(&mut files);
        let refs: Vec<&LsFile> = files.iter().collect();
        self.print_files(&refs, &refs, true)?;

        if o.recursive {
            if let Some(id) = id {
                self.active.push(id);
            }
            for f in files.iter().filter(|f| f.list_as_dir) {
                let mut sub_name = name.to_vec();
                if sub_name.last() != Some(&b'/') {
                    sub_name.push(b'/');
                }
                sub_name.extend_from_slice(&f.name);
                self.list_dir(&f.path, &sub_name, true, false)?;
            }
            if id.is_some() {
                self.active.pop();
            }
        }
        Ok(())
    }

    fn sort(&self, files: &mut [LsFile]) {
        use std::cmp::Ordering;
        let o = self.opts;
        if o.sort == LsSort::None {
            return;
        }
        let by_name = |a: &LsFile, b: &LsFile| a.name.cmp(&b.name);
        let key = |a: &LsFile, b: &LsFile| -> Ordering {
            let primary = match o.sort {
                LsSort::Time => b.time(o.time).cmp(&a.time(o.time)),
                LsSort::Size => b.size().cmp(&a.size()),
                LsSort::Extension => {
                    let ext = |n: &[u8]| -> Vec<u8> {
                        n.iter()
                            .rposition(|c| *c == b'.')
                            .map_or(Vec::new(), |i| n[i..].to_vec())
                    };
                    ext(&a.name).cmp(&ext(&b.name))
                }
                LsSort::Version => filevercmp(&a.name, &b.name),
                LsSort::Width => {
                    let w = |f: &LsFile| display_width(&quote_bytes_raw(&f.name, o.quoting, b""));
                    w(a).cmp(&w(b))
                }
                LsSort::Name | LsSort::None => Ordering::Equal,
            };
            let ord = primary.then_with(|| by_name(a, b));
            if o.reverse { ord.reverse() } else { ord }
        };
        files.sort_by(|a, b| {
            if o.dirs_first {
                let dir =
                    |f: &LsFile| f.is_dir() || f.target_meta.as_ref().is_some_and(|m| m.is_dir());
                match dir(b).cmp(&dir(a)) {
                    Ordering::Equal => key(a, b),
                    other => other,
                }
            } else {
                key(a, b)
            }
        });
    }

    /// The quoted name; the bool says whether quoting changed it.
    fn quoted_name(&self, name: &[u8]) -> (Vec<u8>, bool) {
        let o = self.opts;
        let extra: &[u8] = if o.quoting == Quoting::Escape {
            b" "
        } else {
            b""
        };
        let mut q = quote_bytes_raw(name, o.quoting, extra);
        let changed = q != name;
        if o.hide_control {
            let mut out = Vec::with_capacity(q.len());
            let mut i = 0;
            while i < q.len() {
                let (len, printable) = char_at(&q[i..]);
                if printable {
                    out.extend_from_slice(&q[i..i + len]);
                } else {
                    out.push(b'?');
                }
                i += len;
            }
            q = out;
        }
        (q, changed)
    }

    fn blocks_cell(&self, f: &LsFile) -> String {
        match &f.meta {
            Some(m) => human_readable(m.blocks(), self.opts.human_opts, 512, self.opts.block_size),
            None => "?".into(),
        }
    }

    fn owner_cell(&self, f: &LsFile, group: bool) -> (String, bool) {
        let Some(m) = &f.meta else {
            return ("?".into(), false);
        };
        let id = if group { m.gid() } else { m.uid() };
        let name = if self.opts.numeric {
            None
        } else if group {
            group_name(id)
        } else {
            user_name(id)
        };
        match name {
            Some(n) => (n, false),
            None => (id.to_string(), true),
        }
    }

    fn size_cell(&self, f: &LsFile) -> String {
        use std::os::unix::fs::FileTypeExt;
        match &f.meta {
            Some(m) if m.file_type().is_char_device() || m.file_type().is_block_device() => {
                format!("{}, {}", dev_major(m.rdev()), dev_minor(m.rdev()))
            }
            Some(m) => human_readable(
                m.size(),
                self.opts.file_human_opts,
                1,
                self.opts.file_block_size,
            ),
            None => "?".into(),
        }
    }

    fn context_cell(&self, f: &LsFile) -> String {
        security_context(&f.path).unwrap_or_else(|| "?".into())
    }

    fn compute_widths(&self, files: &[&LsFile]) -> LsWidths {
        use std::os::unix::fs::FileTypeExt;
        let o = self.opts;
        let mut w = LsWidths::default();
        let align_quotes = matches!(
            o.quoting,
            Quoting::Shell | Quoting::ShellEscape | Quoting::CMaybe
        ) && (o.format == LsFormat::Long
            || (matches!(o.format, LsFormat::Columns | LsFormat::Across) && o.line_length > 0));
        for f in files {
            if align_quotes && self.quoted_name(&f.name).1 {
                w.some_quoted = true;
            }
            if o.inode {
                let ino = f.meta.as_ref().map_or(1, |m| m.ino().to_string().len());
                w.inode = w.inode.max(ino);
            }
            if o.blocks || o.format == LsFormat::Long {
                w.blocks = w.blocks.max(self.blocks_cell(f).len());
            }
            if o.context {
                w.context = w
                    .context
                    .max(display_width(self.context_cell(f).as_bytes()));
            }
            if o.format != LsFormat::Long {
                continue;
            }
            let Some(m) = &f.meta else {
                continue;
            };
            w.nlink = w.nlink.max(m.nlink().to_string().len());
            let owner_width = display_width(self.owner_cell(f, false).0.as_bytes());
            if o.owner {
                w.owner = w.owner.max(owner_width);
            }
            if o.author {
                w.author = w.author.max(owner_width);
            }
            if o.group {
                w.group = w
                    .group
                    .max(display_width(self.owner_cell(f, true).0.as_bytes()));
            }
            if m.file_type().is_char_device() || m.file_type().is_block_device() {
                w.major = w.major.max(dev_major(m.rdev()).to_string().len());
                w.minor = w.minor.max(dev_minor(m.rdev()).to_string().len());
                w.size = w.size.max(w.major + 2 + w.minor);
            } else {
                w.size = w.size.max(self.size_cell(f).len());
            }
            if !w.any_acl && acl_marker(f).is_some() {
                w.any_acl = true;
            }
        }
        w
    }

    /// Prints one listing. `width_set` determines column widths (GNU includes
    /// command-line directories there too); `total` adds the `total` line.
    fn print_files(
        &mut self,
        files: &[&LsFile],
        width_set: &[&LsFile],
        total: bool,
    ) -> io::Result<()> {
        let o = self.opts;
        if o.json {
            self.json.extend(files.iter().map(|f| ls_json_entry(f)));
            return Ok(());
        }
        if let Some(st) = self.rich {
            return self.print_rich(files, st);
        }
        let w = self.compute_widths(width_set);
        if total && (o.format == LsFormat::Long || o.blocks) {
            let blocks: u64 = files
                .iter()
                .filter_map(|f| f.meta.as_ref())
                .map(|m| m.blocks())
                .sum();
            let line = format!(
                "total {}",
                human_readable(blocks, o.human_opts, 512, o.block_size)
            );
            self.out.write_all(line.as_bytes())?;
            self.out.write_all(&[o.eol])?;
        }
        match o.format {
            LsFormat::Long => {
                for f in files {
                    let line = self.long_line(f, &w);
                    self.out.write_all(&line)?;
                    self.out.write_all(&[o.eol])?;
                }
            }
            LsFormat::OnePerLine => {
                for f in files {
                    let cell = self.short_cell(f, &w, false);
                    self.out.write_all(&cell.0)?;
                    self.out.write_all(&[o.eol])?;
                }
            }
            LsFormat::Commas => {
                let mut pos = 0;
                for (i, f) in files.iter().enumerate() {
                    let (cell, len) = self.short_cell(f, &w, true);
                    if i != 0 {
                        if o.line_length == 0 || pos + len + 2 < o.line_length {
                            pos += 2;
                            self.out.write_all(b", ")?;
                        } else {
                            pos = 0;
                            self.out.write_all(&[b',', o.eol])?;
                        }
                    }
                    self.out.write_all(&cell)?;
                    pos += len;
                }
                if !files.is_empty() {
                    self.out.write_all(&[o.eol])?;
                }
            }
            LsFormat::Columns | LsFormat::Across => self.print_grid(files, &w)?,
        }
        Ok(())
    }

    /// `-C` / `-x` layout (GNU `calculate_columns`, `print_many_per_line`,
    /// `print_horizontal`).
    fn print_grid(&mut self, files: &[&LsFile], w: &LsWidths) -> io::Result<()> {
        let o = self.opts;
        if files.is_empty() {
            return Ok(());
        }
        let cells: Vec<(Vec<u8>, usize)> =
            files.iter().map(|f| self.short_cell(f, w, false)).collect();
        let by_columns = o.format == LsFormat::Columns;
        let n = cells.len();
        const MIN_COLUMN_WIDTH: usize = 3;
        let max_idx = if o.line_length == 0 {
            n
        } else {
            (o.line_length / MIN_COLUMN_WIDTH).max(1)
        };
        let max_cols = max_idx.min(n);
        // (valid, line length, column widths) for 1..=max_cols columns.
        let mut configs: Vec<(bool, usize, Vec<usize>)> = (0..max_cols)
            .map(|i| {
                (
                    true,
                    (i + 1) * MIN_COLUMN_WIDTH,
                    vec![MIN_COLUMN_WIDTH; i + 1],
                )
            })
            .collect();
        for (fileno, (_, len)) in cells.iter().enumerate() {
            for (i, cfg) in configs.iter_mut().enumerate() {
                if !cfg.0 {
                    continue;
                }
                let idx = if by_columns {
                    fileno / n.div_ceil(i + 1)
                } else {
                    fileno % (i + 1)
                };
                let real = len + if idx == i { 0 } else { 2 };
                if cfg.2[idx] < real {
                    cfg.1 += real - cfg.2[idx];
                    cfg.2[idx] = real;
                    cfg.0 = o.line_length == 0 || cfg.1 < o.line_length;
                }
            }
        }
        let mut cols = max_cols;
        while cols > 1 && !configs[cols - 1].0 {
            cols -= 1;
        }
        let widths = &configs[cols - 1].2;
        // GNU pads with tabs where possible, except for unlimited width.
        let tabs = o.tab_size != 0 && o.line_length != 0;
        let indent = |out: &mut W, mut from: usize, to: usize| -> io::Result<()> {
            while from < to {
                if tabs && to / o.tab_size > (from + 1) / o.tab_size {
                    out.write_all(b"\t")?;
                    from += o.tab_size - from % o.tab_size;
                } else {
                    out.write_all(b" ")?;
                    from += 1;
                }
            }
            Ok(())
        };
        if by_columns {
            let rows = n.div_ceil(cols);
            for row in 0..rows {
                let (mut col, mut fileno, mut pos) = (0, row, 0);
                loop {
                    let (cell, len) = &cells[fileno];
                    self.out.write_all(cell)?;
                    let max = widths[col];
                    col += 1;
                    fileno += rows;
                    if fileno >= n {
                        break;
                    }
                    indent(&mut self.out, pos + len, pos + max)?;
                    pos += max;
                }
                self.out.write_all(&[o.eol])?;
            }
        } else {
            let mut pos = 0;
            self.out.write_all(&cells[0].0)?;
            let (mut len, mut max) = (cells[0].1, widths[0]);
            for (fileno, (cell, cell_len)) in cells.iter().enumerate().skip(1) {
                let col = fileno % cols;
                if col == 0 {
                    self.out.write_all(&[o.eol])?;
                    pos = 0;
                } else {
                    indent(&mut self.out, pos + len, pos + max)?;
                    pos += max;
                }
                self.out.write_all(cell)?;
                len = *cell_len;
                max = widths[col];
            }
            self.out.write_all(&[o.eol])?;
        }
        Ok(())
    }

    /// Inode, block count, context, name and indicator for the non-long formats,
    /// with the display width of the result.
    fn short_cell(&self, f: &LsFile, w: &LsWidths, commas: bool) -> (Vec<u8>, usize) {
        let o = self.opts;
        let mut cell = Vec::new();
        let pad_left = |cell: &mut Vec<u8>, s: &str, width: usize| {
            if !commas {
                cell.resize(cell.len() + width.saturating_sub(s.len()), b' ');
            }
            cell.extend_from_slice(s.as_bytes());
            cell.push(b' ');
        };
        if o.inode {
            let ino = f.meta.as_ref().map_or("?".into(), |m| m.ino().to_string());
            pad_left(&mut cell, &ino, w.inode);
        }
        if o.blocks {
            pad_left(&mut cell, &self.blocks_cell(f), w.blocks);
        }
        if o.context {
            let ctx = self.context_cell(f);
            cell.extend_from_slice(ctx.as_bytes());
            if !commas {
                cell.resize(cell.len() + w.context.saturating_sub(ctx.len()), b' ');
            }
            cell.push(b' ');
        }
        let (name, quoted) = self.quoted_name(&f.name);
        if w.some_quoted && !quoted {
            cell.push(b' ');
        }
        cell.extend_from_slice(&name);
        if let Some(c) = indicator_char(o.indicator, f.meta.as_ref(), f.type_letter) {
            cell.push(c);
        }
        let width = display_width(&cell);
        (cell, width)
    }

    fn long_line(&self, f: &LsFile, w: &LsWidths) -> Vec<u8> {
        use std::os::unix::fs::FileTypeExt;
        let o = self.opts;
        let mut line: Vec<u8> = Vec::with_capacity(96);
        let right = |line: &mut Vec<u8>, s: &str, width: usize| {
            line.resize(
                line.len() + width.saturating_sub(display_width(s.as_bytes())),
                b' ',
            );
            line.extend_from_slice(s.as_bytes());
        };
        let left = |line: &mut Vec<u8>, s: &str, width: usize| {
            line.extend_from_slice(s.as_bytes());
            line.resize(
                line.len() + width.saturating_sub(display_width(s.as_bytes())),
                b' ',
            );
        };
        if o.inode {
            let ino = f.meta.as_ref().map_or("?".into(), |m| m.ino().to_string());
            right(&mut line, &ino, w.inode);
            line.push(b' ');
        }
        if o.blocks {
            right(&mut line, &self.blocks_cell(f), w.blocks);
            line.push(b' ');
        }
        match &f.meta {
            Some(m) => line.extend_from_slice(format_mode_rwx(m.mode()).as_bytes()),
            None => {
                line.push(f.type_letter);
                line.extend_from_slice(b"?????????");
            }
        }
        if w.any_acl {
            line.push(acl_marker(f).unwrap_or(b' '));
        }
        line.push(b' ');
        let nlink = f
            .meta
            .as_ref()
            .map_or("?".into(), |m| m.nlink().to_string());
        right(&mut line, &nlink, w.nlink);
        line.push(b' ');
        let user_cell = |line: &mut Vec<u8>, group: bool, width: usize| {
            let (text, numeric) = self.owner_cell(f, group);
            if numeric {
                right(line, &text, width);
            } else {
                left(line, &text, width);
            }
            line.push(b' ');
        };
        if o.owner {
            user_cell(&mut line, false, w.owner);
        }
        if o.group {
            user_cell(&mut line, true, w.group);
        }
        if o.author {
            user_cell(&mut line, false, w.author);
        }
        if o.context {
            left(&mut line, &self.context_cell(f), w.context);
            line.push(b' ');
        }
        match &f.meta {
            Some(m) if m.file_type().is_char_device() || m.file_type().is_block_device() => {
                let blanks = w.size.saturating_sub(w.major + 2 + w.minor);
                right(
                    &mut line,
                    &dev_major(m.rdev()).to_string(),
                    w.major + blanks,
                );
                line.extend_from_slice(b", ");
                right(&mut line, &dev_minor(m.rdev()).to_string(), w.minor);
            }
            _ => right(&mut line, &self.size_cell(f), w.size),
        }
        line.push(b' ');
        match f.time(o.time).and_then(|t| self.format_time(t)) {
            Some(s) => line.extend_from_slice(s.as_bytes()),
            None => {
                let width = self.long_time_width();
                right(&mut line, "?", width);
            }
        }
        line.push(b' ');
        let (name, quoted) = self.quoted_name(&f.name);
        if w.some_quoted && !quoted {
            line.push(b' ');
        }
        line.extend_from_slice(&name);
        let is_link = f.meta.as_ref().is_some_and(|m| m.file_type().is_symlink());
        if is_link {
            if let Some(target) = &f.link_target {
                line.extend_from_slice(b" -> ");
                line.extend_from_slice(&self.quoted_name(target).0);
                if o.indicator != Indicator::None
                    && let Some(tm) = &f.target_meta
                    && let Some(c) = indicator_char(o.indicator, Some(tm), b'?')
                {
                    line.push(c);
                }
            }
        } else if let Some(c) = indicator_char(o.indicator, f.meta.as_ref(), f.type_letter) {
            line.push(c);
        }
        line
    }

    /// The `-l` timestamp: GNU's six-month "recent" rule picks the format.
    fn format_time(&self, t: Timespec) -> Option<String> {
        let mut now = self.opts.now.get();
        if now < t {
            now = Timespec::now();
            self.opts.now.set(now);
        }
        let six_months_ago = Timespec {
            sec: now.sec - 31_556_952 / 2,
            nsec: now.nsec,
        };
        let recent = six_months_ago < t && t < now;
        format_local_time(&self.opts.time_formats[usize::from(recent)], t)
    }

    /// Width GNU pads `?` times to: that of the epoch in the non-recent format.
    fn long_time_width(&self) -> usize {
        format_local_time(&self.opts.time_formats[0], Timespec::default())
            .map_or(0, |s| display_width(s.as_bytes()))
    }

    /// The Dogfish table / grid used on a terminal.
    fn print_rich(&mut self, files: &[&LsFile], st: Style) -> io::Result<()> {
        let o = self.opts;
        let entries: Vec<LsEntry> = files.iter().map(|f| ls_json_entry(f)).collect();
        let classify = o.indicator != Indicator::None;
        if o.format == LsFormat::Long {
            let mut tbl = Table::new(vec![
                ("PERMS", ColumnAlign::Left),
                ("OWNER", ColumnAlign::Left),
                ("SIZE", ColumnAlign::Right),
                ("MODIFIED", ColumnAlign::Right),
                ("NAME", ColumnAlign::Left),
            ]);
            for (e, f) in entries.iter().zip(files) {
                let size = if e.kind == "dir" {
                    st.dim("-")
                } else if o.file_human_opts & HUMAN_AUTOSCALE == 0 && o.file_block_size != 1 {
                    st.green(&self.size_cell(f))
                } else {
                    st.green(&format_size(e.size, true))
                };
                let mut name = style_entry_name(&e.name, &e.kind, &e.mode, classify, st);
                if let Some(target) = &f.link_target {
                    name = format!("{name} -> {}", st.magenta(&String::from_utf8_lossy(target)));
                }
                tbl.add_row(vec![
                    colorize_mode(&e.mode, st),
                    st.dim(&e.user),
                    size,
                    st.cyan(&e.modified_relative),
                    name,
                ]);
            }
            return tbl.render(&mut self.out, st);
        }
        let styled: Vec<(String, usize)> = entries
            .iter()
            .map(|e| {
                let s = style_entry_name(&e.name, &e.kind, &e.mode, classify, st);
                let w = dogfish_core::table::visible_width(&s);
                (s, w)
            })
            .collect();
        if o.format == LsFormat::OnePerLine {
            for (s, _) in &styled {
                writeln!(self.out, "{s}")?;
            }
            return Ok(());
        }
        let max_w = styled.iter().map(|(_, w)| *w).max().unwrap_or(8) + 3;
        let cols = (88 / max_w.max(1)).max(1);
        for chunk in styled.chunks(cols) {
            for (s, w) in chunk {
                write!(self.out, "{}{}", s, " ".repeat(max_w.saturating_sub(*w)))?;
            }
            writeln!(self.out)?;
        }
        Ok(())
    }
}

/// `+` for a POSIX ACL, `.` for an SELinux context only (GNU's mode suffix).
fn acl_marker(f: &LsFile) -> Option<u8> {
    let m = f.meta.as_ref()?;
    if m.file_type().is_symlink() {
        return None;
    }
    let has_acl = lgetxattr_bytes(&f.path, "system.posix_acl_access").is_some()
        || (m.is_dir() && lgetxattr_bytes(&f.path, "system.posix_acl_default").is_some());
    if has_acl {
        Some(b'+')
    } else if security_context(&f.path).is_some() {
        Some(b'.')
    } else {
        None
    }
}

/// GNU `get_type_indicator`.
fn indicator_char(style: Indicator, meta: Option<&fs::Metadata>, letter: u8) -> Option<u8> {
    use std::os::unix::fs::FileTypeExt;
    if style == Indicator::None {
        return None;
    }
    let (regular, dir, link, fifo, sock) = match meta {
        Some(m) => {
            let ft = m.file_type();
            (
                ft.is_file(),
                ft.is_dir(),
                ft.is_symlink(),
                ft.is_fifo(),
                ft.is_socket(),
            )
        }
        None => (
            letter == b'-',
            letter == b'd',
            letter == b'l',
            letter == b'p',
            letter == b's',
        ),
    };
    if regular {
        let exec = meta.is_some_and(|m| m.mode() & 0o111 != 0);
        return (style == Indicator::Classify && exec).then_some(b'*');
    }
    if dir {
        Some(b'/')
    } else if style == Indicator::Slash {
        None
    } else if link {
        Some(b'@')
    } else if fifo {
        Some(b'|')
    } else if sock {
        Some(b'=')
    } else {
        None
    }
}

fn ls_json_entry(f: &LsFile) -> LsEntry {
    let name = String::from_utf8_lossy(&f.name).into_owned();
    let path = f.path.to_string_lossy().into_owned();
    let Some(meta) = &f.meta else {
        return LsEntry {
            name,
            path,
            kind: "unknown".into(),
            size: 0,
            mode: "?????????".into(),
            mode_octal: "?".into(),
            user: "?".into(),
            group: "?".into(),
            modified_unix: 0,
            modified_relative: "?".into(),
        };
    };
    let mode = meta.mode();
    let kind = if meta.is_dir() {
        "dir"
    } else if meta.file_type().is_symlink() {
        "symlink"
    } else if mode & 0o111 != 0 {
        "exe"
    } else {
        "file"
    };
    let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let (uid, gid) = (meta.uid(), meta.gid());
    LsEntry {
        name,
        path,
        kind: kind.to_string(),
        size: meta.len(),
        mode: format_mode_rwx(mode),
        mode_octal: format!("{:04o}", mode & 0o7777),
        user: get_username_by_uid(uid).unwrap_or_else(|| uid.to_string()),
        group: get_groupname_by_gid(gid).unwrap_or_else(|| gid.to_string()),
        modified_unix: meta.mtime(),
        modified_relative: format_relative_time(mtime),
    }
}

fn style_entry_name(name: &str, kind: &str, mode: &str, classify: bool, st: Style) -> String {
    let suffix = if classify {
        match kind {
            "dir" => "/",
            "symlink" => "@",
            "exe" => "*",
            _ => "",
        }
    } else {
        ""
    };
    let label = format!("{}{}", name, suffix);
    match kind {
        "dir" => st.bold_blue(&label),
        "symlink" => st.magenta(&label),
        "exe" => st.bold_green(&label),
        _ if mode.starts_with('p') || mode.starts_with('s') => st.yellow(&label),
        _ => label,
    }
}

fn colorize_mode(mode: &str, st: Style) -> String {
    let mut out = String::new();
    for ch in mode.chars() {
        match ch {
            'd' => out.push_str(&st.bold_blue("d")),
            'l' => out.push_str(&st.magenta("l")),
            'r' => out.push_str(&st.yellow("r")),
            'w' => out.push_str(&st.red("w")),
            'x' => out.push_str(&st.green("x")),
            '-' => out.push_str(&st.dim("-")),
            other => out.push(other),
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn render_tree<W: Write>(
    dir: &Path,
    prefix: &str,
    depth: usize,
    max_depth: usize,
    show_all: bool,
    out: &mut W,
    st: Style,
    ctx: &DogfishContext,
) -> io::Result<()> {
    if depth >= max_depth {
        return Ok(());
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return Ok(());
    };
    let mut items: Vec<_> = rd
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            show_all || !n.starts_with('.')
        })
        .collect();
    items.sort_by_key(|e| e.file_name());

    let count = items.len();
    for (idx, entry) in items.into_iter().enumerate() {
        let is_last = idx + 1 == count;
        let branch = if is_last { "└── " } else { "├── " };
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let styled = if is_dir {
            st.bold_blue(&format!("{}/", name))
        } else {
            name
        };
        writeln!(out, "{}{}{}", st.dim(prefix), st.dim(branch), styled)?;
        ctx.telemetry.add_items(1);
        if is_dir {
            let next_prefix = format!("{}{}", prefix, if is_last { "    " } else { "│   " });
            render_tree(
                &path,
                &next_prefix,
                depth + 1,
                max_depth,
                show_all,
                out,
                st,
                ctx,
            )?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// GNU compatibility helpers shared by stat, df, du and ls: `getopt_long`-style
// option parsing, `human_readable` number formatting, `--block-size` parsing,
// `strftime`, `quotearg` file-name quoting and `fnmatch` patterns.
// ---------------------------------------------------------------------------

/// Whether a GNU option takes an argument.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OptArg {
    No,
    Required,
    /// Long form only, and only as `--name=VALUE`.
    Optional,
}

/// One accepted option: `key` identifies it to the tool, `short`/`long` are its spellings.
struct OptSpec {
    key: &'static str,
    short: Option<char>,
    long: Option<&'static str>,
    arg: OptArg,
}

impl OptSpec {
    const fn new(
        key: &'static str,
        short: Option<char>,
        long: Option<&'static str>,
        arg: OptArg,
    ) -> Self {
        Self {
            key,
            short,
            long,
            arg,
        }
    }
}

/// One option occurrence, in command-line order.
struct GnuOpt {
    key: &'static str,
    value: Option<String>,
    /// How GNU names the option in diagnostics: `-B` or `--block-size`.
    spelled: String,
}

struct GnuArgs {
    opts: Vec<GnuOpt>,
    operands: Vec<String>,
}

/// Parses `args` (without `argv[0]`) the way glibc's `getopt_long` does: short
/// option clusters, attached or separate option arguments, unambiguous long
/// option prefixes, `--` and argument permutation (unless `POSIXLY_CORRECT`).
/// The error is the diagnostic glibc would print after `tool: `.
fn gnu_getopt(specs: &[OptSpec], args: &[String]) -> Result<GnuArgs, String> {
    let posixly_correct = std::env::var_os("POSIXLY_CORRECT").is_some();
    let mut opts = Vec::new();
    let mut operands = Vec::new();
    let mut only_operands = false;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        if only_operands || arg == "-" || !arg.starts_with('-') {
            operands.push(arg.clone());
            only_operands |= posixly_correct;
            continue;
        }
        if arg == "--" {
            only_operands = true;
            continue;
        }
        if let Some(body) = arg.strip_prefix("--") {
            let (name, inline) = match body.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (body, None),
            };
            let spec = match specs.iter().find(|s| s.long == Some(name)) {
                Some(s) => s,
                None => {
                    let candidates: Vec<&OptSpec> = specs
                        .iter()
                        .filter(|s| s.long.is_some_and(|l| l.starts_with(name)))
                        .collect();
                    match candidates.as_slice() {
                        [] => return Err(format!("unrecognized option '{arg}'")),
                        [first, rest @ ..] if rest.iter().all(|s| s.key == first.key) => *first,
                        many => {
                            let mut msg = format!("option '{arg}' is ambiguous; possibilities:");
                            for s in many {
                                msg.push_str(&format!(" '--{}'", s.long.unwrap_or_default()));
                            }
                            return Err(msg);
                        }
                    }
                }
            };
            let long = spec.long.unwrap_or_default();
            let value = match spec.arg {
                OptArg::No => {
                    if inline.is_some() {
                        return Err(format!("option '--{long}' doesn't allow an argument"));
                    }
                    None
                }
                OptArg::Optional => inline.map(str::to_string),
                OptArg::Required => match inline {
                    Some(v) => Some(v.to_string()),
                    None if i < args.len() => {
                        i += 1;
                        Some(args[i - 1].clone())
                    }
                    None => return Err(format!("option '--{long}' requires an argument")),
                },
            };
            opts.push(GnuOpt {
                key: spec.key,
                value,
                spelled: format!("--{long}"),
            });
            continue;
        }
        let cluster = &arg[1..];
        for (pos, c) in cluster.char_indices() {
            let Some(spec) = specs.iter().find(|s| s.short == Some(c)) else {
                return Err(format!("invalid option -- '{c}'"));
            };
            let spelled = format!("-{c}");
            // Optional arguments exist only as `--name=VALUE`.
            if spec.arg != OptArg::Required {
                opts.push(GnuOpt {
                    key: spec.key,
                    value: None,
                    spelled,
                });
                continue;
            }
            let rest = &cluster[pos + c.len_utf8()..];
            let value = if !rest.is_empty() {
                rest.to_string()
            } else if i < args.len() {
                i += 1;
                args[i - 1].clone()
            } else {
                return Err(format!("option requires an argument -- '{c}'"));
            };
            opts.push(GnuOpt {
                key: spec.key,
                value: Some(value),
                spelled,
            });
            break;
        }
    }
    Ok(GnuArgs { opts, operands })
}

/// Prints a GNU usage diagnostic and returns the exit status to use.
fn gnu_usage_error(tool: &str, msg: &str, status: i32) -> i32 {
    eprintln!("{tool}: {msg}\nTry '{tool} --help' for more information.");
    status
}

fn print_tool_version(tool: &str) {
    println!("{} (GNU/Dogfish) {}", tool, env!("CARGO_PKG_VERSION"));
}

/// Writes `--help` text, tolerating a closed pipe.
fn print_help_text(text: &str) -> Result<i32> {
    ignore_broken_pipe(io::stdout().lock().write_all(text.as_bytes()))?;
    Ok(0)
}

/// Prints GNU's "Try 'TOOL --help'" hint and returns `status`.
fn try_help(tool: &str, status: i32) -> i32 {
    eprintln!("Try '{tool} --help' for more information.");
    status
}

/// Resolves `--opt=WORD` like GNU `argmatch`: an exact match or a prefix that
/// selects one meaning. `choices` pairs each accepted word with its meaning;
/// synonyms are adjacent. On failure, prints GNU's "invalid/ambiguous argument"
/// block (the caller adds the `--help` hint).
fn gnu_argmatch<'a>(
    tool: &str,
    what: &str,
    value: &str,
    choices: &[(&str, &'a str)],
) -> Option<&'a str> {
    if let Some((_, meaning)) = choices.iter().find(|(word, _)| *word == value) {
        return Some(meaning);
    }
    let matches: Vec<&'a str> = choices
        .iter()
        .filter(|(word, _)| !value.is_empty() && word.starts_with(value))
        .map(|(_, meaning)| *meaning)
        .collect();
    if let Some(first) = matches.first()
        && matches.iter().all(|m| m == first)
    {
        return Some(first);
    }
    let kind = if matches.is_empty() {
        "invalid"
    } else {
        "ambiguous"
    };
    let mut msg = format!(
        "{tool}: {kind} argument {} for {}\nValid arguments are:",
        quote_bytes(value.as_bytes(), Quoting::Locale),
        quote_bytes(what.as_bytes(), Quoting::Locale)
    );
    let mut previous: Option<&str> = None;
    for (word, meaning) in choices {
        let quoted = quote_bytes(word.as_bytes(), Quoting::Locale);
        if previous == Some(*meaning) {
            msg.push_str(&format!(", {quoted}"));
        } else {
            msg.push_str(&format!("\n  - {quoted}"));
        }
        previous = Some(meaning);
    }
    eprintln!("{msg}");
    None
}

// ----- human_readable ------------------------------------------------------

const HUMAN_AUTOSCALE: u32 = 1;
const HUMAN_SI: u32 = 2;
const HUMAN_BASE_1024: u32 = 4;
const HUMAN_B: u32 = 8;
const HUMAN_SUPPRESS_POINT_ZERO: u32 = 16;

/// Size/prefix letters indexed by power, as in gnulib (`k` is used for 1000¹).
const POWER_LETTERS: [char; 11] = ['\0', 'K', 'M', 'G', 'T', 'P', 'E', 'Z', 'Y', 'R', 'Q'];

/// gnulib `human_readable` with its default (ceiling) rounding: converts `n`
/// units of `from` bytes into units of `to` bytes, autoscaling with `-h`/`--si`.
fn human_readable(n: u64, opts: u32, from: u64, to: u64) -> String {
    let autoscale = opts & HUMAN_AUTOSCALE != 0;
    let base: u64 = if opts & HUMAN_BASE_1024 != 0 {
        1024
    } else {
        1000
    };
    let max_exp = POWER_LETTERS.len() as i32 - 1;
    let mut exponent: i32 = -1;

    let mut start = None;
    if to <= from {
        if from.is_multiple_of(to) {
            start = n.checked_mul(from / to).map(|amount| (amount, 0, 0));
        }
    } else if from != 0 && to.is_multiple_of(from) {
        let divisor = u128::from(to / from);
        let n = u128::from(n);
        let r10 = (n % divisor) * 10;
        let r2 = (r10 % divisor) * 2;
        let rounding = if r2 < divisor {
            u64::from(0 < r2)
        } else {
            2 + u64::from(divisor < r2)
        };
        start = Some(((n / divisor) as u64, (r10 / divisor) as u64, rounding));
    }
    let Some((mut amount, mut tenths, mut rounding)) = start else {
        return human_readable_float(n, opts, from, to);
    };

    let mut fraction = String::new();
    if autoscale {
        exponent = 0;
        if base <= amount {
            loop {
                let r10 = (amount % base) * 10 + tenths;
                let r2 = (r10 % base) * 2 + (rounding >> 1);
                amount /= base;
                tenths = r10 / base;
                rounding = if r2 < base {
                    u64::from(r2 + rounding != 0)
                } else {
                    2 + u64::from(base < r2 + rounding)
                };
                exponent += 1;
                if !(base <= amount && exponent < max_exp) {
                    break;
                }
            }
            if amount < 10 {
                if 0 < rounding {
                    tenths += 1;
                    rounding = 0;
                    if tenths == 10 {
                        amount += 1;
                        tenths = 0;
                    }
                }
                if amount < 10 && (tenths != 0 || opts & HUMAN_SUPPRESS_POINT_ZERO == 0) {
                    fraction = format!(".{tenths}");
                    tenths = 0;
                    rounding = 0;
                }
            }
        }
    }
    if 0 < tenths + rounding {
        amount += 1;
        if autoscale && amount == base && exponent < max_exp {
            exponent += 1;
            if opts & HUMAN_SUPPRESS_POINT_ZERO == 0 {
                fraction = ".0".to_string();
            }
            amount = 1;
        }
    }
    let mut out = amount.to_string();
    out.push_str(&fraction);
    push_human_suffix(&mut out, opts, exponent, base, to);
    out
}

/// The floating-point fallback gnulib uses when the block sizes do not divide.
fn human_readable_float(n: u64, opts: u32, from: u64, to: u64) -> String {
    let base: u64 = if opts & HUMAN_BASE_1024 != 0 {
        1024
    } else {
        1000
    };
    let mut amount = n as f64 * (from as f64 / to as f64);
    let mut exponent = -1;
    let mut out = if opts & HUMAN_AUTOSCALE == 0 {
        format!("{:.0}", amount.ceil())
    } else {
        exponent = 0;
        let mut scale = 1.0;
        loop {
            scale *= base as f64;
            exponent += 1;
            if !(scale * base as f64 <= amount && exponent < POWER_LETTERS.len() as i32 - 1) {
                break;
            }
        }
        amount /= scale;
        let tenths = (amount * 10.0).ceil() / 10.0;
        if tenths < 10.0 {
            format!("{tenths:.1}")
        } else {
            format!("{:.0}", amount.ceil())
        }
    };
    push_human_suffix(&mut out, opts, exponent, base, to);
    out
}

fn push_human_suffix(out: &mut String, opts: u32, mut exponent: i32, base: u64, to: u64) {
    if opts & HUMAN_SI == 0 {
        return;
    }
    if exponent < 0 {
        exponent = 0;
        let mut power: u64 = 1;
        while power < to {
            power = power.saturating_mul(base);
            exponent += 1;
            if exponent == POWER_LETTERS.len() as i32 - 1 {
                break;
            }
        }
    }
    if exponent > 0 {
        out.push(if base == 1000 && exponent == 1 {
            'k'
        } else {
            POWER_LETTERS[exponent as usize]
        });
    }
    if opts & HUMAN_B != 0 {
        if base == 1024 && exponent > 0 {
            out.push('i');
        }
        out.push('B');
    }
}

/// Why a `--block-size`-style argument was rejected (gnulib `strtol_error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SizeError {
    Invalid,
    InvalidSuffix,
    TooLarge,
}

impl SizeError {
    /// GNU's `xstrtol_fatal` wording, e.g. `invalid suffix in -B argument '1x'`.
    fn message(self, option: &str, value: &str) -> String {
        let value = quote_bytes(value.as_bytes(), Quoting::Locale);
        match self {
            Self::Invalid => format!("invalid {option} argument {value}"),
            Self::InvalidSuffix => format!("invalid suffix in {option} argument {value}"),
            Self::TooLarge => format!("{option} argument {value} too large"),
        }
    }
}

/// gnulib `xstrtoumax` with the `"eEgGkKmMpPtTyYzZ0"` suffixes used for sizes.
fn parse_size_suffixed(s: &str) -> Result<u64, SizeError> {
    let bytes = s.as_bytes();
    if s.trim_start().starts_with('-') {
        return Err(SizeError::Invalid);
    }
    let (radix, digits_start) = if bytes.len() > 2
        && bytes[0] == b'0'
        && (bytes[1] | 0x20) == b'x'
        && bytes[2].is_ascii_hexdigit()
    {
        (16, 2)
    } else if bytes.first() == Some(&b'0') {
        (8, 0)
    } else {
        (10, 0)
    };
    let digits_len = bytes[digits_start..]
        .iter()
        .take_while(|b| (**b as char).is_digit(radix))
        .count();
    let mut overflow = false;
    let mut value: u64 = 1;
    let mut pos = digits_start + digits_len;
    if digits_len == 0 {
        if bytes.is_empty() || !b"eEgGkKmMpPtTyYzZ".contains(&bytes[0]) {
            return Err(SizeError::Invalid);
        }
        pos = 0;
    } else {
        match u64::from_str_radix(&s[digits_start..pos], radix) {
            Ok(v) => value = v,
            Err(_) => {
                overflow = true;
                value = u64::MAX;
            }
        }
    }
    if pos == bytes.len() {
        return if overflow {
            Err(SizeError::TooLarge)
        } else {
            Ok(value)
        };
    }
    let suffix = bytes[pos];
    let mut base: u64 = 1024;
    let mut consumed = 1;
    match bytes.get(pos + 1) {
        Some(b'i') if bytes.get(pos + 2) == Some(&b'B') => consumed = 3,
        Some(b'B') | Some(b'D') => {
            base = 1000;
            consumed = 2;
        }
        _ => {}
    }
    let power = match suffix {
        b'k' | b'K' => 1,
        b'M' | b'm' => 2,
        b'G' | b'g' => 3,
        b'T' | b't' => 4,
        b'P' => 5,
        b'E' => 6,
        b'Z' => 7,
        b'Y' => 8,
        _ => return Err(SizeError::InvalidSuffix),
    };
    for _ in 0..power {
        match value.checked_mul(base) {
            Some(v) => value = v,
            None => overflow = true,
        }
    }
    if pos + consumed != bytes.len() {
        return Err(SizeError::InvalidSuffix);
    }
    if overflow {
        Err(SizeError::TooLarge)
    } else {
        Ok(value)
    }
}

/// gnulib `human_options`: parses a block size such as `1K`, `M`, `KB`, `si` or
/// `human-readable` into `(human_readable opts, output block size)`.
fn parse_block_size(spec: &str) -> Result<(u32, u64), SizeError> {
    let spec = spec.strip_prefix('\'').unwrap_or(spec);
    for (word, opts) in [
        (
            "human-readable",
            HUMAN_AUTOSCALE | HUMAN_SI | HUMAN_BASE_1024,
        ),
        ("si", HUMAN_AUTOSCALE | HUMAN_SI),
    ] {
        if !spec.is_empty() && word.starts_with(spec) {
            return Ok((opts, 1));
        }
    }
    let size = parse_size_suffixed(spec)?;
    if size == 0 {
        return Err(SizeError::Invalid);
    }
    let mut opts = 0;
    // A spec without leading digits ("K", "MiB", "kB") also prints the unit.
    if !spec.starts_with(|c: char| c.is_ascii_digit()) {
        opts |= HUMAN_SI;
        if spec.ends_with('B') {
            opts |= HUMAN_B;
        }
        if !spec.ends_with('B') || spec.ends_with("iB") {
            opts |= HUMAN_BASE_1024;
        }
    }
    Ok((opts, size))
}

/// The default output block size for a tool honoring `tool_var` (e.g.
/// `DU_BLOCK_SIZE`), then `BLOCK_SIZE`, `BLOCKSIZE` and `POSIXLY_CORRECT`.
fn env_block_size(tool_var: &str) -> (u32, u64) {
    let spec = std::env::var(tool_var)
        .or_else(|_| std::env::var("BLOCK_SIZE"))
        .or_else(|_| std::env::var("BLOCKSIZE"));
    if let Ok(spec) = spec
        && let Ok(parsed) = parse_block_size(&spec)
    {
        return parsed;
    }
    let default = if std::env::var_os("POSIXLY_CORRECT").is_some() {
        512
    } else {
        1024
    };
    (0, default)
}

// ----- time formatting -----------------------------------------------------

/// Seconds and nanoseconds since the epoch.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
struct Timespec {
    sec: i64,
    nsec: i64,
}

impl Timespec {
    fn now() -> Self {
        Self::from_system(SystemTime::now())
    }

    fn from_system(t: SystemTime) -> Self {
        match t.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(d) => Self {
                sec: d.as_secs() as i64,
                nsec: i64::from(d.subsec_nanos()),
            },
            Err(e) => {
                let d = e.duration();
                let (mut sec, mut nsec) = (-(d.as_secs() as i64), -i64::from(d.subsec_nanos()));
                if nsec < 0 {
                    sec -= 1;
                    nsec += 1_000_000_000;
                }
                Self { sec, nsec }
            }
        }
    }
}

/// Broken-down local time via the C library, so `TZ` handling and zone
/// abbreviations match GNU exactly.
fn local_tm(sec: i64) -> Option<libc::tm> {
    let t = sec as libc::time_t;
    // SAFETY: `tm` is plain data; localtime_r only writes into it.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            None
        } else {
            Some(tm)
        }
    }
}

fn tm_zone_name(tm: &libc::tm) -> String {
    if tm.tm_zone.is_null() {
        return String::new();
    }
    // SAFETY: a non-null tm_zone from localtime_r points at a static C string.
    unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) }
        .to_string_lossy()
        .into_owned()
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// ISO 8601 week-based `(year, week)` for `%G`/`%V`.
fn iso_week(tm: &libc::tm) -> (i64, i64) {
    fn week_days(yday: i64, wday: i64) -> i64 {
        const BIG_ENOUGH_MULTIPLE_OF_7: i64 = (366 / 7 + 2) * 7;
        yday - (yday - wday + 4 + BIG_ENOUGH_MULTIPLE_OF_7) % 7 + 3
    }
    let mut year = i64::from(tm.tm_year) + 1900;
    let (yday, wday) = (i64::from(tm.tm_yday), i64::from(tm.tm_wday));
    let mut days = week_days(yday, wday);
    if days < 0 {
        year -= 1;
        days = week_days(yday + 365 + i64::from(is_leap_year(year)), wday);
    } else {
        let next = week_days(yday - (365 + i64::from(is_leap_year(year))), wday);
        if next >= 0 {
            year += 1;
            days = next;
        }
    }
    (year, days / 7 + 1)
}

/// GNU `nstrftime` for the C locale, including `%N` (nanoseconds), `%:z`, and
/// the `_ - 0 ^ #` flags with field widths. `t` is the time `tm` describes.
fn gnu_strftime(fmt: &str, tm: &libc::tm, t: Timespec) -> String {
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let mut spec = String::from("%");
        let mut pad: Option<char> = None;
        let mut upper = false;
        let mut swap_case = false;
        while let Some(&f) = chars.peek() {
            match f {
                '_' => pad = Some(' '),
                '-' => pad = Some('\0'),
                '0' => pad = Some('0'),
                '^' => upper = true,
                '#' => swap_case = true,
                _ => break,
            }
            spec.push(f);
            chars.next();
        }
        let mut width: Option<usize> = None;
        while let Some(&d) = chars.peek().filter(|d| d.is_ascii_digit()) {
            width = Some(width.unwrap_or(0) * 10 + d.to_digit(10).unwrap_or(0) as usize);
            spec.push(d);
            chars.next();
        }
        while let Some(&m) = chars.peek().filter(|m| **m == 'E' || **m == 'O') {
            spec.push(m);
            chars.next();
        }
        let mut colons = 0;
        while chars.peek() == Some(&':') {
            colons += 1;
            spec.push(':');
            chars.next();
        }
        let Some(conv) = chars.next() else {
            out.push_str(&spec);
            break;
        };
        let num = |value: i64, digits: usize, default_pad: char| -> String {
            let pad_char = pad.unwrap_or(default_pad);
            let w = width.unwrap_or(digits);
            let body = value.unsigned_abs().to_string();
            let sign = if value < 0 { "-" } else { "" };
            if pad_char == '\0' {
                return format!("{sign}{body}");
            }
            let len = sign.len() + body.len();
            if len >= w {
                format!("{sign}{body}")
            } else if pad_char == '0' {
                format!("{sign}{}{body}", "0".repeat(w - len))
            } else {
                format!("{}{sign}{body}", " ".repeat(w - len))
            }
        };
        let text = |s: &str, is_lower_default: bool| -> String {
            let mut s = if upper || (swap_case && !is_lower_default) {
                s.to_uppercase()
            } else if swap_case {
                s.to_lowercase()
            } else {
                s.to_string()
            };
            if let Some(w) = width
                && s.len() < w
            {
                let p = if pad == Some('0') { "0" } else { " " };
                s = format!("{}{s}", p.repeat(w - s.len()));
            }
            s
        };
        let year = i64::from(tm.tm_year) + 1900;
        let hour12 = match tm.tm_hour % 12 {
            0 => 12,
            h => i64::from(h),
        };
        let month = tm.tm_mon.clamp(0, 11) as usize;
        let wday = tm.tm_wday.clamp(0, 6) as usize;
        let piece = match conv {
            'a' => text(&WEEKDAYS[wday][..3], false),
            'A' => text(WEEKDAYS[wday], false),
            'b' | 'h' => text(&MONTHS[month][..3], false),
            'B' => text(MONTHS[month], false),
            'c' => text(&gnu_strftime("%a %b %e %H:%M:%S %Y", tm, t), false),
            'C' => num(year.div_euclid(100), 2, '0'),
            'd' => num(i64::from(tm.tm_mday), 2, '0'),
            'D' | 'x' => text(&gnu_strftime("%m/%d/%y", tm, t), false),
            'e' => num(i64::from(tm.tm_mday), 2, ' '),
            'F' => text(&gnu_strftime("%Y-%m-%d", tm, t), false),
            'g' => num(iso_week(tm).0.rem_euclid(100), 2, '0'),
            'G' => num(iso_week(tm).0, 1, '0'),
            'H' => num(i64::from(tm.tm_hour), 2, '0'),
            'I' => num(hour12, 2, '0'),
            'j' => num(i64::from(tm.tm_yday) + 1, 3, '0'),
            'k' => num(i64::from(tm.tm_hour), 2, ' '),
            'l' => num(hour12, 2, ' '),
            'm' => num(i64::from(tm.tm_mon) + 1, 2, '0'),
            'M' => num(i64::from(tm.tm_min), 2, '0'),
            'n' => "\n".to_string(),
            'N' => {
                let digits = format!("{:09}", t.nsec);
                let w = width.unwrap_or(9);
                if w <= 9 {
                    digits[..w].to_string()
                } else {
                    format!("{digits}{}", "0".repeat(w - 9))
                }
            }
            'p' => text(if tm.tm_hour < 12 { "AM" } else { "PM" }, true),
            'P' => text(if tm.tm_hour < 12 { "am" } else { "pm" }, true),
            'r' => text(&gnu_strftime("%I:%M:%S %p", tm, t), false),
            'R' => text(&gnu_strftime("%H:%M", tm, t), false),
            's' => num(t.sec, 1, '0'),
            'S' => num(i64::from(tm.tm_sec), 2, '0'),
            't' => "\t".to_string(),
            'T' | 'X' => text(&gnu_strftime("%H:%M:%S", tm, t), false),
            'u' => num(((wday + 6) % 7 + 1) as i64, 1, '0'),
            'U' => num((i64::from(tm.tm_yday) + 7 - wday as i64) / 7, 2, '0'),
            'V' => num(iso_week(tm).1, 2, '0'),
            'w' => num(wday as i64, 1, '0'),
            'W' => num(
                (i64::from(tm.tm_yday) + 7 - ((wday as i64 + 6) % 7)) / 7,
                2,
                '0',
            ),
            'y' => num(year.rem_euclid(100), 2, '0'),
            'Y' => num(year, 1, '0'),
            'z' => {
                #[allow(clippy::useless_conversion)] // `c_long` is 32-bit on some targets
                let off = i64::from(tm.tm_gmtoff);
                let sign = if off < 0 { '-' } else { '+' };
                let off = off.abs();
                let (h, m, s) = (off / 3600, off / 60 % 60, off % 60);
                match colons {
                    0 => format!("{sign}{h:02}{m:02}"),
                    1 => format!("{sign}{h:02}:{m:02}"),
                    2 => format!("{sign}{h:02}:{m:02}:{s:02}"),
                    _ if s != 0 => format!("{sign}{h:02}:{m:02}:{s:02}"),
                    _ if m != 0 => format!("{sign}{h:02}:{m:02}"),
                    _ => format!("{sign}{h:02}"),
                }
            }
            'Z' => text(&tm_zone_name(tm), true),
            '%' => "%".to_string(),
            other => {
                spec.push(other);
                spec
            }
        };
        out.push_str(&piece);
    }
    out
}

/// Formats `t` in local time with a GNU `strftime` format.
fn format_local_time(fmt: &str, t: Timespec) -> Option<String> {
    local_tm(t.sec).map(|tm| gnu_strftime(fmt, &tm, t))
}

// ----- file-name quoting (gnulib quotearg) ---------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Quoting {
    Literal,
    Shell,
    ShellAlways,
    ShellEscape,
    ShellEscapeAlways,
    C,
    CMaybe,
    Escape,
    Locale,
    CLocale,
}

impl Quoting {
    const NAMES: [&'static str; 10] = [
        "literal",
        "shell",
        "shell-always",
        "shell-escape",
        "shell-escape-always",
        "c",
        "c-maybe",
        "escape",
        "locale",
        "clocale",
    ];

    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "literal" => Self::Literal,
            "shell" => Self::Shell,
            "shell-always" => Self::ShellAlways,
            "shell-escape" => Self::ShellEscape,
            "shell-escape-always" => Self::ShellEscapeAlways,
            "c" => Self::C,
            "c-maybe" => Self::CMaybe,
            "escape" => Self::Escape,
            "locale" => Self::Locale,
            "clocale" => Self::CLocale,
            _ => return None,
        })
    }
}

/// True when the character-type locale is UTF-8, so multibyte characters
/// count as printable (otherwise every non-ASCII byte is, as in the C locale).
fn locale_is_utf8() -> bool {
    static UTF8: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *UTF8.get_or_init(|| {
        let value = ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .filter_map(|v| std::env::var(v).ok())
            .find(|v| !v.is_empty())
            .unwrap_or_default()
            .to_ascii_lowercase();
        value.contains("utf-8") || value.contains("utf8")
    })
}

/// Length and printability of the character starting `bytes`, per the locale.
fn char_at(bytes: &[u8]) -> (usize, bool) {
    let c = bytes[0];
    if c < 0x80 || !locale_is_utf8() {
        return (1, (0x20..0x7f).contains(&c));
    }
    let len = match c {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return (1, false),
    };
    match bytes.get(..len).map(std::str::from_utf8) {
        Some(Ok(s)) => (len, s.chars().all(|ch| !ch.is_control())),
        _ => (1, false),
    }
}

/// Display width of an already-quoted name (`mbsnwidth`): control characters
/// take no columns.
fn display_width(bytes: &[u8]) -> usize {
    // In single-byte locales GNU ls counts only printable bytes.
    if !locale_is_utf8() {
        return bytes.iter().filter(|b| (0x20..0x7f).contains(*b)).count();
    }
    String::from_utf8_lossy(bytes)
        .chars()
        .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

/// Quotes `arg` like gnulib `quotearg_buffer` in `style`.
fn quote_bytes(arg: &[u8], style: Quoting) -> String {
    String::from_utf8_lossy(&quote_bytes_raw(arg, style, b"")).into_owned()
}

/// `quotearg_buffer_restyled`; `extra` are additional bytes to backslash-escape.
fn quote_bytes_raw(arg: &[u8], style: Quoting, extra: &[u8]) -> Vec<u8> {
    let utf8 = locale_is_utf8();
    let (open, close): (&[u8], &[u8]) = match style {
        Quoting::C | Quoting::CMaybe => (b"\"", b"\""),
        Quoting::Locale if utf8 => ("\u{2018}".as_bytes(), "\u{2019}".as_bytes()),
        Quoting::CLocale if utf8 => ("\u{2018}".as_bytes(), "\u{2019}".as_bytes()),
        Quoting::Locale => (b"'", b"'"),
        Quoting::CLocale => (b"\"", b"\""),
        Quoting::Shell
        | Quoting::ShellAlways
        | Quoting::ShellEscape
        | Quoting::ShellEscapeAlways => (b"'", b"'"),
        Quoting::Literal | Quoting::Escape => (b"", b""),
    };
    if style == Quoting::Literal {
        return arg.to_vec();
    }
    let shell = matches!(
        style,
        Quoting::Shell | Quoting::ShellAlways | Quoting::ShellEscape | Quoting::ShellEscapeAlways
    );
    // "Elided" styles add the outer quotes only when something needs them.
    let elide = matches!(
        style,
        Quoting::Shell | Quoting::ShellEscape | Quoting::CMaybe
    );
    let backslash = !matches!(style, Quoting::Shell | Quoting::ShellAlways);
    let force = || match style {
        Quoting::Shell => quote_bytes_raw(arg, Quoting::ShellAlways, extra),
        Quoting::CMaybe => quote_bytes_raw(arg, Quoting::C, extra),
        _ => quote_bytes_raw(arg, Quoting::ShellEscapeAlways, extra),
    };

    let mut out: Vec<u8> = Vec::with_capacity(arg.len() + 2);
    if !elide {
        out.extend_from_slice(open);
    }
    let mut pending_shell_escape_end = false;
    let mut encountered_single_quote = false;
    let mut all_compat = true;
    let mut i = 0;
    while i < arg.len() {
        let mut c = arg[i];
        let mut escaping = false;
        let mut compat = false;
        let mut is_right_quote = false;
        if backslash && !shell && !close.is_empty() && arg[i..].starts_with(close) {
            if elide {
                return force();
            }
            is_right_quote = true;
        }
        // `None` means "store as is" (after the quote_these_too check),
        // `Some(e)` means "store backslash + e".
        let mut escape: Option<u8> = None;
        let mut direct = false;
        match c {
            0x07 | 0x08 | 0x0c | 0x0b | b'\n' | b'\r' | b'\t' => {
                let e = match c {
                    0x07 => b'a',
                    0x08 => b'b',
                    0x0c => b'f',
                    0x0b => b'v',
                    b'\n' => b'n',
                    b'\r' => b'r',
                    _ => b't',
                };
                if shell && elide {
                    return force();
                }
                if backslash {
                    escape = Some(e);
                }
            }
            b'\\' => {
                if shell {
                    if elide {
                        return force();
                    }
                    direct = true;
                } else if backslash && elide {
                    // c-maybe: a lone backslash needs no quoting.
                    direct = true;
                } else if backslash {
                    escape = Some(b'\\');
                }
            }
            b'?' => {
                if shell && elide {
                    return force();
                }
            }
            b'{' | b'}' | b'#' | b'~' | b' ' | b'!' | b'"' | b'$' | b'&' | b'(' | b')' | b'*'
            | b';' | b'<' | b'=' | b'>' | b'[' | b'^' | b'`' | b'|' => {
                let special = match c {
                    b'{' | b'}' => arg.len() == 1,
                    b'#' | b'~' => i == 0,
                    _ => true,
                };
                if special {
                    if matches!(c, b'{' | b'}' | b'#' | b'~' | b' ') {
                        compat = true;
                    }
                    if shell && elide {
                        return force();
                    }
                }
            }
            b'\'' => {
                encountered_single_quote = true;
                compat = true;
                if shell {
                    if elide {
                        return force();
                    }
                    out.extend_from_slice(b"'\\'");
                    pending_shell_escape_end = false;
                }
            }
            b'%'
            | b'+'
            | b','
            | b'-'
            | b'.'
            | b'/'
            | b'0'..=b'9'
            | b':'
            | b'A'..=b'Z'
            | b']'
            | b'_'
            | b'a'..=b'z' => compat = true,
            _ => {
                let (len, printable) = char_at(&arg[i..]);
                compat = printable;
                if len > 1 || (backslash && !printable) {
                    for j in 0..len {
                        c = arg[i + j];
                        if backslash && !printable {
                            if elide {
                                return force();
                            }
                            escaping = true;
                            if shell && !pending_shell_escape_end {
                                out.extend_from_slice(b"'$'");
                                pending_shell_escape_end = true;
                            }
                            out.push(b'\\');
                            out.push(b'0' + (c >> 6));
                            out.push(b'0' + ((c >> 3) & 7));
                            c = b'0' + (c & 7);
                        } else if is_right_quote {
                            out.push(b'\\');
                            is_right_quote = false;
                        }
                        if j + 1 == len {
                            break;
                        }
                        if pending_shell_escape_end && !escaping {
                            out.extend_from_slice(b"''");
                            pending_shell_escape_end = false;
                        }
                        out.push(c);
                    }
                    i += len - 1;
                    direct = true;
                }
            }
        }
        if escape.is_none()
            && !direct
            && !(((backslash && !shell) || elide) && extra.contains(&c))
            && !is_right_quote
        {
            direct = true;
        }
        if !direct {
            // store_escape
            if elide {
                return force();
            }
            escaping = true;
            if shell && !pending_shell_escape_end {
                out.extend_from_slice(b"'$'");
                pending_shell_escape_end = true;
            }
            out.push(b'\\');
            if let Some(e) = escape {
                c = e;
            }
        }
        // store_c
        if pending_shell_escape_end && !escaping {
            out.extend_from_slice(b"''");
            pending_shell_escape_end = false;
        }
        out.push(c);
        all_compat &= compat;
        i += 1;
    }
    if arg.is_empty() && shell && elide {
        return force();
    }
    if shell && !elide && encountered_single_quote && all_compat {
        return quote_bytes_raw(arg, Quoting::C, extra);
    }
    if !elide {
        out.extend_from_slice(close);
    }
    out
}

/// coreutils `quoteaf`: shell-escape-always quoting used in diagnostics.
fn quote_af(name: &str) -> String {
    quote_bytes(name.as_bytes(), Quoting::ShellEscapeAlways)
}

/// coreutils `quotef`: shell-escape quoting, only when needed.
fn quote_f(name: &str) -> String {
    quote_bytes(name.as_bytes(), Quoting::ShellEscape)
}

// ----- fnmatch -------------------------------------------------------------

/// POSIX `fnmatch` (no flags beyond `period`: a leading `.` must match literally).
fn fnmatch(pattern: &[u8], name: &[u8], period: bool) -> bool {
    if period && name.first() == Some(&b'.') && pattern.first() != Some(&b'.') {
        return false;
    }
    fnmatch_inner(pattern, name)
}

fn fnmatch_inner(p: &[u8], n: &[u8]) -> bool {
    let (mut pi, mut ni) = (0, 0);
    let mut backtrack: Option<(usize, usize)> = None;
    while ni < n.len() || pi < p.len() {
        if pi < p.len() {
            match p[pi] {
                b'*' => {
                    backtrack = Some((pi, ni + 1));
                    pi += 1;
                    continue;
                }
                b'?' if ni < n.len() => {
                    pi += 1;
                    ni += 1;
                    continue;
                }
                b'[' if ni < n.len() => {
                    if let Some((matched, next)) = match_bracket(&p[pi..], n[ni]) {
                        if matched {
                            pi += next;
                            ni += 1;
                            continue;
                        }
                    } else if n[ni] == b'[' {
                        pi += 1;
                        ni += 1;
                        continue;
                    }
                }
                b'\\' if pi + 1 < p.len() && ni < n.len() && p[pi + 1] == n[ni] => {
                    pi += 2;
                    ni += 1;
                    continue;
                }
                c if c != b'\\' && ni < n.len() && c == n[ni] => {
                    pi += 1;
                    ni += 1;
                    continue;
                }
                _ => {}
            }
        }
        match backtrack {
            Some((bp, bn)) if bn <= n.len() => {
                pi = bp + 1;
                ni = bn;
                backtrack = Some((bp, bn + 1));
            }
            _ => return false,
        }
    }
    true
}

/// Matches `c` against the bracket expression at the start of `p`; returns
/// `(matched, length of the expression)` or `None` if it is unterminated.
fn match_bracket(p: &[u8], c: u8) -> Option<(bool, usize)> {
    let mut i = 1;
    let negate = matches!(p.get(i), Some(b'!') | Some(b'^'));
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < p.len() {
        if p[i] == b']' && !first {
            return Some((matched != negate, i + 1));
        }
        first = false;
        let mut lo = p[i];
        if lo == b'\\' && i + 1 < p.len() {
            i += 1;
            lo = p[i];
        }
        if p.get(i + 1) == Some(&b'-') && p.get(i + 2).is_some_and(|h| *h != b']') {
            let hi = p[i + 2];
            matched |= (lo..=hi).contains(&c);
            i += 3;
        } else {
            matched |= lo == c;
            i += 1;
        }
    }
    None
}

// ----- version sort (gnulib filevercmp) ------------------------------------

/// gnulib `filevercmp`, used by `ls -v` / `--sort=version`.
fn filevercmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }
    // "." sorts first, then "..", then other dot files, then everything else.
    match (a[0] == b'.', b[0] == b'.') {
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        (true, true) => {
            for special in [&b"."[..], &b".."[..]] {
                match (a == special, b == special) {
                    (true, true) => return Ordering::Equal,
                    (true, false) => return Ordering::Less,
                    (false, true) => return Ordering::Greater,
                    _ => {}
                }
            }
        }
        _ => {}
    }
    let (ap, bp) = (version_prefix_len(a), version_prefix_len(b));
    let result = version_revcmp(&a[..ap], &b[..bp]);
    if result != Ordering::Equal || (ap == a.len() && bp == b.len()) {
        result
    } else {
        version_revcmp(a, b)
    }
}

/// Length of `s` without its file suffix, `(\.[A-Za-z~][A-Za-z0-9~]*)*$`.
fn version_prefix_len(s: &[u8]) -> usize {
    let n = s.len();
    let mut prefix = 0;
    let mut i = 0;
    while i < n {
        i += 1;
        prefix = i;
        while i + 1 < n && s[i] == b'.' && (s[i + 1].is_ascii_alphabetic() || s[i + 1] == b'~') {
            i += 2;
            while i < n && (s[i].is_ascii_alphanumeric() || s[i] == b'~') {
                i += 1;
            }
        }
    }
    prefix
}

fn version_order(s: &[u8], pos: usize) -> i32 {
    match s.get(pos) {
        None => -1,
        Some(c) if c.is_ascii_digit() => 0,
        Some(c) if c.is_ascii_alphabetic() => i32::from(*c),
        Some(b'~') => -2,
        Some(c) => i32::from(*c) + 256,
    }
}

fn version_revcmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        while (i < a.len() && !a[i].is_ascii_digit()) || (j < b.len() && !b[j].is_ascii_digit()) {
            let (x, y) = (version_order(a, i), version_order(b, j));
            if x != y {
                return x.cmp(&y);
            }
            i += 1;
            j += 1;
        }
        while i < a.len() && a[i] == b'0' {
            i += 1;
        }
        while j < b.len() && b[j] == b'0' {
            j += 1;
        }
        let mut first_diff = std::cmp::Ordering::Equal;
        while i < a.len() && j < b.len() && a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            if first_diff == std::cmp::Ordering::Equal {
                first_diff = a[i].cmp(&b[j]);
            }
            i += 1;
            j += 1;
        }
        if i < a.len() && a[i].is_ascii_digit() {
            return std::cmp::Ordering::Greater;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            return std::cmp::Ordering::Less;
        }
        if first_diff != std::cmp::Ordering::Equal {
            return first_diff;
        }
    }
    std::cmp::Ordering::Equal
}

// ----- misc file helpers ---------------------------------------------------

/// `major(dev)` as glibc / Darwin define it.
fn dev_major(dev: u64) -> u64 {
    if cfg!(target_os = "macos") {
        (dev >> 24) & 0xff
    } else {
        ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff)
    }
}

/// `minor(dev)` as glibc / Darwin define it.
fn dev_minor(dev: u64) -> u64 {
    if cfg!(target_os = "macos") {
        dev & 0xff_ffff
    } else {
        (dev & 0xff) | ((dev >> 12) & !0xff)
    }
}

/// gnulib `file_type`: the `%F` description of a file.
fn file_type_description(meta: &fs::Metadata) -> &'static str {
    use std::os::unix::fs::FileTypeExt;
    let ft = meta.file_type();
    if ft.is_file() {
        if meta.len() == 0 {
            "regular empty file"
        } else {
            "regular file"
        }
    } else if ft.is_dir() {
        "directory"
    } else if ft.is_symlink() {
        "symbolic link"
    } else if ft.is_fifo() {
        "fifo"
    } else if ft.is_socket() {
        "socket"
    } else if ft.is_char_device() {
        "character special file"
    } else if ft.is_block_device() {
        "block special file"
    } else {
        "weird file"
    }
}

fn user_name(uid: u32) -> Option<String> {
    get_username_by_uid(uid)
}

fn group_name(gid: u32) -> Option<String> {
    get_groupname_by_gid(gid)
}

fn meta_mtime(meta: &fs::Metadata) -> Timespec {
    Timespec {
        sec: meta.mtime(),
        nsec: meta.mtime_nsec(),
    }
}

fn meta_atime(meta: &fs::Metadata) -> Timespec {
    Timespec {
        sec: meta.atime(),
        nsec: meta.atime_nsec(),
    }
}

fn meta_ctime(meta: &fs::Metadata) -> Timespec {
    Timespec {
        sec: meta.ctime(),
        nsec: meta.ctime_nsec(),
    }
}

/// Birth time, when the platform and file system record it.
fn meta_birth(meta: &fs::Metadata) -> Option<Timespec> {
    meta.created().ok().map(Timespec::from_system)
}

/// Reads an extended attribute of `path` without following symlinks (Linux).
#[cfg(target_os = "linux")]
fn lgetxattr_bytes(path: &Path, name: &str) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let c_name = CString::new(name).ok()?;
    let mut buf = vec![0u8; 256];
    // SAFETY: valid C strings and a buffer of the advertised length.
    let len = unsafe {
        libc::lgetxattr(
            c_path.as_ptr(),
            c_name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if len <= 0 {
        return None;
    }
    buf.truncate(len as usize);
    Some(buf)
}

#[cfg(not(target_os = "linux"))]
fn lgetxattr_bytes(_path: &Path, _name: &str) -> Option<Vec<u8>> {
    None
}

/// The SELinux security context of `path`, if it has one.
fn security_context(path: &Path) -> Option<String> {
    let mut ctx = lgetxattr_bytes(path, "security.selinux")?;
    while ctx.last() == Some(&0) {
        ctx.pop();
    }
    Some(String::from_utf8_lossy(&ctx).into_owned())
}

/// libselinux `is_selinux_enabled`: an SELinux file system is mounted.
fn selinux_enabled() -> bool {
    cfg!(target_os = "linux") && Path::new("/sys/fs/selinux/enforce").exists()
}

fn path_bytes(p: &Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes()
}

fn bytes_path(b: &[u8]) -> &Path {
    use std::os::unix::ffi::OsStrExt;
    Path::new(std::ffi::OsStr::from_bytes(b))
}

// ---------------------------------------------------------------------------
// 13. realpath
// ---------------------------------------------------------------------------
const REALPATH_HELP: &str = "\
Usage: realpath [OPTION]... FILE...
Print the resolved absolute file name;
all but the last component must exist

  -e, --canonicalize-existing  all components of the path must exist
  -m, --canonicalize-missing   no path components need exist or be a directory
  -L, --logical                resolve '..' components before symlinks
  -P, --physical               resolve symlinks as encountered (default)
  -q, --quiet                  suppress most error messages
      --relative-to=DIR        print the resolved path relative to DIR
      --relative-base=DIR      print absolute paths unless paths below DIR
  -s, --strip, --no-symlinks   don't expand symlinks
  -z, --zero                   end each output line with NUL, not newline
  -J, --json                   emit {input, realpath} objects (Dogfish)
      --help        display this help and exit
      --version     output version information and exit
";

/// How much of a path must exist (gnulib `canonicalize_filename_mode`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum CanMode {
    Existing,
    AllButLast,
    Missing,
}

/// Makes `path` absolute, resolving `.`, `..` and (unless `nolinks`) symbolic
/// links, requiring components to exist as `mode` says. A port of gnulib's
/// `canonicalize_filename_mode`, quirks included: without symlink resolution,
/// only the last component, or one followed by `/..`, a final `/.` or a
/// trailing slash, is checked.
fn canonicalize_mode(path: &Path, mode: CanMode, nolinks: bool) -> io::Result<PathBuf> {
    const MAX_LINKS: usize = 40;
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() {
        return Err(io::Error::from_raw_os_error(libc::ENOENT));
    }
    let mut resolved = if bytes[0] == b'/' {
        PathBuf::from("/")
    } else {
        std::env::current_dir()?
    };
    let split = |b: &[u8]| -> VecDeque<OsString> {
        b.split(|&c| c == b'/')
            .filter(|c| !c.is_empty())
            .map(|c| OsString::from_vec(c.to_vec()))
            .collect()
    };
    let trailing_slash = bytes.len() > 1 && bytes.ends_with(b"/");
    // gnulib `suffix_requires_dir_check`: must the path so far be a directory?
    let needs_dir = |rest: &VecDeque<OsString>| {
        for (i, c) in rest.iter().enumerate() {
            match c.as_bytes() {
                b".." => return true,
                b"." if i + 1 == rest.len() => return true,
                b"." => {}
                _ => return false,
            }
        }
        trailing_slash
    };
    let errno = |e: &io::Error| e.raw_os_error().unwrap_or(0);
    let mut pending = split(bytes);
    let mut seen: HashSet<(PathBuf, Vec<u8>)> = HashSet::new();
    let mut links = 0;
    while let Some(comp) = pending.pop_front() {
        if comp == "." {
            continue;
        }
        if comp == ".." {
            resolved.pop();
            continue;
        }
        let candidate = resolved.join(&comp);
        if mode == CanMode::Missing && nolinks {
            resolved = candidate;
            continue;
        }
        let mut link_err = None;
        if !nolinks {
            match fs::read_link(&candidate) {
                Ok(target) => {
                    links += 1;
                    // The same link reached with the same remaining path is a loop.
                    let mut rest = comp.as_bytes().to_vec();
                    for c in &pending {
                        rest.push(b'/');
                        rest.extend_from_slice(c.as_bytes());
                    }
                    if links > MAX_LINKS || !seen.insert((resolved.clone(), rest)) {
                        if mode == CanMode::Missing {
                            resolved = candidate;
                            continue;
                        }
                        return Err(io::Error::from_raw_os_error(libc::ELOOP));
                    }
                    let tb = target.as_os_str().as_bytes();
                    if tb.first() == Some(&b'/') {
                        resolved = PathBuf::from("/");
                    }
                    for c in split(tb).into_iter().rev() {
                        pending.push_front(c);
                    }
                    continue;
                }
                Err(e) => link_err = Some(e),
            }
        }
        if mode != CanMode::Missing {
            let check = if needs_dir(&pending) {
                match fs::metadata(&candidate) {
                    Ok(m) if m.is_dir() => Ok(()),
                    Ok(_) => Err(io::Error::from_raw_os_error(libc::ENOTDIR)),
                    Err(e) => Err(e),
                }
            } else if let Some(e) = link_err {
                // readlink(2) fails with EINVAL on anything that is not a symlink.
                if errno(&e) == libc::EINVAL {
                    Ok(())
                } else {
                    Err(e)
                }
            } else if !pending.is_empty() {
                Ok(())
            } else {
                fs::metadata(&candidate).map(|_| ())
            };
            if let Err(e) = check
                && !(mode == CanMode::AllButLast && errno(&e) == libc::ENOENT && pending.is_empty())
            {
                return Err(e);
            }
        }
        resolved = candidate;
    }
    Ok(resolved)
}

/// `path` relative to the directory `base` (both absolute and canonical).
fn relpath(path: &Path, base: &Path) -> PathBuf {
    let p: Vec<_> = path.components().collect();
    let b: Vec<_> = base.components().collect();
    let common = p.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut out = PathBuf::new();
    for _ in common..b.len() {
        out.push("..");
    }
    for c in &p[common..] {
        out.push(c);
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// True if `prefix` is `path` or one of its ancestors (component-wise).
fn path_prefix(prefix: &Path, path: &Path) -> bool {
    path.starts_with(prefix)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RealpathOpt {
    Existing,
    Missing,
    Logical,
    Physical,
    Quiet,
    RelativeTo,
    RelativeBase,
    Strip,
    Zero,
    Json,
}

pub fn run_realpath(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    use RealpathOpt::*;
    const SPECS: &[ToolOpt<RealpathOpt>] = &[
        opt("e", "canonicalize-existing", HasArg::No, Existing),
        opt("m", "canonicalize-missing", HasArg::No, Missing),
        opt("L", "logical", HasArg::No, Logical),
        opt("P", "physical", HasArg::No, Physical),
        opt("q", "quiet", HasArg::No, Quiet),
        opt("", "relative-to", HasArg::Required, RelativeTo),
        opt("", "relative-base", HasArg::Required, RelativeBase),
        opt("s", "strip", HasArg::No, Strip),
        opt("", "no-symlinks", HasArg::No, Strip),
        opt("z", "zero", HasArg::No, Zero),
        opt("J", "json", HasArg::No, Json),
    ];
    let parsed = match parse_tool_args("realpath", &args, SPECS, REALPATH_HELP) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let (mut mode, mut nolinks, mut logical, mut quiet, mut zero, mut json) =
        (CanMode::AllButLast, false, false, false, false, false);
    let (mut relative_to, mut relative_base) = (None, None);
    for (o, v) in parsed.opts {
        match o {
            Existing => mode = CanMode::Existing,
            Missing => mode = CanMode::Missing,
            Logical => {
                logical = true;
                nolinks = true;
            }
            Physical => {
                logical = false;
                nolinks = false;
            }
            Strip => {
                nolinks = true;
                logical = false;
            }
            Quiet => quiet = true,
            RelativeTo => relative_to = v,
            RelativeBase => relative_base = v,
            Zero => zero = true,
            Json => json = true,
        }
    }
    if parsed.operands.is_empty() {
        return Ok(usage_error("realpath", "missing operand"));
    }
    let canon = |p: &Path| -> io::Result<PathBuf> {
        let first = canonicalize_mode(p, mode, nolinks)?;
        if logical {
            // -L: '..' was resolved lexically; now resolve the symlinks.
            canonicalize_mode(&first, mode, false)
        } else {
            Ok(first)
        }
    };
    let canon_dir = |d: &str| -> std::result::Result<PathBuf, i32> {
        match canon(Path::new(d)) {
            Ok(p) if mode == CanMode::Existing && !p.is_dir() => {
                eprintln!("realpath: {}: Not a directory", qf(d));
                Err(1)
            }
            Ok(p) => Ok(p),
            Err(e) => {
                eprintln!("realpath: {}: {}", qf(d), gnu_io_error(&e));
                Err(1)
            }
        }
    };
    if relative_base.is_some() && relative_to.is_none() {
        relative_to = relative_base.clone();
    }
    let mut rel_to = match &relative_to {
        Some(d) => match canon_dir(d) {
            Ok(p) => Some(p),
            Err(code) => return Ok(code),
        },
        None => None,
    };
    let rel_base = match &relative_base {
        Some(b) if Some(b) == relative_to.as_ref() => rel_to.clone(),
        Some(b) => match canon_dir(b) {
            // --relative-to is a no-op unless --relative-base is a prefix of it.
            Ok(base) if rel_to.as_ref().is_some_and(|to| path_prefix(&base, to)) => Some(base),
            Ok(_) => rel_to.take(),
            Err(code) => return Ok(code),
        },
        None => None,
    };

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    let mut exit_code = 0;
    let mut results = Vec::new();
    for arg in &parsed.operands {
        let resolved = match canon(Path::new(arg)) {
            Ok(p) => p,
            Err(e) => {
                if !quiet {
                    out.flush()?;
                    eprintln!("realpath: {}: {}", qf(arg), gnu_io_error(&e));
                }
                exit_code = 1;
                continue;
            }
        };
        let shown = match (&rel_to, &rel_base) {
            (Some(to), base) if base.as_ref().is_none_or(|b| path_prefix(b, &resolved)) => {
                relpath(&resolved, to)
            }
            _ => resolved,
        };
        ctx.telemetry.add_items(1);
        if json {
            results.push(serde_json::json!({ "input": arg, "realpath": shown.to_string_lossy() }));
        } else {
            out.write_all(shown.as_os_str().as_bytes())?;
            out.write_all(if zero { b"\0" } else { b"\n" })?;
        }
    }
    ignore_broken_pipe(out.flush())?;
    drop(out);
    if json {
        emit_objects_or_table(&results, ctx, true, false)?;
    }
    Ok(exit_code)
}

// ---------------------------------------------------------------------------
// 14. basename
// ---------------------------------------------------------------------------
const BASENAME_HELP: &str = "\
Usage: basename NAME [SUFFIX]
  or:  basename OPTION... NAME...
Print NAME with any leading directory components removed.
If specified, also remove a trailing SUFFIX.

  -a, --multiple       support multiple arguments and treat each as a NAME
  -s, --suffix=SUFFIX  remove a trailing SUFFIX; implies -a
  -z, --zero           end each output line with NUL, not newline
      --help        display this help and exit
      --version     output version information and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum BasenameOpt {
    Multiple,
    Suffix,
    Zero,
}

pub fn run_basename(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    use BasenameOpt::*;
    const SPECS: &[ToolOpt<BasenameOpt>] = &[
        opt("a", "multiple", HasArg::No, Multiple),
        opt("s", "suffix", HasArg::Required, Suffix),
        opt("z", "zero", HasArg::No, Zero),
    ];
    let parsed = match parse_tool_args_posix("basename", &args, SPECS, BASENAME_HELP) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let (mut multiple, mut suffix, mut zero) = (false, None, false);
    for (o, v) in parsed.opts {
        match o {
            Multiple => multiple = true,
            Suffix => {
                multiple = true;
                suffix = v;
            }
            Zero => zero = true,
        }
    }
    let names = parsed.operands;
    if names.is_empty() {
        return Ok(usage_error("basename", "missing operand"));
    }
    let (names, suffix) = if multiple {
        (&names[..], suffix)
    } else {
        if names.len() > 2 {
            return Ok(usage_error(
                "basename",
                &format!("extra operand {}", quote_locale(&names[2])),
            ));
        }
        (&names[..1], names.get(1).cloned())
    };
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    for name in names {
        out.write_all(&compute_basename(
            name.as_bytes(),
            suffix.as_deref().map(str::as_bytes),
        ))?;
        out.write_all(if zero { b"\0" } else { b"\n" })?;
        ctx.telemetry.add_items(1);
    }
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}

/// GNU `basename`: the last component without trailing slashes, minus `suffix`
/// unless that would leave nothing.
fn compute_basename(path: &[u8], suffix: Option<&[u8]>) -> Vec<u8> {
    let base = last_component(path);
    let mut name = if base.is_empty() {
        path[..base_len(path)].to_vec()
    } else {
        let mut len = base_len(base);
        if base.get(len) == Some(&b'/') {
            len += 1;
        }
        base[..len].to_vec()
    };
    let keep = base_len(&name);
    name.truncate(keep);
    if let Some(suf) = suffix
        && name.first() != Some(&b'/')
        && name.len() > suf.len()
        && name.ends_with(suf)
    {
        name.truncate(name.len() - suf.len());
    }
    name
}

// ---------------------------------------------------------------------------
// 15. dirname
// ---------------------------------------------------------------------------
const DIRNAME_HELP: &str = "\
Usage: dirname [OPTION] NAME...
Output each NAME with its last non-slash component and trailing slashes
removed; if NAME contains no /'s, output '.' (meaning the current directory).

  -z, --zero     end each output line with NUL, not newline
      --help     display this help and exit
      --version  output version information and exit
";

pub fn run_dirname(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    const SPECS: &[ToolOpt<()>] = &[opt("z", "zero", HasArg::No, ())];
    let parsed = match parse_tool_args("dirname", &args, SPECS, DIRNAME_HELP) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let zero = !parsed.opts.is_empty();
    if parsed.operands.is_empty() {
        return Ok(usage_error("dirname", "missing operand"));
    }
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    for name in &parsed.operands {
        out.write_all(compute_dirname(name.as_bytes()))?;
        out.write_all(if zero { b"\0" } else { b"\n" })?;
        ctx.telemetry.add_items(1);
    }
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}

/// GNU `dirname` (gnulib `dir_len`).
fn compute_dirname(path: &[u8]) -> &[u8] {
    let prefix = usize::from(path.first() == Some(&b'/'));
    let mut len = path.len() - last_component(path).len();
    while len > prefix && path[len - 1] == b'/' {
        len -= 1;
    }
    if len == 0 { b"." } else { &path[..len] }
}

// ---------------------------------------------------------------------------
// 16. mktemp
// ---------------------------------------------------------------------------
const MKTEMP_HELP: &str = "\
Usage: mktemp [OPTION]... [TEMPLATE]
Create a temporary file or directory, safely, and print its name.
TEMPLATE must contain at least 3 consecutive 'X's in last component.
If TEMPLATE is not specified, use tmp.XXXXXXXXXX, and --tmpdir is implied.
Files are created u+rw, and directories u+rwx, minus umask restrictions.

  -d, --directory     create a directory, not a file
  -u, --dry-run       do not create anything; merely print a name (unsafe)
  -q, --quiet         suppress diagnostics about file/dir-creation failure
      --suffix=SUFF   append SUFF to TEMPLATE; SUFF must not contain a slash.
                        This option is implied if TEMPLATE does not end in X
  -p DIR, --tmpdir[=DIR]  interpret TEMPLATE relative to DIR; if DIR is not
                        specified, use $TMPDIR if set, else /tmp.  With
                        this option, TEMPLATE must not be an absolute name;
                        unlike with -t, TEMPLATE may contain slashes, but
                        mktemp creates only the final component
  -t                  interpret TEMPLATE as a single file name component,
                        relative to a directory: $TMPDIR, if set; else the
                        directory specified via -p; else /tmp [deprecated]
      --help        display this help and exit
      --version     output version information and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum MktempOpt {
    Directory,
    DryRun,
    Quiet,
    TmpDir,
    Deprecated,
    Suffix,
}

pub fn run_mktemp(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    use MktempOpt::*;
    use std::io::Read;
    const SPECS: &[ToolOpt<MktempOpt>] = &[
        opt("d", "directory", HasArg::No, Directory),
        opt("u", "dry-run", HasArg::No, DryRun),
        opt("q", "quiet", HasArg::No, Quiet),
        opt("p", "", HasArg::Required, TmpDir),
        opt("", "tmpdir", HasArg::Optional, TmpDir),
        opt("t", "", HasArg::No, Deprecated),
        opt("", "suffix", HasArg::Required, Suffix),
    ];
    let parsed = match parse_tool_args("mktemp", &args, SPECS, MKTEMP_HELP) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let (mut directory, mut dry_run, mut quiet, mut use_dest_dir, mut deprecated_t) =
        (false, false, false, false, false);
    let (mut dest_dir_arg, mut suffix_opt) = (None, None);
    for (o, v) in parsed.opts {
        match o {
            Directory => directory = true,
            DryRun => dry_run = true,
            Quiet => quiet = true,
            TmpDir => {
                use_dest_dir = true;
                dest_dir_arg = v;
            }
            Deprecated => {
                use_dest_dir = true;
                deprecated_t = true;
            }
            Suffix => suffix_opt = v,
        }
    }
    let fail = |msg: String| {
        if !quiet {
            eprintln!("mktemp: {msg}");
        }
        Ok(1)
    };
    let template = match parsed.operands.len() {
        0 => {
            use_dest_dir = true;
            "tmp.XXXXXXXXXX".to_string()
        }
        1 => parsed.operands[0].clone(),
        _ => return Ok(usage_error("mktemp", "too many templates")),
    };

    // The suffix is --suffix, or whatever follows the last X of the template.
    let (template, suffix) = match &suffix_opt {
        Some(s) => {
            if !template.ends_with('X') {
                let mut shown = template.clone();
                shown.push_str(s);
                return fail(format!(
                    "with --suffix, template {} must end in X",
                    quote_locale(&shown)
                ));
            }
            (template.clone() + s, s.clone())
        }
        None => {
            let cut = template.rfind('X').map_or(template.len(), |i| i + 1);
            (template.clone(), template[cut..].to_string())
        }
    };
    if suffix.contains('/') {
        return fail(format!(
            "invalid suffix {}, contains directory separator",
            quote_locale(&suffix)
        ));
    }
    let stem = &template[..template.len() - suffix.len()];
    let x_count = stem.len() - stem.trim_end_matches('X').len();
    if x_count < 3 {
        return fail(format!(
            "too few X's in template {}",
            quote_locale(&template)
        ));
    }

    let full_template = if use_dest_dir {
        let env_tmpdir = std::env::var("TMPDIR").ok().filter(|t| !t.is_empty());
        let given = dest_dir_arg.filter(|d| !d.is_empty());
        let dir = if deprecated_t {
            if last_component(template.as_bytes()).len() != template.len() {
                return fail(format!(
                    "invalid template, {}, contains directory separator",
                    quote_locale(&template)
                ));
            }
            env_tmpdir.or(given)
        } else {
            if template.starts_with('/') {
                return fail(format!(
                    "invalid template, {}; with --tmpdir, it may not be absolute",
                    quote_locale(&template)
                ));
            }
            given.or(env_tmpdir)
        }
        .unwrap_or_else(|| "/tmp".to_string());
        concat_path(&dir, OsStr::new(&template))
    } else {
        PathBuf::from(&template)
    };
    let full = full_template.as_os_str().as_bytes().to_vec();
    let x_start = full.len() - suffix.len() - x_count;

    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut urandom = match fs::File::open("/dev/urandom") {
        Ok(f) => f,
        Err(e) => return fail(format!("cannot open /dev/urandom: {}", gnu_io_error(&e))),
    };
    let mut random = vec![0u8; x_count];
    let kind = if directory { "directory" } else { "file" };
    // Names are unpredictable and creation exclusive (O_EXCL, never following an
    // existing symlink), with restrictive permissions applied atomically.
    for _ in 0..10_000 {
        urandom.read_exact(&mut random)?;
        let mut name = full.clone();
        for (slot, b) in name[x_start..x_start + x_count].iter_mut().zip(&random) {
            *slot = ALPHABET[usize::from(*b) % ALPHABET.len()];
        }
        let path = PathBuf::from(OsString::from_vec(name));
        let res = if dry_run {
            match fs::symlink_metadata(&path) {
                Ok(_) => Err(io::Error::from_raw_os_error(libc::EEXIST)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
        } else if directory {
            fs::DirBuilder::new().mode(0o700).create(&path)
        } else {
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map(|_| ())
        };
        match res {
            Ok(()) => {
                ctx.telemetry.add_items(1);
                let mut out = io::stdout().lock();
                out.write_all(path.as_os_str().as_bytes())?;
                out.write_all(b"\n")?;
                return Ok(0);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return fail(format!(
                    "failed to create {kind} via template {}: {}",
                    quote_locale(&full_template),
                    gnu_io_error(&e)
                ));
            }
        }
    }
    fail(format!(
        "failed to create {kind} via template {}: File exists",
        quote_locale(&full_template)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // `rm`'s preserve-root guard is tested through its predicate alone so that a
    // regression can never turn the test into an actual `rm -r /`.
    #[test]
    fn is_root_dir_matches_every_spelling_of_root() {
        // `/usr` rather than `/tmp`: on macOS `/tmp` is a symlink, so `/tmp/..` is `/private`.
        for p in ["/", "//", "/.", "/./", "/usr/..", "/usr/../."] {
            assert!(is_root_dir(Path::new(p)), "{p}");
        }
    }

    #[test]
    fn is_root_dir_rejects_non_root_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("root_link");
        symlink("/", &link).unwrap();
        assert!(!is_root_dir(&link), "a symlink to / is just a link");
        assert!(!is_root_dir(tmp.path()));
        assert!(!is_root_dir(Path::new("/nonexistent-dogfish-path")));
    }
}
