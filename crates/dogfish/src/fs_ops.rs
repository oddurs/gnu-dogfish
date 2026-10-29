#[cfg(target_os = "macos")]
use std::ffi::CStr;
use std::ffi::CString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Local};
use clap::Parser;
use dogfish_core::{
    ColumnAlign, DogfishContext, JournalAction, Style, Table, UndoJournal, copy_file_fast,
    emit_objects_or_table, format_mode_rwx, format_relative_time, format_size, gnu_io_error,
    io::ignore_broken_pipe, move_path, same_file, sparkline_bar,
};
use filetime::FileTime;
use jwalk::WalkDir;
use rayon::prelude::*;
use serde::Serialize;

use crate::sys::{get_groupname_by_gid, get_username_by_uid};

// ---------------------------------------------------------------------------
// 1. mkdir
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "mkdir",
    about = "Create directories if they do not already exist"
)]
struct MkdirArgs {
    /// No error if existing, make parent directories as needed
    #[arg(short = 'p', long = "parents")]
    parents: bool,

    /// Set file mode (as in chmod), e.g., 755
    #[arg(short = 'm', long = "mode")]
    mode: Option<String>,

    /// Print a message for each created directory
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    #[arg(required = true)]
    dirs: Vec<String>,
}

pub fn run_mkdir(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = MkdirArgs::try_parse_from(args)?;
    let mut exit_code = 0;

    for dir in &parsed.dirs {
        let res = if parsed.parents {
            fs::create_dir_all(dir)
        } else {
            fs::create_dir(dir)
        };
        match res {
            Ok(()) => {
                if let Some(ref m_str) = parsed.mode
                    && let Ok(oct) = u32::from_str_radix(m_str.trim_start_matches('0'), 8)
                {
                    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(oct));
                }
                ctx.telemetry.add_items(1);
                if parsed.verbose {
                    println!("mkdir: created directory '{}'", dir);
                }
            }
            Err(e) => {
                eprintln!("mkdir: cannot create directory '{}': {}", dir, e);
                exit_code = 1;
            }
        }
    }
    Ok(exit_code)
}

// ---------------------------------------------------------------------------
// 2. touch
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "touch",
    about = "Update the access and modification times of each FILE to the current time"
)]
struct TouchArgs {
    /// Do not create any files
    #[arg(short = 'c', long = "no-create")]
    no_create: bool,

    /// Change only the access time
    #[arg(short = 'a')]
    access_only: bool,

    /// Change only the modification time
    #[arg(short = 'm')]
    modify_only: bool,

    /// Use this file's times instead of current time
    #[arg(short = 'r', long = "reference")]
    reference: Option<String>,

    #[arg(required = true)]
    files: Vec<String>,
}

pub fn run_touch(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = TouchArgs::try_parse_from(args)?;
    let (ref_atime, ref_mtime) = if let Some(ref rpath) = parsed.reference {
        let meta = fs::metadata(rpath)?;
        (
            FileTime::from_last_access_time(&meta),
            FileTime::from_last_modification_time(&meta),
        )
    } else {
        let now = FileTime::now();
        (now, now)
    };

    for path_str in &parsed.files {
        let path = Path::new(path_str);
        if !path.exists() {
            if parsed.no_create {
                continue;
            }
            // Never truncate: another process may have created it since the check.
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
        }
        let meta = fs::metadata(path)?;
        let new_atime = if parsed.modify_only && !parsed.access_only {
            FileTime::from_last_access_time(&meta)
        } else {
            ref_atime
        };
        let new_mtime = if parsed.access_only && !parsed.modify_only {
            FileTime::from_last_modification_time(&meta)
        } else {
            ref_mtime
        };
        filetime::set_file_times(path, new_atime, new_mtime)?;
        ctx.telemetry.add_items(1);
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// 3. ln
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "ln", about = "Make links between files")]
struct LnArgs {
    /// Make symbolic links instead of hard links
    #[arg(short = 's', long = "symbolic")]
    symbolic: bool,

    /// Remove existing destination files
    #[arg(short = 'f', long = "force")]
    force: bool,

    /// Print name of each linked file
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    target: String,
    link_name: String,
}

pub fn run_ln(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = LnArgs::try_parse_from(args)?;
    let link_path = Path::new(&parsed.link_name);
    let final_link = if link_path.is_dir() {
        let fname = Path::new(&parsed.target)
            .file_name()
            .ok_or_else(|| anyhow!("invalid target"))?;
        link_path.join(fname)
    } else {
        link_path.to_path_buf()
    };

    // Resolve the target relative to the link's directory (how a symlink would) and
    // refuse to replace a file with a link to itself: `ln -f a a` would delete `a`.
    let target_from_link = final_link
        .parent()
        .unwrap_or(Path::new(""))
        .join(&parsed.target);
    let link_target = if parsed.symbolic {
        target_from_link.as_path()
    } else {
        Path::new(&parsed.target)
    };
    let link_is_symlink = final_link
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink());
    if !link_is_symlink && same_file(link_target, &final_link) {
        return Err(anyhow!(
            "ln: '{}' and '{}' are the same file",
            parsed.target,
            final_link.display()
        ));
    }
    if parsed.force && final_link.symlink_metadata().is_ok() {
        if final_link.symlink_metadata()?.is_dir() {
            return Err(anyhow!(
                "ln: '{}': cannot overwrite directory",
                final_link.display()
            ));
        }
        fs::remove_file(&final_link)?;
    }

    if parsed.symbolic {
        symlink(&parsed.target, &final_link)?;
    } else {
        fs::hard_link(&parsed.target, &final_link)?;
    }
    ctx.telemetry.add_items(1);
    if parsed.verbose {
        println!("'{}' -> '{}'", final_link.display(), parsed.target);
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// 4. chmod — Supports both octal (`755`) and symbolic (`+x`, `u+rw`, `go-w`) + `-R`
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "chmod", about = "Change file mode bits")]
#[command(allow_hyphen_values = true)]
struct ChmodArgs {
    /// Change files and directories recursively
    #[arg(short = 'R', long = "recursive")]
    recursive: bool,

    /// Output a diagnostic for every file processed
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    mode: String,

    #[arg(required = true)]
    files: Vec<String>,
}

