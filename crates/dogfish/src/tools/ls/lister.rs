//! Reading, sorting and printing the listing.

use std::ffi::CString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use dogfish_core::{ColumnAlign, Style, Table, format_mode_rwx, format_size, gnu_io_error};

use crate::common::canonicalize::{CanMode, canonicalize_mode};
use crate::common::file_info::{
    dev_major, dev_minor, group_name, has_xattrs, lgetxattr_bytes, meta_atime, meta_birth,
    meta_ctime, meta_mtime, path_bytes, security_context, user_name,
};
use crate::common::fnmatch::fnmatch;
use crate::common::human::{HUMAN_AUTOSCALE, human_readable};
use crate::common::quote::{Quoting, char_at, display_width, qa, qf, quote_bytes_raw};
use crate::common::strftime::{Timespec, format_local_time};
use crate::tools::ls::colors::{
    C_BLK, C_CAP, C_CHR, C_CLR_TO_EOL, C_DIR, C_END, C_EXEC, C_FIFO, C_FILE, C_LEFT, C_LINK,
    C_MISSING, C_MULTIHARDLINK, C_NORM, C_ORPHAN, C_OTHER_WRITABLE, C_RESET, C_RIGHT, C_SETGID,
    C_SETUID, C_SOCK, C_STICKY, C_STICKY_OTHER_WRITABLE, LsColors, colorize_mode, style_entry_name,
};
use crate::tools::ls::options::{Indicator, LsDeref, LsFormat, LsIgnore, LsOpts, LsSort, LsTime};
use crate::tools::ls::rich::{LsEntry, ls_json_entry};
use crate::tools::ls::version_sort::filevercmp;

/// One listed file.
pub(crate) struct LsFile {
    /// The name as printed: the operand, or the entry name inside a directory.
    pub(crate) name: Vec<u8>,
    pub(crate) path: PathBuf,
    pub(crate) meta: Option<fs::Metadata>,
    /// `ls -l` file-type letter from `readdir` when the file could not be stat'ed.
    type_letter: u8,
    link_target: Option<Vec<u8>>,
    target_meta: Option<fs::Metadata>,
    /// A command-line directory to list, or a subdirectory to recurse into.
    list_as_dir: bool,
    /// Whether GNU would have stat'ed this file (its `stat_ok`, given that the
    /// stat succeeded): otherwise GNU colours it by its `readdir` type alone.
    gnu_stat: bool,
    /// With `--hyperlink`: the canonical absolute name the link points to.
    absolute: Option<Vec<u8>>,
}

impl LsFile {
    fn is_dir(&self) -> bool {
        match &self.meta {
            Some(m) => m.is_dir(),
            None => self.type_letter == b'd',
        }
    }

    pub(crate) fn time(&self, which: LsTime) -> Option<Timespec> {
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

    /// The inode number for `-i`, when the file was stat'ed (GNU prints `?`
    /// otherwise: readdir's `d_ino` is wrong for mount points).
    fn ino(&self) -> Option<u64> {
        self.meta.as_ref().map(|m| m.ino())
    }
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
    pub(crate) inode: usize,
    pub(crate) blocks: usize,
    nlink: usize,
    pub(crate) owner: usize,
    pub(crate) group: usize,
    pub(crate) author: usize,
    pub(crate) context: usize,
    size: usize,
    major: usize,
    minor: usize,
    any_acl: bool,
    some_quoted: bool,
}

/// A writer that counts the bytes written through it.
pub(crate) struct Counted<W: Write> {
    inner: W,
    count: u64,
}

impl<W: Write> Counted<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self { inner, count: 0 }
    }
}

impl<W: Write> Write for Counted<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count += n as u64;
        Ok(n)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.inner.write_all(buf)?;
        self.count += buf.len() as u64;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// `--dired` bookkeeping: GNU's `dired_pos` counts every byte written except
/// colour sequences, and the offsets of names are collected for the
/// `//DIRED//` and `//SUBDIRED//` lines.
#[derive(Default)]
pub(crate) struct Dired {
    /// Bytes of colour sequences produced so far (written or buffered).
    color_bytes: std::cell::Cell<u64>,
    /// (start, end) of each file name in a `-l` line.
    names: std::cell::RefCell<Vec<u64>>,
    /// (start, end) of each directory name in a header.
    subdirs: Vec<u64>,
}

