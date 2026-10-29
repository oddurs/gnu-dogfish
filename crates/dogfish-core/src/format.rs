use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Lightweight zero-allocation ANSI style helper.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub enabled: bool,
}

impl Style {
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    fn wrap(&self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{}m{}\x1b[0m", code, text)
        } else {
            text.to_string()
        }
    }

    pub fn bold(&self, text: &str) -> String {
        self.wrap("1", text)
    }

    pub fn dim(&self, text: &str) -> String {
        self.wrap("2", text)
    }

    pub fn red(&self, text: &str) -> String {
        self.wrap("31", text)
    }

    pub fn bold_red(&self, text: &str) -> String {
        self.wrap("1;31", text)
    }

    pub fn green(&self, text: &str) -> String {
        self.wrap("32", text)
    }

    pub fn bold_green(&self, text: &str) -> String {
        self.wrap("1;32", text)
    }

    pub fn yellow(&self, text: &str) -> String {
        self.wrap("33", text)
    }

    pub fn bold_yellow(&self, text: &str) -> String {
        self.wrap("1;33", text)
    }

    pub fn blue(&self, text: &str) -> String {
        self.wrap("34", text)
    }

    pub fn bold_blue(&self, text: &str) -> String {
        self.wrap("1;34", text)
    }

    pub fn magenta(&self, text: &str) -> String {
        self.wrap("35", text)
    }

    pub fn cyan(&self, text: &str) -> String {
        self.wrap("36", text)
    }

    pub fn bold_cyan(&self, text: &str) -> String {
        self.wrap("1;36", text)
    }
}

/// Formats bytes into human-readable units (e.g., `14.2 MiB` or raw bytes).
pub fn format_size(bytes: u64, human: bool) -> String {
    if !human {
        return bytes.to_string();
    }
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if bytes < 1024 {
        return format!("{} B", bytes);
    }
    let mut val = bytes as f64;
    let mut unit_idx = 0;
    while val >= 1024.0 && unit_idx < UNITS.len() - 1 {
        val /= 1024.0;
        unit_idx += 1;
    }
    if val >= 100.0 {
        format!("{:.0} {}", val, UNITS[unit_idx])
    } else if val >= 10.0 {
        format!("{:.1} {}", val, UNITS[unit_idx])
    } else {
        format!("{:.2} {}", val, UNITS[unit_idx])
    }
}

/// Parses human size strings like `10K`, `5MiB`, `2G` into byte counts.
pub fn parse_size_str(input: &str) -> Result<u64, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty size string".into());
    }
    let split_idx = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num_part, unit_part) = s.split_at(split_idx);
    let num: f64 = num_part
        .parse()
        .map_err(|_| format!("invalid size number: {}", input))?;
    let mult: u64 = match unit_part.trim().to_ascii_uppercase().as_str() {
        "" | "B" | "C" => 1,
        "W" => 2,
        "K" | "KB" | "KIB" => 1024,
        "M" | "MB" | "MIB" => 1024 * 1024,
        "G" | "GB" | "GIB" => 1024 * 1024 * 1024,
        "T" | "TB" | "TIB" => 1024_u64.pow(4),
        "P" | "PB" | "PIB" => 1024_u64.pow(5),
        other => return Err(format!("unknown size suffix '{}'", other)),
    };
    Ok((num * (mult as f64)) as u64)
}

/// Formats `Duration` with microsecond/millisecond precision for `--stats` and benchmarks.
pub fn format_duration_human(d: Duration) -> String {
    let nanos = d.as_nanos();
    if nanos < 1_000 {
        format!("{}ns", nanos)
    } else if nanos < 1_000_000 {
        format!("{:.2}µs", nanos as f64 / 1_000.0)
    } else if nanos < 1_000_000_000 {
        format!("{:.2}ms", nanos as f64 / 1_000_000.0)
    } else {
        format!("{:.2}s", d.as_secs_f64())
    }
}

/// Parses modern human durations (`500ms`, `1.5s`, `2m`, `1h`, or multi-part `1m30s`).
pub fn parse_duration_str(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if let Ok(secs) = s.parse::<f64>() {
        if secs < 0.0 {
            return Err("negative duration not allowed".into());
        }
        return Ok(Duration::from_secs_f64(secs));
    }

    let mut total = Duration::ZERO;
    let mut current_num = String::new();
    let mut current_unit = String::new();

    for ch in s.chars() {
        if ch.is_whitespace() {
            continue;
        }
        if ch.is_ascii_digit() || ch == '.' {
            if !current_unit.is_empty() {
                total += apply_duration_unit(&current_num, &current_unit)?;
                current_num.clear();
                current_unit.clear();
            }
            current_num.push(ch);
        } else {
            current_unit.push(ch);
        }
    }
    if !current_num.is_empty() {
        let unit = if current_unit.is_empty() {
            "s"
        } else {
            &current_unit
        };
        total += apply_duration_unit(&current_num, unit)?;
    } else {
        return Err(format!("invalid duration '{}'", s));
    }
    Ok(total)
}