pub fn run_chmod(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = ChmodArgs::try_parse_from(args)?;
    for f in &parsed.files {
        if parsed.recursive {
            for e in WalkDir::new(f).into_iter().flatten() {
                let p = e.path();
                apply_chmod(&p, &parsed.mode, parsed.verbose)?;
                ctx.telemetry.add_items(1);
            }
        } else {
            apply_chmod(Path::new(f), &parsed.mode, parsed.verbose)?;
            ctx.telemetry.add_items(1);
        }
    }
    Ok(0)
}

fn apply_chmod(path: &Path, mode_spec: &str, verbose: bool) -> Result<()> {
    let meta = fs::metadata(path)?;
    let current = meta.mode() & 0o7777;
    let new_mode = parse_chmod_mode(mode_spec, current)?;
    fs::set_permissions(path, fs::Permissions::from_mode(new_mode))?;
    if verbose {
        println!(
            "mode of '{}' changed from {:04o} to {:04o} ({})",
            path.display(),
            current,
            new_mode,
            &format_mode_rwx(new_mode)[1..]
        );
    }
    Ok(())
}

fn parse_chmod_mode(spec: &str, current: u32) -> Result<u32> {
    if let Ok(oct) = u32::from_str_radix(spec, 8) {
        return Ok(oct);
    }
    let mut mode = current;
    for clause in spec.split(',') {
        let op_idx = clause
            .find(['+', '-', '='])
            .ok_or_else(|| anyhow!("invalid mode: '{}'", spec))?;
        let (who_part, rest) = clause.split_at(op_idx);
        let op = rest.chars().next().unwrap();
        let perms_part = &rest[1..];

        let mut who_mask = 0u32;
        if who_part.is_empty() || who_part.contains('a') {
            who_mask = 0o777;
        } else {
            if who_part.contains('u') {
                who_mask |= 0o700;
            }
            if who_part.contains('g') {
                who_mask |= 0o070;
            }
            if who_part.contains('o') {
                who_mask |= 0o007;
            }
        }

        let mut perm_bits = 0u32;
        for ch in perms_part.chars() {
            match ch {
                'r' => perm_bits |= 0o444,
                'w' => perm_bits |= 0o222,
                'x' => perm_bits |= 0o111,
                'X' if current & 0o111 != 0 => {
                    perm_bits |= 0o111;
                }
                _ => {}
            }
        }
        let masked = perm_bits & who_mask;
        match op {
            '+' => mode |= masked,
            '-' => mode &= !masked,
            '=' => mode = (mode & !who_mask) | masked,
            _ => {}
        }
    }
    Ok(mode)
}

// ---------------------------------------------------------------------------
// 5. chown
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "chown", about = "Change file owner and group")]
struct ChownArgs {
    #[arg(short = 'R', long = "recursive")]
    recursive: bool,

    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    owner_group: String,

    #[arg(required = true)]
    files: Vec<String>,
}

pub fn run_chown(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = ChownArgs::try_parse_from(args)?;
    let (uid, gid) = parse_owner_group(&parsed.owner_group)?;

    for f in &parsed.files {
        if parsed.recursive {
            for e in WalkDir::new(f).into_iter().flatten() {
                chown_path(&e.path(), uid, gid)?;
                ctx.telemetry.add_items(1);
            }
        } else {
            chown_path(Path::new(f), uid, gid)?;
            ctx.telemetry.add_items(1);
        }
    }
    Ok(0)
}

fn parse_owner_group(spec: &str) -> Result<(Option<u32>, Option<u32>)> {
    let (u_part, g_part) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec, None),
    };
    let uid = if u_part.is_empty() {
        None
    } else if let Ok(id) = u_part.parse::<u32>() {
        Some(id)
    } else {
        let c_name = CString::new(u_part)?;
        let pw = unsafe { libc::getpwnam(c_name.as_ptr()) };
        if pw.is_null() {
            return Err(anyhow!("chown: invalid user: '{}'", u_part));
        }
        Some(unsafe { (*pw).pw_uid })
    };

    let gid = match g_part {
        Some(g) if !g.is_empty() => {
            if let Ok(id) = g.parse::<u32>() {
                Some(id)
            } else {
                let c_name = CString::new(g)?;
                let gr = unsafe { libc::getgrnam(c_name.as_ptr()) };
                if gr.is_null() {
                    return Err(anyhow!("chown: invalid group: '{}'", g));
                }
                Some(unsafe { (*gr).gr_gid })
            }
        }
        _ => None,
    };
    Ok((uid, gid))
}

fn chown_path(path: &Path, uid: Option<u32>, gid: Option<u32>) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = CString::new(path.as_os_str().as_bytes())?;
    let u = uid.unwrap_or(u32::MAX);
    let g = gid.unwrap_or(u32::MAX);
    let ret = unsafe { libc::chown(c_path.as_ptr(), u, g) };
    if ret != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 6. stat — Visual inspection card + `--json` + `-c FORMAT`
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "stat",
    about = "Display file or file system status with a rich visual card or JSON"
)]
struct StatArgs {
    /// Follow symbolic links
    #[arg(short = 'L', long = "dereference")]
    dereference: bool,

    /// Use the specified FORMAT instead of the default visual card
    #[arg(short = 'c', long = "format")]
    format: Option<String>,

    /// Emit structured JSON / NDJSON
    #[arg(short = 'J', long = "json")]
    json: bool,

