//! df — Sparkline table on a TTY, GNU `df` output otherwise

#[cfg(target_os = "macos")]
use std::ffi::CStr;
use std::ffi::CString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::Result;
use dogfish_core::{
    ColumnAlign, DogfishContext, Style, Table, emit_objects_or_table, format_size, gnu_io_error,
    io::ignore_broken_pipe, sparkline_bar, stdout,
};
use serde::Serialize;

use crate::common::file_info::find_mount_point;
use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy_all, print_help, print_version, usage_error,
};
use crate::common::human::{
    HUMAN_AUTOSCALE, HUMAN_B, HUMAN_BASE_1024, HUMAN_SI, HUMAN_SUPPRESS_POINT_ZERO, env_block_size,
    human_readable, parse_block_size,
};
use crate::common::quote::{Quoting, display_width, quote_bytes, quote_f};

const DF_OPTIONS: &[OptSpec<&str>] = &[
    OptSpec::both(b'a', "all", HasArg::No, "all"),
    OptSpec::both(b'B', "block-size", HasArg::Required, "block-size"),
    OptSpec::both(b'i', "inodes", HasArg::No, "inodes"),
    OptSpec::both(b'h', "human-readable", HasArg::No, "human-readable"),
    OptSpec::both(b'H', "si", HasArg::No, "si"),
    OptSpec::both(b'l', "local", HasArg::No, "local"),
    OptSpec::long("output", HasArg::Optional, "output"),
    OptSpec::both(b'P', "portability", HasArg::No, "portability"),
    OptSpec::both(b'T', "print-type", HasArg::No, "print-type"),
    OptSpec::long("sync", HasArg::No, "sync"),
    OptSpec::long("no-sync", HasArg::No, "no-sync"),
    OptSpec::long("total", HasArg::No, "total"),
    OptSpec::both(b't', "type", HasArg::Required, "type"),
    OptSpec::both(b'x', "exclude-type", HasArg::Required, "exclude-type"),
    OptSpec::long("help", HasArg::No, "help"),
    OptSpec::long("version", HasArg::No, "version"),
    // Dogfish extensions.
    OptSpec::both(b'J', "json", HasArg::No, "json").exact(),
    // Short-only options.
    OptSpec::short(b'k', HasArg::No, "kilobytes"),
    OptSpec::short(b'm', HasArg::No, "megabytes"),
    OptSpec::short(b'F', HasArg::Required, "type"),
    OptSpec::short(b'v', HasArg::No, "ignored"),
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

pub fn run_df(args: Vec<std::ffi::OsString>, ctx: &DogfishContext) -> Result<i32> {
    let mut parsed = Getopt::new(args.into_iter().skip(1).collect(), DF_OPTIONS);
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
    while let Some(item) = parsed.next_text() {
        let opt = match item {
            Ok(o) => o,
            Err(e) => return Ok(getopt_error("df", &e)),
        };
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
                        return Ok(usage_error("df", &msg));
                    };
                    if cfg.fields.contains(field) {
                        let msg = format!(
                            "option --output: field {} used more than once",
                            quote_bytes(name.as_bytes(), Quoting::Locale)
                        );
                        return Ok(usage_error("df", &msg));
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
            "help" => return Ok(print_help(DF_HELP)),
            "version" => return Ok(print_version("df")),
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
    let operands = lossy_all(parsed.operands());
    if operands.is_empty() {
        df_all_rows(&mounts, &cfg, &mut rows, &mut status);
    } else {
        for name in &operands {
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
    let mut out = io::BufWriter::new(stdout::stdout());
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
    usage_error("df", &msg)
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
