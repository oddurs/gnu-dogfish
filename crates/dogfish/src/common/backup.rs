//! `--backup` version control and backup file names, as in GNU `backupfile` (cp, ln, mv).

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use dogfish_core::journal::inode;
use dogfish_core::{JournalAction, JournalEntry, Take, UndoJournal, crash_point, move_path};

use crate::common::getopt::argmatch;
use crate::common::path::{base_len, c_path, last_component, rename_noreplace, stable_path};

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

/// The words `--backup=CONTROL` accepts, for the manual pages and
/// completions.
pub(crate) fn backup_words() -> impl Iterator<Item = &'static str> {
    BACKUP_ARGS.iter().map(|(w, _)| *w)
}

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

/// The name an existing `dst` is renamed to before being replaced (gnulib
/// `find_backup_file_name`). Trailing slashes on `dst` are ignored.
pub(crate) fn backup_name(dst: &Path, mode: BackupMode, suffix: &str) -> PathBuf {
    backup_candidate(dst, mode, suffix).0
}

/// What a scan of the directory found (gnulib's `numbered_backup_result`).
#[derive(PartialEq, Eq)]
enum Numbered {
    /// As long as an existing backup name, so known to fit.
    SameLength,
    /// Longer than every existing backup name.
    Longer,
    /// No numbered backups yet.
    New,
}

/// gnulib `backupfile_internal` without the rename: the backup name, the mode
/// actually used (`existing` becomes `simple` when there are no numbered
/// backups) and whether the name is known to fit the file system.
fn backup_candidate(dst: &Path, mode: BackupMode, suffix: &str) -> (PathBuf, BackupMode, bool) {
    let file = dst.as_os_str().as_bytes();
    let last = last_component(file);
    let base_offset = file.len() - last.len();
    let filelen = base_offset + base_len(last);
    let mut s = file[..filelen].to_vec();
    if mode == BackupMode::Simple {
        s.extend_from_slice(suffix.as_bytes());
        return (PathBuf::from(OsStr::from_bytes(&s)), mode, true);
    }
    let dir = if base_offset == 0 {
        PathBuf::from(".")
    } else {
        PathBuf::from(OsStr::from_bytes(&file[..base_offset]))
    };
    let base = &file[base_offset..filelen];
    // The highest version so far, as digits: compared by length, then bytes,
    // so that no number is too large.
    let mut highest: Option<Vec<u8>> = None;
    for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
        let name = entry.file_name();
        let Some(digits) = name
            .as_bytes()
            .strip_prefix(base)
            .and_then(|r| r.strip_prefix(b".~"))
            .and_then(|r| r.strip_suffix(b"~"))
        else {
            continue;
        };
        if !digits.first().is_some_and(|d| (b'1'..=b'9').contains(d))
            || !digits.iter().all(u8::is_ascii_digit)
        {
            continue;
        }
        let newer = highest
            .as_ref()
            .is_none_or(|h| (h.len(), h.as_slice()) <= (digits.len(), digits));
        if newer {
            highest = Some(digits.to_vec());
        }
    }
    let (result, version) = match highest {
        None => (Numbered::New, b"1".to_vec()),
        Some(mut v) => {
            let all_9s = v.iter().all(|&d| d == b'9');
            if all_9s {
                v.insert(0, b'0');
            }
            // Add one, carrying through trailing nines.
            for d in v.iter_mut().rev() {
                if *d == b'9' {
                    *d = b'0';
                } else {
                    *d += 1;
                    break;
                }
            }
            let result = if all_9s {
                Numbered::Longer
            } else {
                Numbered::SameLength
            };
            (result, v)
        }
    };
    let mut mode = mode;
    if result == Numbered::New && mode == BackupMode::Existing {
        mode = BackupMode::Simple;
        s.extend_from_slice(suffix.as_bytes());
    } else {
        s.extend_from_slice(b".~");
        s.extend_from_slice(&version);
        s.push(b'~');
    }
    let fits = result == Numbered::SameLength || check_extension(&mut s, filelen, &dir);
    (PathBuf::from(OsStr::from_bytes(&s)), mode, fits)
}