    #[arg(required = true)]
    files: Vec<String>,
}

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
    let parsed = StatArgs::try_parse_from(args)?;
    let mut records = Vec::with_capacity(parsed.files.len());
    let mut exit_code = 0;

    for f in &parsed.files {
        let p = Path::new(f);
        let meta_res = if parsed.dereference {
            fs::metadata(p)
        } else {
            fs::symlink_metadata(p)
        };
        let meta = match meta_res {
            Ok(m) => m,
            Err(e) => {
                eprintln!("stat: cannot statx '{}': {}", f, e);
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

    if parsed.json {
        emit_objects_or_table(&records, ctx, true, false)?;
        return Ok(exit_code);
    }

    let st = ctx.style();
    for rec in &records {
        if let Some(ref fmt) = parsed.format {
            let out = fmt
                .replace("%n", &rec.name)
                .replace("%s", &rec.size.to_string())
                .replace("%F", &rec.file_type)
                .replace("%a", &rec.mode_octal)
                .replace("%A", &rec.mode_symbolic)
                .replace("%u", &rec.uid.to_string())
                .replace("%U", &rec.user)
                .replace("%g", &rec.gid.to_string())
                .replace("%G", &rec.group)
                .replace("%i", &rec.inode.to_string());
            println!("{}", out);
        } else {
            // Modern Dogfish Visual Card!
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
    }
    Ok(exit_code)
}

// ---------------------------------------------------------------------------
// 7. rm — Supports `--safe` (or `--undoable`) trash staging + parallel `-r`
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "rm",
    about = "Remove files or directories (supports --safe undoable trash staging)"
)]
struct RmArgs {
    /// Ignore nonexistent files and arguments, never prompt
    #[arg(short = 'f', long = "force")]
    force: bool,

    /// Remove directories and their contents recursively
    #[arg(short = 'r', visible_short_alias = 'R', long = "recursive")]
    recursive: bool,

    /// Remove empty directories
    #[arg(short = 'd', long = "dir")]
    dir: bool,

    /// Explain what is being done
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    /// Stage removed items to Dogfish's undoable trash (`dogfish undo` to restore!)
    #[arg(long = "safe", visible_alias = "trash")]
    safe: bool,

    /// Do not treat '/' specially (dangerous)
    #[arg(long = "no-preserve-root")]
    no_preserve_root: bool,

    #[arg(required = true)]
    files: Vec<String>,
}

/// True when `p` names the root directory itself (`/`, `//`, `/.`, `/usr/..`, a bind
/// of the same inode), as `--preserve-root` checks. A symlink pointing at `/` is not
/// the root: removing it only unlinks the link.
fn is_root_dir(p: &Path) -> bool {
    p.symlink_metadata().is_ok_and(|m| m.is_dir()) && same_file(p, Path::new("/"))
}

pub fn run_rm(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = RmArgs::try_parse_from(args)?;
    let journal = if parsed.safe {
        Some(UndoJournal::open()?)
    } else {
        None
    };

    ctx.telemetry.set_strategy(if parsed.safe {
        "journaled trash staging (undoable via `dogfish undo`)"
    } else {
        "direct unlink"
    });

    let mut exit_code = 0;
    let st = ctx.style();

    for f in &parsed.files {
        let p = Path::new(f);
        if !p.exists() && p.symlink_metadata().is_err() {
            if !parsed.force {
                eprintln!("rm: cannot remove '{}': No such file or directory", f);
                exit_code = 1;
            }
            continue;
        }

        if matches!(
            f.trim_end_matches('/').rsplit('/').next(),
            Some(".") | Some("..")
        ) {
            eprintln!(
                "rm: refusing to remove '.' or '..' directory: skipping '{}'",
                f
            );
            exit_code = 1;
            continue;
        }
        if parsed.recursive && !parsed.no_preserve_root && is_root_dir(p) {
            if f == "/" {
                eprintln!("rm: it is dangerous to operate recursively on '/'");
            } else {
                eprintln!(
                    "rm: it is dangerous to operate recursively on '{}' (same as '/')",
                    f
                );
            }
            eprintln!("rm: use --no-preserve-root to override this failsafe");
            exit_code = 1;
            continue;
        }
        // `--safe` must refuse exactly what plain `rm` refuses.
        if !parsed.recursive && p.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            let empty = fs::read_dir(p).is_ok_and(|mut d| d.next().is_none());
            if !parsed.dir || !empty {
                eprintln!(
                    "rm: cannot remove '{}': {}",
                    f,
                    if parsed.dir {
                        "Directory not empty"
                    } else {
                        "Is a directory"
                    }
                );
                exit_code = 1;
                continue;
            }
        }

        if let Some(ref j) = journal {
            match j.stage_to_trash(p) {
                Ok(entry) => {
                    ctx.telemetry.add_items(1);
                    if parsed.verbose {
                        println!(
                            "{} '{}' (undo id #{}: run `{}` to restore)",
                            st.yellow("trashed"),
                            f,
                            entry.id,
                            st.bold_cyan("dogfish undo")
                        );
                    }
                }
                Err(e) => {
                    eprintln!("rm: failed to stage '{}' to trash: {}", f, e);
                    exit_code = 1;
                }
            }
            continue;
        }

        let meta = match p.symlink_metadata() {
            Ok(m) => m,
            Err(e) => {
                if !parsed.force {
                    eprintln!("rm: cannot remove '{}': {}", f, e);
                    exit_code = 1;
                }
                continue;
            }
        };

        let res = if meta.is_dir() {
            if parsed.recursive {
                fs::remove_dir_all(p)
            } else if parsed.dir {
                fs::remove_dir(p)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::IsADirectory,
                    "is a directory (use -r to remove recursively)",
                ))
            }
        } else {
            fs::remove_file(p)
        };

        match res {
            Ok(()) => {
                ctx.telemetry.add_items(1);
                if parsed.verbose {
                    println!("removed '{}'", f);
                }
            }
            Err(e) => {
                if !parsed.force {
                    eprintln!("rm: cannot remove '{}': {}", f, e);
                    exit_code = 1;
                }
            }
        }
    }
    Ok(exit_code)
}

// ---------------------------------------------------------------------------
// 8. cp — APFS `clonefile` zero-copy CoW + UndoJournal recording
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "cp",
    about = "Copy files and directories (uses zero-copy CoW clonefile when supported)"
)]
struct CpArgs {
    /// Copy directories recursively
    #[arg(short = 'r', visible_short_alias = 'R', long = "recursive")]
    recursive: bool,

    /// Do not overwrite an existing file
    #[arg(short = 'n', long = "no-clobber")]
    no_clobber: bool,

    /// Explain what is being done
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    #[arg(required = true)]
    paths: Vec<String>,
}

pub fn run_cp(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = CpArgs::try_parse_from(args)?;
    if parsed.paths.len() < 2 {
        return Err(anyhow!("cp: missing destination file operand"));
    }
    let dst = Path::new(parsed.paths.last().unwrap());
    let sources = &parsed.paths[..parsed.paths.len() - 1];
    let dst_is_dir = dst.is_dir();

    if sources.len() > 1 && !dst_is_dir {
        return Err(anyhow!("cp: target '{}' is not a directory", dst.display()));
    }

    let journal = UndoJournal::open().ok();
    let mut exit_code = 0;

    for src_str in sources {
        let src = Path::new(src_str);
        let target = if dst_is_dir {
            dst.join(src.file_name().ok_or_else(|| anyhow!("invalid source"))?)
        } else {
            dst.to_path_buf()
        };
        if let Err(e) = cp_one(src, &target, &parsed, journal.as_ref(), ctx) {
            eprintln!("cp: {}", e);
            exit_code = 1;
        }
    }
    Ok(exit_code)
}

