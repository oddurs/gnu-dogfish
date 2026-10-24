use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum JournalAction {
    /// File or directory moved to Dogfish trash (`trash_path` -> original `original_path`).
    TrashRemove {
        original_path: String,
        trash_path: String,
    },
    /// File or directory moved/renamed (`src` -> `dst`).
    Move { src: String, dst: String },
    /// File or directory created via `cp` or `mkdir` (`created_path`).
    CopyCreate {
        src: String,
        created_path: String,
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
pub struct UndoJournal {
    base_dir: PathBuf,
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

    pub fn load_entries(&self) -> io::Result<Vec<JournalEntry>> {
        let p = self.journal_path();
        if !p.exists() {
            return Ok(Vec::new());
        }
        let content = fs::read_to_string(p)?;
        if content.trim().is_empty() {
            return Ok(Vec::new());
        }
        let entries: Vec<JournalEntry> = serde_json::from_str(&content).unwrap_or_default();
        Ok(entries)
    }

    fn save_entries(&self, entries: &[JournalEntry]) -> io::Result<()> {
        let json = serde_json::to_string_pretty(entries)?;
        fs::write(self.journal_path(), json)
    }

    pub fn record(&self, tool: &str, action: JournalAction) -> io::Result<JournalEntry> {
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

    /// Stages a file or directory into `~/.dogfish/trash` and records a `TrashRemove` entry.
    pub fn stage_to_trash(&self, target: &Path) -> io::Result<JournalEntry> {
        let abs = fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let fname = target
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "item".to_string());
        let trash_target = self.trash_dir().join(format!("{}_{}", nanos, fname));

        if fs::rename(target, &trash_target).is_err() {
            // Cross-device fallback
            if target.is_dir() {
                copy_dir_recursive(target, &trash_target)?;
                fs::remove_dir_all(target)?;
            } else {
                fs::copy(target, &trash_target)?;
                fs::remove_file(target)?;
            }
        }

        self.record(
            "rm",
            JournalAction::TrashRemove {
                original_path: abs.to_string_lossy().into_owned(),
                trash_path: trash_target.to_string_lossy().into_owned(),
            },
        )
    }

    /// Pops and reverses the most recent journal entry.
    pub fn undo_last(&self) -> io::Result<Option<(JournalEntry, String)>> {
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
                if let Some(parent) = orig.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(trash_path, orig)?;
                format!("Restored '{}' from trash", original_path)
            }
            JournalAction::Move { src, dst } => {
                fs::rename(dst, src)?;
                format!("Moved '{}' back to '{}'", dst, src)
            }
            JournalAction::CopyCreate { src: _, created_path } => {
                let p = Path::new(created_path);
                if p.is_dir() {
                    fs::remove_dir_all(p)?;
                } else if p.exists() {
                    fs::remove_file(p)?;
                }
                format!("Removed copied target '{}'", created_path)
            }
        };

        self.save_entries(&entries)?;
        Ok(Some((entry, summary)))
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dest_path)?;
        } else {
            fs::copy(entry.path(), dest_path)?;
        }
    }
    Ok(())
}