fn apply_duration_unit(num_str: &str, unit: &str) -> Result<Duration, String> {
    let val: f64 = num_str
        .parse()
        .map_err(|_| format!("invalid number '{}'", num_str))?;
    if val < 0.0 {
        return Err("duration cannot be negative".into());
    }
    match unit {
        "ns" => Ok(Duration::from_nanos(val as u64)),
        "us" | "µs" => Ok(Duration::from_micros(val as u64)),
        "ms" => Ok(Duration::from_secs_f64(val / 1000.0)),
        "s" | "sec" | "secs" => Ok(Duration::from_secs_f64(val)),
        "m" | "min" | "mins" => Ok(Duration::from_secs_f64(val * 60.0)),
        "h" | "hr" | "hrs" => Ok(Duration::from_secs_f64(val * 3600.0)),
        "d" | "day" | "days" => Ok(Duration::from_secs_f64(val * 86400.0)),
        _ => Err(format!("unknown duration unit '{}'", unit)),
    }
}

/// Renders a Unicode progress/usage bar (`[████████░░]`) with color thresholds.
pub fn sparkline_bar(ratio: f64, width: usize, style: Style) -> String {
    let clamped = ratio.clamp(0.0, 1.0);
    let filled = (clamped * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);

    let bar_str = format!("{}{}", "█".repeat(filled), "░".repeat(empty));
    let colored = if clamped >= 0.85 {
        style.red(&bar_str)
    } else if clamped >= 0.65 {
        style.yellow(&bar_str)
    } else {
        style.green(&bar_str)
    };
    format!("{}{}{}", style.dim("["), colored, style.dim("]"))
}

// File-type bits as `u32`: `libc::mode_t` is `u32` on Linux but `u16` on macOS, so the
// casts are needed on one platform and redundant on the other.
#[allow(clippy::unnecessary_cast)]
mod mode_bits {
    pub const S_IFMT: u32 = libc::S_IFMT as u32;
    #[cfg(test)]
    pub const S_IFREG: u32 = libc::S_IFREG as u32;
    pub const S_IFDIR: u32 = libc::S_IFDIR as u32;
    pub const S_IFLNK: u32 = libc::S_IFLNK as u32;
    pub const S_IFCHR: u32 = libc::S_IFCHR as u32;
    pub const S_IFBLK: u32 = libc::S_IFBLK as u32;
    pub const S_IFIFO: u32 = libc::S_IFIFO as u32;
    pub const S_IFSOCK: u32 = libc::S_IFSOCK as u32;
}

/// Formats POSIX file mode into symbolic `drwxr-xr-x` string.
pub fn format_mode_rwx(mode: u32) -> String {
    use mode_bits::*;
    let file_type = match mode & S_IFMT {
        S_IFDIR => 'd',
        S_IFLNK => 'l',
        S_IFCHR => 'c',
        S_IFBLK => 'b',
        S_IFIFO => 'p',
        S_IFSOCK => 's',
        _ => '-',
    };
    let mut s = String::with_capacity(10);
    s.push(file_type);

    let flags = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ];
    for (bit, ch) in flags {
        s.push(if mode & bit != 0 { ch } else { '-' });
    }
    // SUID / SGID / Sticky bit handling
    if mode & 0o4000 != 0 {
        s.replace_range(3..4, if mode & 0o100 != 0 { "s" } else { "S" });
    }
    if mode & 0o2000 != 0 {
        s.replace_range(6..7, if mode & 0o010 != 0 { "s" } else { "S" });
    }
    if mode & 0o1000 != 0 {
        s.replace_range(9..10, if mode & 0o001 != 0 { "t" } else { "T" });
    }
    s
}

/// Formats a `SystemTime` into a concise human relative string (`12m ago`, `3d ago`).
pub fn format_relative_time(time: SystemTime) -> String {
    let now = SystemTime::now();
    match now.duration_since(time) {
        Ok(diff) => {
            let secs = diff.as_secs();
            if secs < 60 {
                format!("{}s ago", secs)
            } else if secs < 3600 {
                format!("{}m ago", secs / 60)
            } else if secs < 86400 {
                format!("{}h ago", secs / 3600)
            } else if secs < 86400 * 30 {
                format!("{}d ago", secs / 86400)
            } else if secs < 86400 * 365 {
                format!("{}mo ago", secs / (86400 * 30))
            } else {
                format!("{}y ago", secs / (86400 * 365))
            }
        }
        Err(_) => "in future".to_string(),
    }
}