fn exists_no_follow(p: &Path) -> bool {
    p.symlink_metadata().is_ok()
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

fn cp_one(
    src: &Path,
    target: &Path,
    parsed: &CpArgs,
    journal: Option<&UndoJournal>,
    ctx: &DogfishContext,
) -> Result<()> {
    let src_meta = fs::metadata(src)
        .map_err(|e| anyhow!("cannot stat '{}': {}", src.display(), gnu_io_error(&e)))?;
    if same_file(src, target) {
        return Err(anyhow!(
            "'{}' and '{}' are the same file",
            src.display(),
            target.display()
        ));
    }
    if parsed.no_clobber && exists_no_follow(target) {
        return Ok(());
    }

    let target_is_dir = target.is_dir();
    if src_meta.is_dir() {
        if !parsed.recursive {
            return Err(anyhow!(
                "-r not specified; omitting directory '{}'",
                src.display()
            ));
        }
        if exists_no_follow(target) && !target_is_dir {
            return Err(anyhow!(
                "cannot overwrite non-directory '{}' with directory '{}'",
                target.display(),
                src.display()
            ));
        }
        let abs_src = fs::canonicalize(src)?;
        if resolve_for_containment(target).starts_with(&abs_src) {
            return Err(anyhow!(
                "cannot copy a directory, '{}', into itself, '{}'",
                src.display(),
                target.display()
            ));
        }
    } else if target_is_dir {
        return Err(anyhow!(
            "cannot overwrite directory '{}' with non-directory",
            target.display()
        ));
    }

    // Preserve whatever we are about to overwrite so `dogfish undo` can bring it back.
    // A directory copy merges into the existing tree, so it is backed up by copy;
    // a file is moved aside (cheap rename) and then recreated.
    let backup = match journal {
        Some(j) => j
            .backup_existing(target, src_meta.is_dir())
            .with_context(|| format!("cannot back up '{}' before overwriting", target.display()))?,
        None => None,
    };

    let result = if src_meta.is_dir() {
        copy_dir_cow(src, target, parsed.verbose, ctx)
    } else {
        copy_file_fast(src, target)
            .map(|(bytes, method)| {
                ctx.telemetry.set_strategy(method.as_str());
                ctx.telemetry.add_read(bytes);
                ctx.telemetry.add_written(bytes);
                ctx.telemetry.add_items(1);
                if parsed.verbose {
                    println!(
                        "'{}' -> '{}' ({})",
                        src.display(),
                        target.display(),
                        method.as_str()
                    );
                }
            })
            .map_err(|e| {
                anyhow!(
                    "cannot create regular file '{}': {}",
                    target.display(),
                    gnu_io_error(&e)
                )
            })
    };

    if let Err(e) = result {
        // Put a moved-aside file back so a failed copy never loses the original.
        if let (Some(b), false) = (&backup, src_meta.is_dir()) {
            let _ = fs::remove_file(target);
            let _ = move_path(Path::new(b), target);
        }
        return Err(e);
    }

    if let Some(j) = journal {
        let _ = j.record(
            "cp",
            JournalAction::CopyCreate {
                src: std::path::absolute(src)?.to_string_lossy().into_owned(),
                created_path: std::path::absolute(target)?.to_string_lossy().into_owned(),
                backup_path: backup,
            },
        );
    }
    Ok(())
}

fn copy_dir_cow(src: &Path, dst: &Path, verbose: bool, ctx: &DogfishContext) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let sub_dst = dst.join(entry.file_name());
        if ty.is_symlink() {
            // Like GNU `cp -R`: copy symlinks as symlinks, never follow them.
            if exists_no_follow(&sub_dst) {
                fs::remove_file(&sub_dst)?;
            }
            symlink(fs::read_link(entry.path())?, &sub_dst)?;
        } else if ty.is_dir() {
            copy_dir_cow(&entry.path(), &sub_dst, verbose, ctx)?;
        } else {
            let (bytes, method) = copy_file_fast(&entry.path(), &sub_dst)?;
            ctx.telemetry.set_strategy(method.as_str());
            ctx.telemetry.add_read(bytes);
            ctx.telemetry.add_written(bytes);
            ctx.telemetry.add_items(1);
            if verbose {
                println!("'{}' -> '{}'", entry.path().display(), sub_dst.display());
            }
        }
    }
    fs::set_permissions(dst, fs::metadata(src)?.permissions())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 9. mv — With UndoJournal recording (`dogfish undo` reverses `mv`!)
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "mv",
    about = "Move (rename) files (undoable via `dogfish undo`)"
)]
struct MvArgs {
    /// Do not overwrite an existing file
    #[arg(short = 'n', long = "no-clobber")]
    no_clobber: bool,

    /// Do not prompt before overwriting
    #[arg(short = 'f', long = "force")]
    force: bool,

    /// Explain what is being done
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    #[arg(required = true)]
    paths: Vec<String>,
}

pub fn run_mv(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = MvArgs::try_parse_from(args)?;
    if parsed.paths.len() < 2 {
        return Err(anyhow!("mv: missing destination file operand"));
    }
    let dst = Path::new(parsed.paths.last().unwrap());
    let sources = &parsed.paths[..parsed.paths.len() - 1];
    let dst_is_dir = dst.is_dir();

    if sources.len() > 1 && !dst_is_dir {
        return Err(anyhow!("mv: target '{}' is not a directory", dst.display()));
    }

    let journal = UndoJournal::open().ok();
    let mut exit_code = 0;

    for src_str in sources {
        let src = Path::new(src_str);
        let target = if dst_is_dir {
            dst.join(src.file_name().ok_or_else(|| anyhow!("invalid source"))?)
        } else {
            dst.to_path_buf()
        };
        match mv_one(src, &target, &parsed, journal.as_ref()) {
            Ok(true) => {
                ctx.telemetry.add_items(1);
                if parsed.verbose {
                    println!("renamed '{}' -> '{}'", src.display(), target.display());
                }
            }
            Ok(false) => {}
            Err(e) => {
                eprintln!("mv: {}", e);
                exit_code = 1;
            }
        }
    }
    Ok(exit_code)
}