/// gnulib `check_extension`: when the backup's last component is too long for
/// the file system, shortens it to end in `~`. False if it had to.
fn check_extension(s: &mut Vec<u8>, filelen: usize, dir: &Path) -> bool {
    const NAME_MAX_MINIMUM: usize = 14;
    let base_offset = s.len() - last_component(s).len();
    let baselen = base_len(&s[base_offset..]);
    let mut baselen_max = 255;
    if NAME_MAX_MINIMUM < baselen {
        let name_max = c_path(dir)
            // SAFETY: `c` is a NUL-terminated path that outlives the call.
            .map(|c| unsafe { libc::pathconf(c.as_ptr(), libc::_PC_NAME_MAX) })
            .unwrap_or(-1);
        baselen_max = usize::try_from(name_max)
            .ok()
            .filter(|&n| n > 0)
            .unwrap_or(255);
    }
    if baselen <= baselen_max {
        return true;
    }
    let mut keep = filelen - base_offset;
    if baselen_max <= keep {
        keep = baselen_max - 1;
    }
    s.truncate(base_offset + keep);
    s.push(b'~');
    false
}

/// A rename made by [`make_backup`].
pub(crate) struct Backup {
    /// The backup's name, spelled after the destination operand (for messages).
    pub(crate) name: PathBuf,
    /// The names by which the backup and the destination are reached
    /// afterwards (to put the backup back, or to replace the destination),
    /// as GNU reaches them: through the target directory's descriptor, so
    /// they keep naming the renamed entries when the rename moved a symlink
    /// the operand goes through (`cp -r l/l l/l`, `l` a symlink to `.`, backs
    /// up `l/l/l`, which is `l`), else by the destination's name as spelled.
    pub(crate) real: PathBuf,
    pub(crate) real_dst: PathBuf,
    /// The rename's journal entry.
    pub(crate) entry: Option<JournalEntry>,
}

/// gnulib `backup_file_rename`: renames `dst` to its backup name (never over an
/// existing numbered backup) and returns that name, with its journal entry.
/// With a journal, the rename is recorded (before it happens, see
/// [`UndoJournal::begin`]) and a backup file that is about to be replaced is
/// saved first, so `dogfish undo` can bring everything back.
///
/// `real_dst` is how `dst` is reached (see [`Backup::real`]). The journal
/// records both names with their directory resolved ([`stable_path`]), as
/// they are when renamed, so undo puts back what was renamed.
pub(crate) fn make_backup(
    dst: &Path,
    real_dst: &Path,
    mode: BackupMode,
    suffix: &str,
    journal: Option<&UndoJournal>,
    tool: &str,
) -> io::Result<Backup> {
    let in_dir = |dir: &Path, name: &Path| match (dir.parent(), name.file_name()) {
        (Some(d), Some(base)) => d.join(base),
        _ => name.to_path_buf(),
    };
    let stable_dst = stable_path(real_dst);
    loop {
        let (name, mode, fits) = backup_candidate(dst, mode, suffix);
        let backup = in_dir(real_dst, &name);
        let stable_backup = in_dir(&stable_dst, &name);
        let mut entry = match journal {
            Some(j) => Some(j.begin(
                tool,
                JournalAction::Move {
                    src: stable_dst.clone(),
                    dst: stable_backup.clone(),
                    backup_path: None,
                    copied: false,
                    existed: fs::symlink_metadata(&stable_backup).is_ok(),
                    src_inode: inode(&stable_dst),
                    src_link: None,
                },
            )?),
            None => None,
        };
        let saved = match (journal, &mut entry) {
            (Some(j), Some(e)) if mode == BackupMode::Simple => {
                j.take(e, &stable_backup, Take::Move)?
            }
            _ => None,
        };
        crash_point();
        let res = if mode == BackupMode::Simple {
            fs::rename(&stable_dst, &stable_backup)
        } else {
            rename_noreplace(&stable_dst, &stable_backup)
        };
        if let Err(e) = res {
            if let (Some(j), Some(mut entry)) = (journal, entry) {
                let back = saved
                    .as_ref()
                    .is_none_or(|s| move_path(Path::new(s), &stable_backup).is_ok());
                if back {
                    let _ = j.cancel(&mut entry);
                }
            }
            // Another process took that number: try the next one.
            if e.raw_os_error() == Some(libc::EEXIST) && fits && mode != BackupMode::Simple {
                continue;
            }
            return Err(e);
        }
        if let (Some(j), Some(e)) = (journal, &mut entry) {
            let _ = j.commit(e);
        }
        return Ok(Backup {
            name,
            real: backup,
            real_dst: real_dst.to_path_buf(),
            entry,
        });
    }
}