pub fn system_time_to_unix(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Formats an [`std::io::Error`] the way GNU coreutils does: just the `strerror`
/// text (`No such file or directory`), without Rust's ` (os error N)` suffix.
pub fn gnu_io_error(err: &std::io::Error) -> String {
    strip_os_error_suffix(&err.to_string())
}

/// Removes every ` (os error N)` fragment that Rust's `io::Error` `Display`
/// appends, so wrapped/contextualized messages also read like GNU's.
pub fn strip_os_error_suffix(msg: &str) -> String {
    const MARK: &str = " (os error ";
    let mut out = String::with_capacity(msg.len());
    let mut rest = msg;
    while let Some(idx) = rest.find(MARK) {
        let after = &rest[idx + MARK.len()..];
        let digits = after
            .bytes()
            .take_while(|b| b.is_ascii_digit() || *b == b'-')
            .count();
        if digits > 0 && after[digits..].starts_with(')') {
            out.push_str(&rest[..idx]);
            rest = &after[digits + 1..];
        } else {
            out.push_str(&rest[..idx + MARK.len()]);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// The C escape GNU uses for a byte that is not printable ASCII (`\n`, `\001`).
fn c_escape(c: u8) -> String {
    match c {
        0x07 => "\\a".into(),
        0x08 => "\\b".into(),
        0x0c => "\\f".into(),
        b'\n' => "\\n".into(),
        b'\r' => "\\r".into(),
        b'\t' => "\\t".into(),
        0x0b => "\\v".into(),
        _ => format!("\\{c:03o}"),
    }
}

fn is_printable(c: u8) -> bool {
    (0x20..0x7f).contains(&c)
}

/// Quotes an argument for a diagnostic like GNU's `quote` in the C locale:
/// always in single quotes, with C escapes (`'a\'b'`, `'x\ty'`).
pub fn gnu_quote(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('\'');
    for &c in arg.as_bytes() {
        match c {
            b'\\' => out.push_str("\\\\"),
            b'\'' => out.push_str("\\'"),
            c if is_printable(c) => out.push(c as char),
            c => out.push_str(&c_escape(c)),
        }
    }
    out.push('\'');
    out
}

/// Quotes a file name for a diagnostic like GNU's `quotef` (shell-escape
/// style, only when needed): `a b` becomes `'a b'`, a newline becomes
/// `'x'$'\n''y'`, and names that need no quoting are returned unchanged.
pub fn gnu_quote_file(name: &str) -> std::borrow::Cow<'_, str> {
    let b = name.as_bytes();
    let shell_special = |i: usize, c: u8| match c {
        b'#' | b'~' => i == 0,
        b' ' | b'!' | b'"' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b':' | b';' | b'<'
        | b'=' | b'>' | b'?' | b'[' | b'\\' | b'^' | b'`' | b'|' => true,
        _ => false,
    };
    let unprintable = b.iter().any(|&c| !is_printable(c));
    if !b.is_empty() && !unprintable && !b.iter().enumerate().any(|(i, &c)| shell_special(i, c)) {
        return std::borrow::Cow::Borrowed(name);
    }
    if !unprintable {
        if b.contains(&b'\'') && !b.iter().any(|c| b"$`\\\"!".contains(c)) {
            return format!("\"{name}\"").into();
        }
        return format!("'{}'", name.replace('\'', "'\\''")).into();
    }
    let mut out = String::from("'");
    let mut escaping = false;
    for &c in b {
        if is_printable(c) {
            if escaping {
                out.push_str("''");
                escaping = false;
            }
            if c == b'\'' {
                out.push_str("'\\''");
            } else {
                out.push(c as char);
            }
        } else {
            if !escaping {
                out.push_str("'$'");
                escaping = true;
            }
            out.push_str(&c_escape(c));
        }
    }
    out.push('\'');
    out.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gnu_quote_styles() {
        assert_eq!(gnu_quote("a'b\\c\t\u{e9}"), "'a\\'b\\\\c\\t\\303\\251'");
        assert_eq!(gnu_quote_file("plain-name.txt"), "plain-name.txt");
        assert_eq!(gnu_quote_file("a b"), "'a b'");
        assert_eq!(gnu_quote_file("a'b"), "\"a'b\"");
        assert_eq!(gnu_quote_file("a'$b"), "'a'\\''$b'");
        assert_eq!(gnu_quote_file(""), "''");
        assert_eq!(gnu_quote_file("x\ny"), "'x'$'\\n''y'");
        assert_eq!(gnu_quote_file("\ty"), "''$'\\t''y'");
        assert_eq!(gnu_quote_file("a\u{1}"), "'a'$'\\001'");
        assert_eq!(gnu_quote_file("a~#"), "a~#");
        assert_eq!(gnu_quote_file("~a"), "'~a'");
    }

    #[test]
    fn test_gnu_io_error_strips_os_suffix() {
        let e = std::io::Error::from_raw_os_error(2);
        assert_eq!(gnu_io_error(&e), "No such file or directory");
        assert_eq!(
            strip_os_error_suffix("x: Permission denied (os error 13); y (os error z)"),
            "x: Permission denied; y (os error z)"
        );
    }

    #[test]
    fn test_format_size_human_and_raw() {
        assert_eq!(format_size(0, true), "0 B");
        assert_eq!(format_size(512, true), "512 B");
        assert_eq!(format_size(1024, true), "1.00 KiB");
        assert_eq!(format_size(1536, true), "1.50 KiB");
        assert_eq!(format_size(15 * 1024, true), "15.0 KiB");
        assert_eq!(format_size(250 * 1024, true), "250 KiB");
        assert_eq!(format_size(1024 * 1024 * 5, true), "5.00 MiB");
        assert_eq!(format_size(1024 * 1024 * 1024 * 2, true), "2.00 GiB");
        assert_eq!(format_size(9876543, false), "9876543");
    }

    #[test]
    fn test_parse_size_str_all_units() {
        assert_eq!(parse_size_str("100").unwrap(), 100);
        assert_eq!(parse_size_str("100B").unwrap(), 100);
        assert_eq!(parse_size_str("2K").unwrap(), 2048);
        assert_eq!(parse_size_str("2KiB").unwrap(), 2048);
        assert_eq!(parse_size_str("1.5M").unwrap(), 1572864);
        assert_eq!(parse_size_str("1G").unwrap(), 1073741824);
        assert_eq!(parse_size_str("1T").unwrap(), 1099511627776);
        assert!(parse_size_str("").is_err());
        assert!(parse_size_str("12XYZ").is_err());
    }

    #[test]
    fn test_parse_duration_str_human_and_compound() {
        assert_eq!(
            parse_duration_str("500ns").unwrap(),
            Duration::from_nanos(500)
        );
        assert_eq!(
            parse_duration_str("250us").unwrap(),
            Duration::from_micros(250)
        );
        assert_eq!(
            parse_duration_str("100ms").unwrap(),
            Duration::from_millis(100)
        );
        assert_eq!(
            parse_duration_str("2.5s").unwrap(),
            Duration::from_millis(2500)
        );
        assert_eq!(parse_duration_str("3").unwrap(), Duration::from_secs(3));
        assert_eq!(
            parse_duration_str("1m 30s").unwrap(),
            Duration::from_secs(90)
        );
        assert_eq!(
            parse_duration_str("1h30m").unwrap(),
            Duration::from_secs(5400)
        );
        assert!(parse_duration_str("-5s").is_err());
        assert!(parse_duration_str("abc").is_err());
    }

    #[test]
    fn test_format_mode_rwx_permissions_and_special_bits() {
        assert_eq!(format_mode_rwx(mode_bits::S_IFREG | 0o755), "-rwxr-xr-x");
        assert_eq!(format_mode_rwx(mode_bits::S_IFDIR | 0o700), "drwx------");
        assert_eq!(format_mode_rwx(mode_bits::S_IFLNK | 0o777), "lrwxrwxrwx");
        // SUID + SGID + Sticky
        assert_eq!(format_mode_rwx(mode_bits::S_IFREG | 0o4755), "-rwsr-xr-x");
        assert_eq!(format_mode_rwx(mode_bits::S_IFREG | 0o2755), "-rwxr-sr-x");
        assert_eq!(format_mode_rwx(mode_bits::S_IFDIR | 0o1777), "drwxrwxrwt");
        assert_eq!(format_mode_rwx(mode_bits::S_IFDIR | 0o1776), "drwxrwxrwT");
    }

    #[test]
    fn test_sparkline_bar_rendering() {
        let st = Style::new(false);
        assert_eq!(sparkline_bar(0.0, 10, st), "[░░░░░░░░░░]");
        assert_eq!(sparkline_bar(0.5, 10, st), "[█████░░░░░]");
        assert_eq!(sparkline_bar(1.0, 10, st), "[██████████]");
        assert_eq!(sparkline_bar(2.5, 5, st), "[█████]"); // Clamped to 1.0
    }
}