/// Moves one source; returns `Ok(false)` when skipped by `--no-clobber`.
fn mv_one(
    src: &Path,
    target: &Path,
    parsed: &MvArgs,
    journal: Option<&UndoJournal>,
) -> Result<bool> {
    let src_meta = src
        .symlink_metadata()
        .map_err(|e| anyhow!("cannot stat '{}': {}", src.display(), gnu_io_error(&e)))?;
    if exists_no_follow(target) && same_file(src, target) {
        return Err(anyhow!(
            "'{}' and '{}' are the same file",
            src.display(),
            target.display()
        ));
    }
    if parsed.no_clobber && exists_no_follow(target) {
        return Ok(false);
    }
    if src_meta.is_dir() {
        let abs_src = fs::canonicalize(src)?;
        if resolve_for_containment(target).starts_with(&abs_src) {
            return Err(anyhow!(
                "cannot move '{}' to a subdirectory of itself, '{}'",
                src.display(),
                target.display()
            ));
        }
    }
    if let Ok(tm) = target.symlink_metadata() {
        if tm.is_dir() {
            if !src_meta.is_dir() {
                return Err(anyhow!(
                    "cannot overwrite directory '{}' with non-directory",
                    target.display()
                ));
            }
            if fs::read_dir(target)?.next().is_some() {
                return Err(anyhow!(
                    "cannot move '{}' to '{}': Directory not empty",
                    src.display(),
                    target.display()
                ));
            }
        } else if src_meta.is_dir() {
            return Err(anyhow!(
                "cannot overwrite non-directory '{}' with directory '{}'",
                target.display(),
                src.display()
            ));
        }
    }

    let abs_src = std::path::absolute(src)?;
    // Move an existing target aside first so `dogfish undo` can restore it.
    let backup = match journal {
        Some(j) => j
            .backup_existing(target, false)
            .with_context(|| format!("cannot back up '{}' before overwriting", target.display()))?,
        None => None,
    };

    if let Err(e) = move_path(src, target) {
        if let Some(b) = &backup {
            let _ = move_path(Path::new(b), target);
        }
        return Err(anyhow!(
            "cannot move '{}' to '{}': {}",
            src.display(),
            target.display(),
            gnu_io_error(&e)
        ));
    }

    if let Some(j) = journal {
        let _ = j.record(
            "mv",
            JournalAction::Move {
                src: abs_src.to_string_lossy().into_owned(),
                dst: std::path::absolute(target)?.to_string_lossy().into_owned(),
                backup_path: backup,
            },
        );
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// 10. df — Disk free space with Unicode sparkline bars & `--json`
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "df",
    about = "Report file system disk space usage with sparkline bars",
    disable_help_flag = true
)]
struct DfArgs {
    /// Print help (`-h` is reserved for human-readable output, as in GNU)
    #[arg(long = "help", action = clap::ArgAction::Help)]
    help: Option<bool>,

    /// Print sizes in powers of 1024 (e.g., 1023M)
    #[arg(short = 'h', long = "human-readable")]
    human: bool,

    /// List inode information instead of block usage
    #[arg(short = 'i', long = "inodes")]
    inodes: bool,

    /// Emit structured JSON / NDJSON
    #[arg(short = 'J', long = "json")]
    json: bool,
}

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

pub fn run_df(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = DfArgs::try_parse_from(args)?;
    let mounts = get_mounted_filesystems()?;

    if parsed.json {
        emit_objects_or_table(&mounts, ctx, true, false)?;
        return Ok(0);
    }

    let human = parsed.human || ctx.stdout_is_tty;
    let st = ctx.style();
    let mut table = Table::new(vec![
        ("FILESYSTEM", ColumnAlign::Left),
        ("TYPE", ColumnAlign::Left),
        ("SIZE", ColumnAlign::Right),
        ("USED", ColumnAlign::Right),
        ("AVAIL", ColumnAlign::Right),
        ("USAGE", ColumnAlign::Left),
        ("MOUNTED ON", ColumnAlign::Left),
    ]);

    for m in &mounts {
        let ratio = (m.use_percent / 100.0).clamp(0.0, 1.0);
        let usage_col = format!("{} {:>5.1}%", sparkline_bar(ratio, 10, st), m.use_percent);
        table.add_row(vec![
            st.bold(&m.filesystem),
            st.dim(&m.fs_type),
            format_size(m.total_bytes, human),
            st.yellow(&format_size(m.used_bytes, human)),
            st.green(&format_size(m.available_bytes, human)),
            usage_col,
            st.bold_cyan(&m.mount_point),
        ]);
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    table.render(&mut out, st)?;
    Ok(0)
}

#[cfg(target_os = "macos")]
fn get_mounted_filesystems() -> Result<Vec<DfRecord>> {
    let mut mntbuf: *mut libc::statfs = std::ptr::null_mut();
    let count = unsafe { libc::getmntinfo(&mut mntbuf, libc::MNT_NOWAIT) };
    if count <= 0 || mntbuf.is_null() {
        return Ok(Vec::new());
    }
    let slice = unsafe { std::slice::from_raw_parts(mntbuf, count as usize) };
    let mut records = Vec::new();
    for s in slice {
        let total = s.f_blocks.saturating_mul(s.f_bsize as u64);
        if total == 0 {
            continue;
        }
        let avail = s.f_bavail.saturating_mul(s.f_bsize as u64);
        let free = s.f_bfree.saturating_mul(s.f_bsize as u64);
        let used = total.saturating_sub(free);
        let denom = used + avail;
        let pct = if denom > 0 {
            (used as f64 / denom as f64) * 100.0
        } else {
            0.0
        };

        let cstr = |ptr: *const libc::c_char| unsafe {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        };
        let fs_name = cstr(s.f_mntfromname.as_ptr());
        let fs_type = cstr(s.f_fstypename.as_ptr());
        let mount_on = cstr(s.f_mntonname.as_ptr());

        if fs_type == "devfs" || fs_type == "autofs" {
            continue;
        }

        records.push(DfRecord {
            filesystem: fs_name,
            fs_type,
            total_bytes: total,
            used_bytes: used,
            available_bytes: avail,
            use_percent: (pct * 10.0).round() / 10.0,
            mount_point: mount_on,
        });
    }
    Ok(records)
}

#[cfg(target_os = "linux")]
fn get_mounted_filesystems() -> Result<Vec<DfRecord>> {
    let mounts_content = fs::read_to_string("/proc/mounts")
        .or_else(|_| fs::read_to_string("/etc/mtab"))
        .unwrap_or_default();
    let mut records = Vec::new();
    let mut seen_mounts = std::collections::HashSet::new();

    for line in mounts_content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let fs_name = parts[0];
        let mount_on = parts[1].replace("\\040", " ");
        let fs_type = parts[2];

        // Skip virtual/pseudo filesystems
        if matches!(
            fs_type,
            "proc"
                | "sysfs"
                | "devtmpfs"
                | "devpts"
                | "cgroup"
                | "cgroup2"
                | "pstore"
                | "securityfs"
                | "debugfs"
                | "tracefs"
                | "configfs"
                | "fusectl"
                | "mqueue"
                | "hugetlbfs"
                | "autofs"
                | "binfmt_misc"
                | "bpf"
                | "efivarfs"
                | "nsfs"
                | "ramfs"
                | "squashfs"
        ) {
            continue;
        }
        if !seen_mounts.insert(mount_on.clone()) {
            continue;
        }

        let Ok(c_mount) = CString::new(mount_on.as_bytes()) else {
            continue;
        };
        let mut st_vfs: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c_mount.as_ptr(), &mut st_vfs) } != 0 {
            continue;
        }

        let bsize = if st_vfs.f_frsize > 0 {
            st_vfs.f_frsize as u64
        } else {
            st_vfs.f_bsize as u64
        };
        let total = (st_vfs.f_blocks as u64).saturating_mul(bsize);
        if total == 0 {
            continue;
        }
        let avail = (st_vfs.f_bavail as u64).saturating_mul(bsize);
        let free = (st_vfs.f_bfree as u64).saturating_mul(bsize);
        let used = total.saturating_sub(free);
        let denom = used + avail;
        let pct = if denom > 0 {
            (used as f64 / denom as f64) * 100.0
        } else {
            0.0
        };

        records.push(DfRecord {
            filesystem: fs_name.to_string(),
            fs_type: fs_type.to_string(),
            total_bytes: total,
            used_bytes: used,
            available_bytes: avail,
            use_percent: (pct * 10.0).round() / 10.0,
            mount_point: mount_on,
        });
    }
    Ok(records)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn get_mounted_filesystems() -> Result<Vec<DfRecord>> {
    Ok(Vec::new())
}

