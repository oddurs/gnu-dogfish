use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::io::{copy_tree, move_path};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum JournalAction {
    /// File or directory moved to Dogfish trash (`trash_path` -> original `original_path`).
    TrashRemove {
        original_path: String,
        trash_path: String,
    },
    /// File or directory moved/renamed (`src` -> `dst`). `backup_path` holds whatever
    /// `dst` contained before it was overwritten, if anything.
    Move {
        src: String,
        dst: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backup_path: Option<String>,
    },
    /// File or directory created via `cp` (`created_path`). `backup_path` holds the prior
    /// contents of `created_path` if the copy overwrote or merged into something.
    CopyCreate {
        src: String,
        created_path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backup_path: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub id: u64,
    pub timestamp_unix: u64,
    pub tool: String,
    pub action: JournalAction,
}

/// Manages `~/.dogfish/trash` and `~/.dogfish/journal.json` for `dogfish undo` and `dogfish history`.
///
/// Mutations hold an exclusive `flock` on `journal.lock` and the journal is replaced
/// atomically (write to a temp file, then rename), so concurrent `dogfish` processes
/// cannot interleave or truncate each other's entries.
pub struct UndoJournal {
    base_dir: PathBuf,
}

/// Exclusive advisory lock on the journal, released on drop.
struct JournalLock(File);

impl Drop for JournalLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn exists_no_follow(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

fn refuse_existing(p: &Path) -> io::Result<()> {
    if exists_no_follow(p) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "refusing to undo: '{}' already exists and would be overwritten",
                p.display()
            ),
        ));
    }
    Ok(())
}

impl UndoJournal {
    pub fn open() -> io::Result<Self> {
        let home = std::env::var_os("DOGFISH_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".dogfish")))
            .unwrap_or_else(|| PathBuf::from("/tmp/.dogfish"));
        fs::create_dir_all(home.join("trash"))?;
        Ok(Self { base_dir: home })
    }

    fn journal_path(&self) -> PathBuf {
        self.base_dir.join("journal.json")
    }

    pub fn trash_dir(&self) -> PathBuf {
        self.base_dir.join("trash")
    }

    fn lock(&self) -> io::Result<JournalLock> {
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.base_dir.join("journal.lock"))?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(JournalLock(f))
    }

    /// Returns a fresh, unique path inside the trash directory for `original`.
    fn trash_slot(&self, original: &Path) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let fname = original
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "item".to_string());
        self.trash_dir().join(format!(
            "{}_{}_{}_{}",
            nanos,
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            fname
        ))
    }

    pub fn load_entries(&self) -> io::Result<Vec<JournalEntry>> {
        let p = self.journal_path();
        if !p.exists() {
            return Ok(Vec::new());
        }
        let content = fs::read_to_string(&p)?;
        if content.trim().is_empty() {
            return Ok(Vec::new());
        }
        // A corrupt journal is an error, not an empty one: silently defaulting would
        // let the next `record` overwrite it and orphan everything in the trash.
        serde_json::from_str(&content).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("corrupt undo journal '{}': {}", p.display(), e),
            )
        })
    }

    fn save_entries(&self, entries: &[JournalEntry]) -> io::Result<()> {
        let json = serde_json::to_string_pretty(entries)?;
        let tmp = self
            .base_dir
            .join(format!("journal.json.tmp.{}", std::process::id()));
        fs::write(&tmp, json)?;
        fs::rename(&tmp, self.journal_path())
    }

    pub fn record(&self, tool: &str, action: JournalAction) -> io::Result<JournalEntry> {
        let _lock = self.lock()?;
        let mut entries = self.load_entries()?;
        let next_id = entries.last().map(|e| e.id + 1).unwrap_or(1);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let entry = JournalEntry {
            id: next_id,
            timestamp_unix: now,
            tool: tool.to_string(),
            action,
        };
        entries.push(entry.clone());
        // Keep last 500 operations
        if entries.len() > 500 {
            let drain_count = entries.len() - 500;
            entries.drain(0..drain_count);
        }
        self.save_entries(&entries)?;
        Ok(entry)
    }

    /// Preserves whatever currently lives at `target` (if anything) in the trash so an
    /// overwriting `cp`/`mv` can be undone. With `by_copy`, `target` is left in place
    /// (needed when the caller writes into it, e.g. `cp -r` merging into a directory).
    pub fn backup_existing(&self, target: &Path, by_copy: bool) -> io::Result<Option<String>> {
        if !exists_no_follow(target) {
            return Ok(None);
        }
        let slot = self.trash_slot(target);
        if by_copy {
            copy_tree(target, &slot)?;
        } else {
            move_path(target, &slot)?;
        }
        Ok(Some(slot.to_string_lossy().into_owned()))
    }

    /// Stages a file or directory into `~/.dogfish/trash` and records a `TrashRemove` entry.
    pub fn stage_to_trash(&self, target: &Path) -> io::Result<JournalEntry> {
        // `absolute`, not `canonicalize`: trashing a symlink must restore the link
        // itself, not something at the location it points to.
        let abs = std::path::absolute(target)?;
        let trash_target = self.trash_slot(target);
        move_path(target, &trash_target)?;

        self.record(
            "rm",
            JournalAction::TrashRemove {
                original_path: abs.to_string_lossy().into_owned(),
                trash_path: trash_target.to_string_lossy().into_owned(),
            },
        )
    }

    /// Pops and reverses the most recent journal entry.
    ///
    /// Never overwrites: if something now occupies a path the undo would restore to,
    /// the entry is left in the journal and an error is returned. Paths an undo
    /// removes (copies, overwritten targets) go to the trash rather than being deleted.
    pub fn undo_last(&self) -> io::Result<Option<(JournalEntry, String)>> {
        let _lock = self.lock()?;
        let mut entries = self.load_entries()?;
        let Some(entry) = entries.pop() else {
            return Ok(None);
        };

        let summary = match &entry.action {
            JournalAction::TrashRemove {
                original_path,
                trash_path,
            } => {
                let orig = Path::new(original_path);
                refuse_existing(orig)?;
                if let Some(parent) = orig.parent() {
                    fs::create_dir_all(parent)?;
                }
                move_path(Path::new(trash_path), orig)?;
                format!("Restored '{}' from trash", original_path)
            }
            JournalAction::Move {
                src,
                dst,
                backup_path,
            } => {
                let src_p = Path::new(src);
                refuse_existing(src_p)?;
                move_path(Path::new(dst), src_p)?;
                if let Some(backup) = backup_path {
                    move_path(Path::new(backup), Path::new(dst))?;
                    format!(
                        "Moved '{}' back to '{}' and restored the original",
                        dst, src
                    )
                } else {
                    format!("Moved '{}' back to '{}'", dst, src)
                }
            }
            JournalAction::CopyCreate {
                src: _,
                created_path,
                backup_path,
            } => {
                let p = Path::new(created_path);
                if exists_no_follow(p) {
                    move_path(p, &self.trash_slot(p))?;
                }
                if let Some(backup) = backup_path {
                    move_path(Path::new(backup), p)?;
                    format!("Restored '{}' to its state before the copy", created_path)
                } else {
                    format!("Removed copied target '{}'", created_path)
                }
            }
        };

        self.save_entries(&entries)?;
        Ok(Some((entry, summary)))
    }
}
