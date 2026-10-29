//! stat — Visual inspection card on a TTY, GNU `stat` output otherwise

#[cfg(target_os = "macos")]
use std::ffi::CStr;
use std::ffi::CString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;
use chrono::{DateTime, Local};
use dogfish_core::{
    DogfishContext, emit_objects_or_table, format_mode_rwx, format_relative_time, format_size,
    gnu_io_error, io::ignore_broken_pipe, outln, stdout,
};
use serde::Serialize;

use crate::common::file_info::{
    dev_major, dev_minor, file_type_description, find_mount_point, group_name, meta_atime,
    meta_birth, meta_ctime, meta_mtime, path_bytes, security_context, user_name,
};
use crate::common::getopt::{
    Getopt, HasArg, OptSpec, argmatch, getopt_error, lossy_all, print_help, print_version,
    usage_error,
};
use crate::common::quote::{Quoting, quote_af, quote_bytes, quote_bytes_raw};
use crate::common::strftime::{Timespec, format_local_time};
use crate::common::users::{get_groupname_by_gid, get_username_by_uid};

const STAT_OPTIONS: &[OptSpec<&str>] = &[
    OptSpec::both(b'L', "dereference", HasArg::No, "dereference"),
    OptSpec::both(b'f', "file-system", HasArg::No, "file-system"),
    OptSpec::both(b'c', "format", HasArg::Required, "format"),
    OptSpec::long("printf", HasArg::Required, "printf"),
    OptSpec::both(b't', "terse", HasArg::No, "terse"),
    OptSpec::long("cached", HasArg::Required, "cached"),
    OptSpec::long("help", HasArg::No, "help"),
    OptSpec::long("version", HasArg::No, "version"),
    // Dogfish extensions.
    OptSpec::both(b'J', "json", HasArg::No, "json").exact(),
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

pub fn run_stat(args: Vec<std::ffi::OsString>, ctx: &DogfishContext) -> Result<i32> {
    let mut parsed = Getopt::new(args.into_iter().skip(1).collect(), STAT_OPTIONS);
    let mut dereference = false;
    let mut file_system = false;
    let mut terse = false;
    let mut json = false;
    // (FORMAT, interpret backslash escapes and omit the trailing newline)
    let mut format: Option<(String, bool)> = None;
    while let Some(item) = parsed.next_text() {
        let opt = match item {
            Ok(o) => o,
            Err(e) => return Ok(getopt_error("stat", &e)),
        };
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
                if let Err(code) = argmatch("stat", "--cached", &value, &choices) {
                    return Ok(code);
                }
            }
            "help" => return Ok(print_help(STAT_HELP)),
            "version" => return Ok(print_version("stat")),
            _ => unreachable!("unhandled stat option"),
        }
    }
    let operands = lossy_all(parsed.operands());
    if operands.is_empty() {
        return Ok(usage_error("stat", "missing operand"));
    }
    if json || (ctx.stdout_is_tty && format.is_none() && !terse && !file_system) {
        return stat_card(&operands, dereference, json, ctx);
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
    let mut out = io::BufWriter::new(stdout::stdout());
    let mut status = 0;
    for name in &operands {
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
            if name == "-" {
                eprintln!(
                    "stat: using {} to denote standard input does not work in file system mode",
                    quote_af(name)
                );
                status = 1;
                continue;
            }
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
        // SAFETY: `c_path` is a NUL-terminated path and `s` a writable statfs;
        // both outlive the call.
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
        // SAFETY: `c_path` is a NUL-terminated path and `s` a writable statvfs;
        // both outlive the call.
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
    // SAFETY: `c_path` is a NUL-terminated path and `s` a writable statfs;
    // both outlive the call.
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
        outln!(
            "  {} {}  {}",
            st.dim("File:"),
            header_target,
            st.dim(&format!("({})", rec.file_type))
        );
        outln!(
            "  {} {} ({} B)   {} {}   {} {}",
            st.dim("Size:"),
            st.bold_green(&rec.size_human),
            rec.size,
            st.dim("Blocks:"),
            rec.blocks,
            st.dim("Inode:"),
            rec.inode
        );
        outln!(
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
        outln!(
            "{} {} ({})",
            st.dim("Modify:"),
            rec.modified,
            st.cyan(&rec.modified_relative)
        );
        outln!("{} {}", st.dim("Access:"), rec.accessed);
        outln!("{} {}", st.dim("Change:"), rec.changed);
    }
    Ok(exit_code)
}

/// libselinux `is_selinux_enabled`: an SELinux file system is mounted.
fn selinux_enabled() -> bool {
    cfg!(target_os = "linux") && Path::new("/sys/fs/selinux/enforce").exists()
}