// ---------------------------------------------------------------------------
// 11. du — Parallel `jwalk` disk usage analyzer with sparklines & `--json`
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "du",
    about = "Estimate file space usage in parallel with sparkline bars",
    disable_help_flag = true
)]
struct DuArgs {
    /// Print help (`-h` is reserved for human-readable output, as in GNU)
    #[arg(long = "help", action = clap::ArgAction::Help)]
    help: Option<bool>,

    /// Display only a total for each argument
    #[arg(short = 's', long = "summarize")]
    summarize: bool,

    /// Print sizes in human readable format (e.g., 1K 234M 2G)
    #[arg(short = 'h', long = "human-readable")]
    human: bool,

    /// Print apparent sizes rather than disk usage
    #[arg(short = 'b', long = "bytes")]
    bytes: bool,

    /// Max directory depth to print
    #[arg(short = 'd', long = "max-depth")]
    max_depth: Option<usize>,

    /// Emit structured JSON / NDJSON
    #[arg(short = 'J', long = "json")]
    json: bool,

    paths: Vec<String>,
}

#[derive(Serialize, Clone)]
struct DuRecord {
    path: String,
    size: u64,
    size_human: String,
    files: u64,
}

pub fn run_du(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = DuArgs::try_parse_from(args)?;
    let targets = if parsed.paths.is_empty() {
        vec![".".to_string()]
    } else {
        parsed.paths.clone()
    };

    ctx.telemetry
        .set_strategy("jwalk work-stealing parallel traversal");
    let mut records = Vec::new();

    for t in &targets {
        if parsed.summarize || parsed.max_depth == Some(0) {
            let (sz, count) = compute_dir_size_parallel(Path::new(t), parsed.bytes);
            ctx.telemetry.add_items(count);
            records.push(DuRecord {
                path: t.clone(),
                size: sz,
                size_human: format_size(sz, true),
                files: count,
            });
        } else {
            let root = Path::new(t);
            let mut subdirs: Vec<PathBuf> = Vec::new();
            let mut root_direct_sz = 0_u64;
            let mut root_direct_cnt = 0_u64;
            if root.is_dir()
                && let Ok(rd) = fs::read_dir(root)
            {
                for e in rd.flatten() {
                    if let Ok(meta) = e.metadata() {
                        if meta.is_dir() {
                            subdirs.push(e.path());
                        } else {
                            root_direct_cnt += 1;
                            root_direct_sz += if parsed.bytes {
                                meta.len()
                            } else {
                                meta.blocks() * 512
                            };
                        }
                    }
                }
            }
            let mut sub_recs: Vec<DuRecord> = subdirs
                .par_iter()
                .map(|p| {
                    let (sz, cnt) = compute_dir_size_parallel(p, parsed.bytes);
                    DuRecord {
                        path: p.to_string_lossy().into_owned(),
                        size: sz,
                        size_human: format_size(sz, true),
                        files: cnt,
                    }
                })
                .collect();
            let total_sz: u64 = root_direct_sz + sub_recs.iter().map(|r| r.size).sum::<u64>();
            let total_cnt: u64 = root_direct_cnt + sub_recs.iter().map(|r| r.files).sum::<u64>();
            sub_recs.sort_unstable_by_key(|r| std::cmp::Reverse(r.size));
            ctx.telemetry.add_items(total_cnt);
            records.extend(sub_recs);
            records.push(DuRecord {
                path: t.clone(),
                size: total_sz,
                size_human: format_size(total_sz, true),
                files: total_cnt,
            });
        }
    }

    if parsed.json {
        emit_objects_or_table(&records, ctx, true, false)?;
        return Ok(0);
    }

    let human = parsed.human || (ctx.stdout_is_tty && !parsed.bytes);
    let max_sz = records.iter().map(|r| r.size).max().unwrap_or(1).max(1);
    let st = ctx.style();
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    for r in &records {
        let sz_str = if human {
            format!("{:>10}", format_size(r.size, true))
        } else {
            format!("{:>10}", r.size / 1024)
        };
        if ctx.stdout_is_tty {
            let ratio = r.size as f64 / max_sz as f64;
            writeln!(
                out,
                "{}  {}  {}",
                st.bold_green(&sz_str),
                sparkline_bar(ratio, 10, st),
                st.bold(&r.path)
            )?;
        } else {
            writeln!(out, "{}\t{}", sz_str.trim(), r.path)?;
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}

fn compute_dir_size_parallel(root: &Path, apparent_bytes: bool) -> (u64, u64) {
    let mut total_size = 0_u64;
    let mut file_count = 0_u64;
    for entry in WalkDir::new(root).skip_hidden(false) {
        if let Ok(e) = entry
            && let Ok(meta) = e.metadata()
            && !meta.is_dir()
        {
            file_count += 1;
            if apparent_bytes {
                total_size += meta.len();
            } else {
                total_size += meta.blocks() * 512;
            }
        }
    }
    (total_size, file_count)
}

// ---------------------------------------------------------------------------
// 12. ls — Modernized directory lister (`-l`, `-a`, `--tree`, `-S`, `-t`, `--json`)
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(
    name = "ls",
    about = "List directory contents with smart colors, tree view, and NDJSON output",
    disable_help_flag = true
)]
struct LsArgs {
    /// Print help (`-h` is reserved for human-readable output, as in GNU)
    #[arg(long = "help", action = clap::ArgAction::Help)]
    help: Option<bool>,

    /// Do not ignore entries starting with .
    #[arg(short = 'a', long = "all")]
    all: bool,

    /// Use a long listing format
    #[arg(short = 'l', long = "long")]
    long: bool,

    /// Print human readable sizes (default on TTY)
    #[arg(short = 'h', long = "human-readable")]
    human: bool,

    /// List one file per line
    #[arg(short = '1')]
    one_per_line: bool,

    /// Reverse order while sorting
    #[arg(short = 'r', long = "reverse")]
    reverse: bool,

    /// Sort by file size, largest first
    #[arg(short = 'S')]
    sort_size: bool,

    /// Sort by time, newest first
    #[arg(short = 't')]
    sort_time: bool,

    /// Append indicator (one of */=>@|) to entries
    #[arg(short = 'F', long = "classify")]
    classify: bool,

    /// Render directory hierarchy as an indented visual tree
    #[arg(long = "tree", visible_short_alias = 'T')]
    tree: bool,

    /// Maximum depth for `--tree` view
    #[arg(short = 'L', long = "level", default_value_t = 3)]
    level: usize,

    /// Emit structured JSON / NDJSON (`dogfish ls -J | dogfish sort --by size`)
    #[arg(short = 'J', long = "json")]
    json: bool,

    paths: Vec<String>,
}

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

pub fn run_ls(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = LsArgs::try_parse_from(args)?;
    let targets = if parsed.paths.is_empty() {
        vec![".".to_string()]
    } else {
        parsed.paths.clone()
    };

    let st = ctx.style();

    if parsed.tree {
        ctx.telemetry.set_strategy("recursive visual tree renderer");
        let stdout = io::stdout();
        let mut out = io::BufWriter::new(stdout.lock());
        for t in &targets {
            writeln!(out, "{}", st.bold_blue(t))?;
            render_tree(
                Path::new(t),
                "",
                0,
                parsed.level,
                parsed.all,
                &mut out,
                st,
                ctx,
            )?;
        }
        ignore_broken_pipe(out.flush())?;
        return Ok(0);
    }

    let mut all_entries = Vec::new();
    for t in &targets {
        let p = Path::new(t);
        if p.is_dir() {
            let mut entries = collect_dir_entries(p, parsed.all)?;
            sort_ls_entries(&mut entries, &parsed);
            all_entries.extend(entries);
        } else if p.exists() || p.symlink_metadata().is_ok() {
            if let Ok(e) = build_ls_entry(p, t.clone()) {
                all_entries.push(e);
            }
        } else {
            eprintln!("ls: cannot access '{}': No such file or directory", t);
            return Ok(2);
        }
    }

    ctx.telemetry.add_items(all_entries.len() as u64);

    if parsed.json {
        ctx.telemetry
            .set_strategy("NDJSON structured object stream");
        emit_objects_or_table(&all_entries, ctx, true, false)?;
        return Ok(0);
    }

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    if parsed.long {
        let mut tbl = Table::new(vec![
            ("PERMS", ColumnAlign::Left),
            ("OWNER", ColumnAlign::Left),
            ("SIZE", ColumnAlign::Right),
            ("MODIFIED", ColumnAlign::Right),
            ("NAME", ColumnAlign::Left),
        ]);
        for e in &all_entries {
            let perm_str = colorize_mode(&e.mode, st);
            let sz_str = if e.kind == "dir" {
                st.dim("-")
            } else {
                st.green(&format_size(e.size, true))
            };
            let name_str = style_entry_name(&e.name, &e.kind, &e.mode, parsed.classify, st);
            tbl.add_row(vec![
                perm_str,
                st.dim(&e.user),
                sz_str,
                st.cyan(&e.modified_relative),
                name_str,
            ]);
        }
        tbl.render(&mut out, st)?;
    } else if parsed.one_per_line || !ctx.stdout_is_tty {
        for e in &all_entries {
            let name_str = style_entry_name(&e.name, &e.kind, &e.mode, parsed.classify, st);
            ignore_broken_pipe(writeln!(out, "{}", name_str))?;
        }
    } else {
        // Multi-column grid on interactive TTY
        let styled_names: Vec<(String, usize)> = all_entries
            .iter()
            .map(|e| {
                let s = style_entry_name(&e.name, &e.kind, &e.mode, parsed.classify, st);
                let w = dogfish_core::table::visible_width(&s);
                (s, w)
            })
            .collect();
        let max_w = styled_names.iter().map(|(_, w)| *w).max().unwrap_or(8) + 3;
        let cols = (88 / max_w.max(1)).max(1);
        for chunk in styled_names.chunks(cols) {
            for (s, w) in chunk {
                write!(out, "{}{}", s, " ".repeat(max_w.saturating_sub(*w)))?;
            }
            writeln!(out)?;
        }
    }

    ignore_broken_pipe(out.flush())?;
    Ok(0)
}

fn collect_dir_entries(dir: &Path, show_all: bool) -> Result<Vec<LsEntry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !show_all && name.starts_with('.') {
            continue;
        }
        if let Ok(item) = build_ls_entry(&entry.path(), name) {
            entries.push(item);
        }
    }
    Ok(entries)
}