/// Walks operands and directories, printing them as GNU `ls` does.
pub(crate) struct Lister<'a, W: Write> {
    pub(crate) opts: &'a LsOpts,
    pub(crate) out: Counted<W>,
    pub(crate) dired: Dired,
    /// With `--hyperlink`: this host's name, percent-encoded.
    pub(crate) hostname: Vec<u8>,
    pub(crate) status: i32,
    pub(crate) first_header: bool,
    /// Dogfish styling on a terminal.
    pub(crate) rich: Option<Style>,
    pub(crate) json: Vec<LsEntry>,
    /// Directories being listed (for `-R` loop detection).
    pub(crate) active: Vec<(u64, u64)>,
    /// GNU `used_color`: whether a colour sequence has been written yet.
    pub(crate) used_color: std::cell::Cell<bool>,
    /// The last `-l` timestamp formatted: (seconds, recent, text).
    pub(crate) time_cache: std::cell::RefCell<Option<(i64, bool, Option<String>)>>,
}

impl<W: Write> Lister<'_, W> {
    pub(crate) fn run(&mut self, targets: &[Vec<u8>]) -> io::Result<()> {
        let o = self.opts;
        let mut files = Vec::new();
        for t in targets {
            if let Some(f) = self.gobble_operand(t) {
                files.push(f);
            }
        }
        let mut sorted: Vec<&LsFile> = files.iter().collect();
        self.sort(&mut sorted);
        let (dirs, plain): (Vec<&LsFile>, Vec<&LsFile>) =
            sorted.iter().copied().partition(|f| f.list_as_dir);
        // GNU names each directory unless a lone directory operand is listed.
        let print_names =
            o.recursive || !(plain.is_empty() && targets.len() <= 1 && dirs.len() == 1);
        if !plain.is_empty() {
            self.print_files(&plain, &sorted, false)?;
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
        // Like gnulib `error`, flush what was listed so far first, so that
        // the two streams interleave as GNU's do. A failed flush shows up
        // again on the next write.
        let _ = self.out.flush();
        eprintln!("ls: {msg}");
        self.status = self.status.max(if serious { 2 } else { 1 });
    }

    /// GNU's `absolute_name` for `--hyperlink` (`canonicalize_filename_mode`
    /// with `CAN_MISSING`), reporting a failure as GNU does.
    fn absolute_name(&mut self, path: &Path, command_line: bool) -> Option<Vec<u8>> {
        if !self.opts.hyperlink {
            return None;
        }
        match canonicalize_mode(path, CanMode::Missing, false) {
            Ok(p) => Some(p.into_os_string().into_vec()),
            Err(e) => {
                self.fail(
                    format!("error canonicalizing {}: {}", qa(path), gnu_io_error(&e)),
                    command_line,
                );
                None
            }
        }
    }

    fn gobble_operand(&mut self, name: &[u8]) -> Option<LsFile> {
        let o = self.opts;
        let path = bytes_path(name).to_path_buf();
        let absolute = self.absolute_name(&path, true);
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
                    format!("cannot access {}: {}", qa(&path), gnu_io_error(&e)),
                    true,
                );
                return None;
            }
        };
        let list_as_dir = meta.is_dir() && !o.immediate_dirs;
        let mut f = LsFile {
            name: name.to_vec(),
            path,
            meta: Some(meta),
            type_letter: b'?',
            link_target: None,
            target_meta: None,
            list_as_dir,
            gnu_stat: true,
            absolute,
        };
        self.inspect_link(&mut f);
        Some(f)
    }

    fn gobble_entry(&mut self, dir: &Path, name: Vec<u8>, letter: u8) -> LsFile {
        let o = self.opts;
        let d = path_bytes(dir);
        // `dir == "."` component-wise (which also takes `./`), without
        // parsing components for the usual paths that cannot match.
        let path = if d == b"." || (d.first() == Some(&b'.') && dir == Path::new(".")) {
            bytes_path(&name).to_path_buf()
        } else {
            // `dir.join(name)`, in one allocation.
            let mut p = Vec::with_capacity(d.len() + 1 + name.len());
            p.extend_from_slice(d);
            if !d.is_empty() && !d.ends_with(b"/") {
                p.push(b'/');
            }
            p.extend_from_slice(&name);
            PathBuf::from(std::ffi::OsString::from_vec(p))
        };
        // Like GNU (`gobble_file`), only stat when the output needs it, and
        // only follow a link (and report a dangling one) in that case.
        let colors = o.colors.as_ref();
        let colored =
            |which: &[usize]| colors.is_some_and(|c| which.iter().any(|i| c.is_colored(*i)));
        let needs_stat = o.format == LsFormat::Long
            || o.blocks
            || o.context
            || matches!(o.sort, LsSort::Time | LsSort::Size);
        let needs_type = !needs_stat
            && (o.recursive || colors.is_some() || o.indicator != Indicator::None || o.dirs_first);
        let gnu_stat = needs_stat
            // GNU does not trust d_ino (READDIR_LIES_ABOUT_MOUNTPOINT_D_INO).
            || o.inode
            || o.hyperlink
            || (letter == b'd' && colored(&[C_OTHER_WRITABLE, C_STICKY, C_STICKY_OTHER_WRITABLE]))
            || ((o.inode || needs_type)
                && matches!(letter, b'l' | b'?')
                && (o.deref == LsDeref::Always
                    || colors.is_some_and(|c| c.referent)
                    || o.check_symlink_mode))
            || (needs_type
                && (letter == b'?'
                    || (letter == b'-'
                        && (o.indicator == Indicator::Classify
                            || colored(&[C_EXEC, C_SETUID, C_SETGID, C_CAP])))));
        // Without a stat, GNU goes by readdir's type (for colours,
        // indicators, recursion and grouping): skip it then too.
        // Only the Dogfish JSON and terminal views read more.
        let unused = !gnu_stat && !o.json && self.rich.is_none();
        let follow = o.deref == LsDeref::Always && gnu_stat;
        let absolute = if gnu_stat {
            self.absolute_name(&path, false)
        } else {
            None
        };
        let meta = if unused {
            None
        } else {
            let stat = if follow {
                fs::metadata(&path)
            } else {
                fs::symlink_metadata(&path)
            };
            match stat {
                Ok(m) => Some(m),
                Err(e) => {
                    if gnu_stat {
                        self.fail(
                            format!("cannot access {}: {}", qa(&path), gnu_io_error(&e)),
                            false,
                        );
                    }
                    None
                }
            }
        };
        let is_dot = name == b"." || name == b"..";
        let is_dir = match &meta {
            Some(m) => m.is_dir(),
            None => unused && letter == b'd',
        };
        let list_as_dir = o.recursive && !is_dot && is_dir;
        let mut f = LsFile {
            name,
            path,
            meta,
            type_letter: letter,
            link_target: None,
            target_meta: None,
            list_as_dir,
            gnu_stat,
            absolute,
        };
        self.inspect_link(&mut f);
        f
    }

    /// Reads a symlink's target (for `-l`) and stats it (for `-F` indicators).
    fn inspect_link(&self, f: &mut LsFile) {
        let o = self.opts;
        let is_link = f.meta.as_ref().is_some_and(|m| m.file_type().is_symlink());
        // GNU also checks link targets for --group-directories-first and colours.
        let check_target = o.indicator >= Indicator::FileType || o.check_symlink_mode;
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
        let shown = bytes_path(name);
        if let Some(id) = id
            && self.active.contains(&id)
        {
            self.fail(
                format!("{}: not listing already-listed directory", qf(shown)),
                true,
            );
            return Ok(());
        }
        let entries = match read_dir_entries(path) {
            Ok(e) => e,
            Err(e) => {
                self.fail(
                    format!("cannot open directory {}: {}", qa(shown), gnu_io_error(&e)),
                    command_line,
                );
                return Ok(());
            }
        };
        if print_name {
            if !self.first_header {
                self.out.write_all(&[o.eol])?;
            }
            if o.dired {
                self.out.write_all(b"  ")?;
            }
            let (mut header, _) = self.quote_with(name, b":");
            if let Some(st) = self.rich {
                header = st.bold_blue(&String::from_utf8_lossy(&header)).into_bytes();
            }
            let absolute = self.absolute_name(path, command_line);
            let mut buf = Vec::with_capacity(header.len() + 64);
            let (start, end) = self.hyperlinked(&mut buf, &header, absolute.as_deref(), false);
            if o.dired {
                let (start, end) = (self.dired_pos(start), self.dired_pos(end));
                self.dired.subdirs.extend([start, end]);
            }
            buf.extend_from_slice(b":");
            buf.push(o.eol);
            self.out.write_all(&buf)?;
        }
        self.first_header = false;

        let mut files: Vec<LsFile> = Vec::with_capacity(entries.len());
        for (entry, letter) in entries {
            if !self.ignored(&entry) {
                let f = self.gobble_entry(path, entry, letter);
                files.push(f);
            }
        }
        let mut refs: Vec<&LsFile> = files.iter().collect();
        self.sort(&mut refs);
        self.print_files(&refs, &refs, true)?;

        if o.recursive {
            if let Some(id) = id {
                self.active.push(id);
            }
            for f in refs.iter().filter(|f| f.list_as_dir) {
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

    pub(crate) fn sort(&self, files: &mut [&LsFile]) {
        use std::cmp::Ordering;
        let o = self.opts;
        if o.sort == LsSort::None {
            return;
        }
        if o.sort == LsSort::Name && !o.dirs_first {
            // The common case, without the general comparator's dispatch.
            if o.reverse {
                files.sort_by(|a, b| b.name.cmp(&a.name));
            } else {
                files.sort_by(|a, b| a.name.cmp(&b.name));
            }
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
        files.sort_by(|&a, &b| {
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
        // GNU `filename_quoting_options`: the escape style also escapes
        // spaces, and with indicators the indicator characters (GNU's
        // `&"*=>@|"[indicator_style - file_type]`: `*` only for --file-type).
        let extra: &[u8] = match (o.quoting == Quoting::Escape, o.indicator) {
            (false, Indicator::None | Indicator::Slash) => b"",
            (true, Indicator::None | Indicator::Slash) => b" ",
            (false, Indicator::FileType) => b"*=>@|",
            (true, Indicator::FileType) => b" *=>@|",
            (false, Indicator::Classify) => b"=>@|",
            (true, Indicator::Classify) => b" =>@|",
        };
        self.quote_with(name, extra)
    }

    /// `name` quoted in the listing's style with `extra` bytes escaped too,
    /// and with `-q` applied (GNU `quote_name_buf`).
    fn quote_with(&self, name: &[u8], extra: &[u8]) -> (Vec<u8>, bool) {
        let o = self.opts;
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

    // ----- GNU colours (ls.c `print_name_with_quoting` and friends) -----

    /// GNU `put_indicator`: the first sequence is preceded by
    /// `prep_non_filename_text`, which resets the terminal's colours.
    fn put_indicator(&self, c: &LsColors, buf: &mut Vec<u8>, seq: &[u8]) {
        if !self.used_color.replace(true) {
            self.prep_non_filename_text(c, buf);
        }
        buf.extend_from_slice(seq);
        let colored = &self.dired.color_bytes;
        colored.set(colored.get() + seq.len() as u64);
    }

    /// GNU's `dired_pos` at offset `at` of the buffer about to be written:
    /// every byte so far except colour sequences (all of which are either
    /// written or in that buffer before `at`).
    fn dired_pos(&self, at: usize) -> u64 {
        (self.out.count + at as u64).saturating_sub(self.dired.color_bytes.get())
    }

    /// Appends the quoted `name`, as an OSC 8 hyperlink to `absolute` when
    /// there is one (GNU `quote_name`); returns where the name's text starts
    /// and ends in `buf`. With `skip_quotes`, the outer quotes stay outside
    /// the link.
    fn hyperlinked(
        &self,
        buf: &mut Vec<u8>,
        name: &[u8],
        absolute: Option<&[u8]>,
        skip_quotes: bool,
    ) -> (usize, usize) {
        let Some(absolute) = absolute else {
            let start = buf.len();
            buf.extend_from_slice(name);
            return (start, buf.len());
        };
        let skip_quotes = skip_quotes && name.len() >= 2;
        let inner = if skip_quotes {
            buf.push(name[0]);
            &name[1..name.len() - 1]
        } else {
            name
        };
        buf.extend_from_slice(b"\x1b]8;;file://");
        buf.extend_from_slice(&self.hostname);
        if absolute.first() != Some(&b'/') {
            buf.push(b'/');
        }
        file_escape(buf, absolute, true);
        buf.push(0x07);
        let start = buf.len();
        buf.extend_from_slice(inner);
        let end = buf.len();
        buf.extend_from_slice(b"\x1b]8;;\x07");
        if skip_quotes {
            buf.push(name[name.len() - 1]);
        }
        (start, end)
    }

    /// GNU's `--dired` trailer: the name offsets and the quoting style.
    pub(crate) fn dump_dired(&mut self) -> io::Result<()> {
        if !self.opts.dired {
            return Ok(());
        }
        let mut text = Vec::new();
        for (prefix, positions) in [
            ("//DIRED//", &*self.dired.names.borrow()),
            ("//SUBDIRED//", &self.dired.subdirs),
        ] {
            if !positions.is_empty() {
                text.extend_from_slice(prefix.as_bytes());
                for p in positions {
                    text.extend_from_slice(format!(" {p}").as_bytes());
                }
                text.push(b'\n');
            }
        }
        text.extend_from_slice(b"//DIRED-OPTIONS// --quoting-style=");
        text.extend_from_slice(self.opts.quoting.name().as_bytes());
        text.push(b'\n');
        self.out.write_all(&text)
    }

    /// GNU `prep_non_filename_text`: `ec`, or `lc` `rs` `rc`.
    fn prep_non_filename_text(&self, c: &LsColors, buf: &mut Vec<u8>) {
        if let Some(end) = &c.ind[C_END] {
            self.put_indicator(c, buf, end);
        } else {
            self.put_indicator(c, buf, c.seq(C_LEFT));
            self.put_indicator(c, buf, c.seq(C_RESET));
            self.put_indicator(c, buf, c.seq(C_RIGHT));
        }
    }

    /// GNU `set_normal_color`, written before each entry.
    fn set_normal_color(&self, c: &LsColors, buf: &mut Vec<u8>) {
        if c.is_colored(C_NORM) {
            self.put_indicator(c, buf, c.seq(C_LEFT));
            self.put_indicator(c, buf, c.seq(C_NORM));
            self.put_indicator(c, buf, c.seq(C_RIGHT));
        }
    }

    /// GNU's final `restore_default_color`, skipped when it would be a no-op.
    pub(crate) fn restore_default_color(&mut self) -> io::Result<()> {
        let Some(c) = &self.opts.colors else {
            return Ok(());
        };
        if self.used_color.get() && !(c.seq(C_LEFT) == b"\x1b[" && c.seq(C_RIGHT) == b"m") {
            let mut buf = c.seq(C_LEFT).to_vec();
            buf.extend_from_slice(c.seq(C_RIGHT));
            self.out.write_all(&buf)?;
        }
        Ok(())
    }

    /// Appends the quoted name of `f` (or of its link target), with the
    /// alignment space and, when colouring, GNU's colour sequences
    /// (`print_name_with_quoting`). Returns GNU's length: the quoted name's
    /// bytes plus the alignment space.
    fn name_with_quoting(
        &self,
        colors: Option<&LsColors>,
        buf: &mut Vec<u8>,
        f: &LsFile,
        w: &LsWidths,
        target: bool,
        start_col: usize,
    ) -> usize {
        let raw = if target {
            f.link_target.as_deref().unwrap_or_default()
        } else {
            &f.name
        };
        let (name, quoted) = self.quoted_name(raw);
        let pad = w.some_quoted && !quoted;
        if pad && !target {
            buf.push(b' ');
        }
        let len = name.len() + usize::from(pad);
        let color = colors.and_then(|c| self.color_of(c, f, target));
        if let (Some(c), Some(seq)) = (colors, color) {
            if c.is_colored(C_NORM) {
                self.put_indicator(c, buf, c.seq(C_LEFT));
                self.put_indicator(c, buf, c.seq(C_RIGHT));
            }
            self.put_indicator(c, buf, c.seq(C_LEFT));
            self.put_indicator(c, buf, seq);
            self.put_indicator(c, buf, c.seq(C_RIGHT));
        }
        // GNU leaves the outer quotes of an aligned name out of its link.
        let skip_quotes = w.some_quoted && quoted;
        let (start, end) = self.hyperlinked(buf, &name, f.absolute.as_deref(), skip_quotes);
        if self.opts.dired && !target {
            let pos = self.dired_pos(start);
            let end = self.dired_pos(end);
            self.dired.names.borrow_mut().extend([pos, end]);
        }
        let Some(c) = colors else {
            return len;
        };
        if color.is_some() || c.is_colored(C_NORM) {
            self.prep_non_filename_text(c, buf);
            // GNU clears to the end of the line when the name might wrap (it
            // judges by bytes, not display width).
            let ll = self.opts.line_length;
            if ll != 0 && len != 0 && start_col / ll != (start_col + len - 1) / ll {
                self.put_indicator(c, buf, c.seq(C_CLR_TO_EOL));
            }
        }
        len
    }

    /// GNU `linkok`: the symlink's target was stat'ed (only done when needed).
    fn link_ok(&self, f: &LsFile) -> bool {
        let o = self.opts;
        f.gnu_stat
            && f.link_target.is_some()
            && f.target_meta.is_some()
            && f.meta.as_ref().is_some_and(|m| m.file_type().is_symlink())
            && (o.format == LsFormat::Long || o.check_symlink_mode)
            && (o.indicator >= Indicator::FileType || o.check_symlink_mode)
    }

    /// GNU `get_color_indicator`: the sequence for `f`'s name, or for the
    /// target of its symlink.
    fn color_of<'c>(&self, c: &'c LsColors, f: &LsFile, target: bool) -> Option<&'c [u8]> {
        let link_ok = self.link_ok(f);
        let target_mode = || f.target_meta.as_ref().map_or(0, |m| m.mode());
        let own_mode = || f.meta.as_ref().map_or(0, |m| m.mode());
        // GNU's tri-state `linkok`: -1 for a missing target, else whether the
        // link is known to be ok.
        let (name, mode, linkok): (&[u8], u32, i8) = if target {
            let mode = if link_ok { target_mode() } else { 0 };
            let name = f.link_target.as_deref().unwrap_or_default();
            (name, mode, if link_ok { 0 } else { -1 })
        } else {
            let mode = if c.referent && link_ok {
                target_mode()
            } else {
                own_mode()
            };
            (&f.name, mode, i8::from(link_ok))
        };
        let stat_ok = f.gnu_stat && f.meta.is_some();
        let mut kind = if linkok == -1 && c.is_colored(C_MISSING) {
            C_MISSING
        } else if !stat_ok {
            match f.type_letter {
                b'p' => C_FIFO,
                b'c' => C_CHR,
                b'd' => C_DIR,
                b'b' => C_BLK,
                b'-' => C_FILE,
                b'l' => C_LINK,
                b's' => C_SOCK,
                _ => C_ORPHAN,
            }
        } else {
            const S_ISUID: u32 = 0o4000;
            const S_ISGID: u32 = 0o2000;
            const S_ISVTX: u32 = 0o1000;
            const S_IWOTH: u32 = 0o0002;
            match mode & 0o170_000 {
                0o100_000 => {
                    let nlink = f.meta.as_ref().map_or(0, |m| m.nlink());
                    if mode & S_ISUID != 0 && c.is_colored(C_SETUID) {
                        C_SETUID
                    } else if mode & S_ISGID != 0 && c.is_colored(C_SETGID) {
                        C_SETGID
                    } else if mode & 0o111 != 0 && c.is_colored(C_EXEC) {
                        // `ca` would be checked first, but the reference GNU
                        // build has no libcap and never reports capabilities.
                        C_EXEC
                    } else if nlink > 1 && c.is_colored(C_MULTIHARDLINK) {
                        C_MULTIHARDLINK
                    } else {
                        C_FILE
                    }
                }
                0o040_000 => {
                    if mode & S_ISVTX != 0
                        && mode & S_IWOTH != 0
                        && c.is_colored(C_STICKY_OTHER_WRITABLE)
                    {
                        C_STICKY_OTHER_WRITABLE
                    } else if mode & S_IWOTH != 0 && c.is_colored(C_OTHER_WRITABLE) {
                        C_OTHER_WRITABLE
                    } else if mode & S_ISVTX != 0 && c.is_colored(C_STICKY) {
                        C_STICKY
                    } else {
                        C_DIR
                    }
                }
                0o120_000 => C_LINK,
                0o010_000 => C_FIFO,
                0o140_000 => C_SOCK,
                0o060_000 => C_BLK,
                0o020_000 => C_CHR,
                _ => C_ORPHAN,
            }
        };
        if kind == C_FILE
            && let Some(seq) = c.suffix_color(name)
        {
            return Some(seq);
        }
        if kind == C_LINK && linkok == 0 && (c.referent || c.is_colored(C_ORPHAN)) {
            kind = C_ORPHAN;
        }
        c.ind[kind].as_deref()
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
                let ino = f.ino().map_or(1, |i| i.to_string().len());
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
                "{}total {}",
                if o.dired { "  " } else { "" },
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
                    self.write_cell(f, &w, false, 0, &cell.0)?;
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
                    self.write_cell(f, &w, true, pos, &cell)?;
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
        // GNU `max_idx`: the first column has no separator, so a partial
        // column's worth of width still counts.
        let max_idx = if o.line_length == 0 {
            n
        } else {
            o.line_length.div_ceil(MIN_COLUMN_WIDTH)
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
        let indent = |out: &mut Counted<W>, mut from: usize, to: usize| -> io::Result<()> {
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
                    self.write_cell(files[fileno], w, false, pos, cell)?;
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
            self.write_cell(files[0], w, false, 0, &cells[0].0)?;
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
                self.write_cell(files[fileno], w, false, pos, cell)?;
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
        let mut cell = self.short_prefix(f, w, commas);
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

    /// Writes one non-long entry starting at column `start_col`: `plain` as
    /// [`Self::short_cell`] built it, or the same with GNU's colours
    /// (`print_file_name_and_frills`).
    fn write_cell(
        &mut self,
        f: &LsFile,
        w: &LsWidths,
        commas: bool,
        start_col: usize,
        plain: &[u8],
    ) -> io::Result<()> {
        let o = self.opts;
        let colors = o.colors.as_ref();
        if colors.is_none() && f.absolute.is_none() {
            return self.out.write_all(plain);
        }
        let mut buf = Vec::with_capacity(plain.len() + 24);
        if let Some(c) = colors {
            self.set_normal_color(c, &mut buf);
        }
        buf.extend_from_slice(&self.short_prefix(f, w, commas));
        self.name_with_quoting(colors, &mut buf, f, w, false, start_col);
        if let Some(ch) = indicator_char(o.indicator, f.meta.as_ref(), f.type_letter) {
            buf.push(ch);
        }
        self.out.write_all(&buf)
    }

    /// The inode, block count and context columns before a non-long name.
    fn short_prefix(&self, f: &LsFile, w: &LsWidths, commas: bool) -> Vec<u8> {
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
            let ino = f.ino().map_or("?".into(), |i| i.to_string());
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
        cell
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
            let ino = f.ino().map_or("?".into(), |i| i.to_string());
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
        // GNU restarts its line buffer after these columns, and so counts
        // the name's starting column (for clearing to the end of the line)
        // from here.
        let col_origin = if o.owner || o.group || o.author || o.context {
            line.len()
        } else {
            0
        };
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
        let colors = o.colors.as_ref();
        let name_col = line.len() - col_origin;
        if colors.is_some() || o.dired {
            let mut prefixed = Vec::with_capacity(line.len() + 32);
            if let Some(c) = colors {
                self.set_normal_color(c, &mut prefixed);
            }
            if o.dired {
                prefixed.extend_from_slice(b"  ");
            }
            prefixed.append(&mut line);
            line = prefixed;
        }
        let name_len = self.name_with_quoting(colors, &mut line, f, w, false, name_col);
        let is_link = f.meta.as_ref().is_some_and(|m| m.file_type().is_symlink());
        if is_link {
            if f.link_target.is_some() {
                line.extend_from_slice(b" -> ");
                self.name_with_quoting(colors, &mut line, f, w, true, name_col + name_len + 4);
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
        let fmt = &self.opts.time_formats[usize::from(recent)];
        // Files in one directory often share a second: unless the format
        // shows fractions (`%N`), reuse the text of the last one.
        let cacheable = !fmt.contains('N');
        if cacheable
            && let Some((sec, was_recent, text)) = &*self.time_cache.borrow()
            && *sec == t.sec
            && *was_recent == recent
        {
            return text.clone();
        }
        let text = format_local_time(fmt, t);
        if cacheable {
            *self.time_cache.borrow_mut() = Some((t.sec, recent, text.clone()));
        }
        text
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
    if m.file_type().is_symlink() || has_xattrs(&f.path) == Some(false) {
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

fn bytes_path(b: &[u8]) -> &Path {
    use std::os::unix::ffi::OsStrExt;
    Path::new(std::ffi::OsStr::from_bytes(b))
}

/// GNU `file_escape`: `%XX` for every byte outside RFC 3986's unreserved
/// set (letters, digits, `-._~`), keeping `/` in a `path`.
pub(crate) fn file_escape(buf: &mut Vec<u8>, s: &[u8], path: bool) {
    for &c in s {
        if c.is_ascii_alphanumeric()
            || matches!(c, b'-' | b'.' | b'_' | b'~')
            || (path && c == b'/')
        {
            buf.push(c);
        } else {
            buf.extend_from_slice(format!("%{c:02x}").as_bytes());
        }
    }
}
