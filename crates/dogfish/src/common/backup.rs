//! `--backup` version control and backup file names, as in GNU `backupfile` (cp, ln, mv).

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use dogfish_core::{JournalAction, UndoJournal, move_path};

use crate::common::path::absolute_lossy;
use crate::common::tool_args::argmatch;

/// `--backup` version control, as in GNU `backupfile`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackupMode {
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
pub(crate) fn backup_mode(tool: &str, arg: Option<&str>) -> std::result::Result<BackupMode, i32> {
    match arg {
        Some(a) => argmatch(tool, "backup type", a, BACKUP_ARGS),
        None => match std::env::var("VERSION_CONTROL") {
            Ok(v) if !v.is_empty() => argmatch(tool, "$VERSION_CONTROL", &v, BACKUP_ARGS),
            _ => Ok(BackupMode::Existing),
        },
    }
}

/// `-S SUFFIX`, else `$SIMPLE_BACKUP_SUFFIX`, else `~` (a suffix with a `/` is ignored).
pub(crate) fn backup_suffix(arg: Option<String>) -> String {
    arg.or_else(|| std::env::var("SIMPLE_BACKUP_SUFFIX").ok())
        .filter(|s| !s.is_empty() && !s.contains('/'))
        .unwrap_or_else(|| "~".to_string())
}

/// The name an existing `dst` is renamed to before being replaced.
pub(crate) fn backup_name(dst: &Path, mode: BackupMode, suffix: &str) -> PathBuf {
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
pub(crate) fn make_backup(
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