fn build_ls_entry(path: &Path, name: String) -> Result<LsEntry> {
    let meta = fs::symlink_metadata(path)?;
    let mode = meta.mode();
    let rwx = format_mode_rwx(mode);
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
    let uid = meta.uid();
    let gid = meta.gid();
    Ok(LsEntry {
        name,
        path: path.to_string_lossy().into_owned(),
        kind: kind.to_string(),
        size: meta.len(),
        mode: rwx,
        mode_octal: format!("{:04o}", mode & 0o7777),
        user: get_username_by_uid(uid).unwrap_or_else(|| uid.to_string()),
        group: get_groupname_by_gid(gid).unwrap_or_else(|| gid.to_string()),
        modified_unix: meta.mtime(),
        modified_relative: format_relative_time(mtime),
    })
}

fn sort_ls_entries(entries: &mut [LsEntry], args: &LsArgs) {
    entries.sort_by(|a, b| {
        let ord = if args.sort_size {
            b.size.cmp(&a.size)
        } else if args.sort_time {
            b.modified_unix.cmp(&a.modified_unix)
        } else {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
        };
        if args.reverse { ord.reverse() } else { ord }
    });
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
// 13. realpath
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "realpath", about = "Print the resolved absolute file path")]
struct RealpathArgs {
    /// Do not strip symlinks or require all components to exist (-m)
    #[arg(short = 's', long = "strip", visible_short_alias = 'm')]
    no_symlinks: bool,

    #[arg(short = 'J', long = "json")]
    json: bool,

    #[arg(required = true)]
    paths: Vec<String>,
}

pub fn run_realpath(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = RealpathArgs::try_parse_from(args)?;
    let mut exit_code = 0;
    let mut results = Vec::new();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    for p_str in &parsed.paths {
        let p = Path::new(p_str);
        let resolved = if parsed.no_symlinks {
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd.join(p)
            }
        } else {
            match fs::canonicalize(p) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("realpath: {}: {}", p_str, e);
                    exit_code = 1;
                    continue;
                }
            }
        };
        ctx.telemetry.add_items(1);
        let r_str = resolved.to_string_lossy().into_owned();
        if parsed.json {
            results.push(serde_json::json!({ "input": p_str, "realpath": r_str }));
        } else {
            println!("{}", r_str);
        }
    }
    if parsed.json {
        emit_objects_or_table(&results, ctx, true, false)?;
    }
    Ok(exit_code)
}

// ---------------------------------------------------------------------------
// 14. basename
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "basename", about = "Strip directory and suffix from filenames")]
struct BasenameArgs {
    /// Support multiple arguments and treat each as a NAME
    #[arg(short = 'a', long = "multiple")]
    multiple: bool,

    /// Remove a trailing SUFFIX; implies -a
    #[arg(short = 's', long = "suffix")]
    suffix: Option<String>,

    #[arg(required = true)]
    names: Vec<String>,
}

pub fn run_basename(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = BasenameArgs::try_parse_from(args)?;
    if !parsed.multiple && parsed.suffix.is_none() {
        let name = &parsed.names[0];
        let suf = parsed.names.get(1).map(|s| s.as_str());
        println!("{}", compute_basename(name, suf));
        ctx.telemetry.add_items(1);
    } else {
        let suf = parsed.suffix.as_deref();
        for name in &parsed.names {
            println!("{}", compute_basename(name, suf));
            ctx.telemetry.add_items(1);
        }
    }
    Ok(0)
}

fn compute_basename(path: &str, suffix: Option<&str>) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    let base = Path::new(trimmed)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| trimmed.to_string());
    if let Some(suf) = suffix
        && base != suf
        && base.ends_with(suf)
    {
        return base[..base.len() - suf.len()].to_string();
    }
    base
}

// ---------------------------------------------------------------------------
// 15. dirname
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "dirname", about = "Strip last component from file name")]
struct DirnameArgs {
    #[arg(required = true)]
    names: Vec<String>,
}

pub fn run_dirname(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = DirnameArgs::try_parse_from(args)?;
    for name in &parsed.names {
        let trimmed = name.trim_end_matches('/');
        if trimmed.is_empty() {
            println!("/");
        } else {
            let parent = Path::new(trimmed)
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| ".".to_string());
            if parent.is_empty() {
                println!(".");
            } else {
                println!("{}", parent);
            }
        }
        ctx.telemetry.add_items(1);
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// 16. mktemp
// ---------------------------------------------------------------------------
#[derive(Parser)]
#[command(name = "mktemp", about = "Create a temporary file or directory safely")]
struct MktempArgs {
    /// Create a directory, not a file
    #[arg(short = 'd', long = "directory")]
    directory: bool,

    /// Interpret TEMPLATE relative to DIR
    #[arg(short = 'p', long = "tmpdir")]
    tmpdir: Option<String>,

    /// Optional template (e.g. tmp.XXXXXX)
    template: Option<String>,
}

pub fn run_mktemp(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    use std::io::Read;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    let parsed = MktempArgs::try_parse_from(args)?;
    // Like GNU: an explicit TEMPLATE is relative to the current directory unless
    // -p is given; with no template, use $TMPDIR (or /tmp).
    let base_dir = match (&parsed.tmpdir, &parsed.template) {
        (Some(dir), _) => PathBuf::from(dir),
        (None, Some(_)) => PathBuf::new(),
        (None, None) => std::env::temp_dir(),
    };

    let tmpl = parsed
        .template
        .unwrap_or_else(|| "tmp.XXXXXXXXXX".to_string());
    let prefix = tmpl.trim_end_matches('X');
    let x_count = tmpl.len() - prefix.len();
    if x_count < 3 {
        return Err(anyhow!("mktemp: too few X's in template '{}'", tmpl));
    }

    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut urandom = fs::File::open("/dev/urandom").context("mktemp: cannot open /dev/urandom")?;
    let mut bytes = vec![0u8; x_count];

    // Names must be unpredictable and creation exclusive (O_EXCL, never following an
    // existing symlink), with restrictive permissions applied atomically at creation.
    for _ in 0..100 {
        urandom.read_exact(&mut bytes)?;
        let suffix: String = bytes
            .iter()
            .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
            .collect();
        let final_path = base_dir.join(format!("{}{}", prefix, suffix));
        let res = if parsed.directory {
            fs::DirBuilder::new().mode(0o700).create(&final_path)
        } else {
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&final_path)
                .map(|_| ())
        };
        match res {
            Ok(()) => {
                ctx.telemetry.add_items(1);
                println!("{}", final_path.display());
                return Ok(0);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(anyhow!(
                    "mktemp: failed to create {} via template '{}': {}",
                    if parsed.directory {
                        "directory"
                    } else {
                        "file"
                    },
                    base_dir.join(&tmpl).display(),
                    gnu_io_error(&e)
                ));
            }
        }
    }
    Err(anyhow!(
        "mktemp: failed to create a unique name from template '{}'",
        tmpl
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
