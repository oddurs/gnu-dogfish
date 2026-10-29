use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::format::parse_size_str;
use crate::io::{copy_tree, move_path};

/// Number of operations kept for `dogfish undo` (override with `DOGFISH_JOURNAL_MAX`).
pub const DEFAULT_MAX_ENTRIES: usize = 500;

/// Total size of the automatic backups `cp`/`mv` keep of files they overwrite
/// (override with `DOGFISH_BACKUP_LIMIT`, e.g. `4G`). Files staged with `rm --safe`
/// never count against it.
pub const DEFAULT_BACKUP_LIMIT: u64 = 1 << 30;

/// Trash items no journal entry refers to (undone copies, leftovers from crashes or
/// from 0.1.0, which never cleaned up) are deleted this long after entering the trash.
const ORPHAN_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// Paths in the journal, losslessly: a JSON string when the path is UTF-8
/// (as every journal before non-UTF-8 support wrote them), otherwise
/// `{"path_bytes": "<base64>"}`.
mod json_path {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    #[serde(untagged)]
    enum Repr<'a> {
        Text(std::borrow::Cow<'a, str>),
        Bytes { path_bytes: String },
    }

    fn repr(p: &Path) -> Repr<'_> {
        match p.to_str() {
            Some(t) => Repr::Text(t.into()),
            None => Repr::Bytes {
                path_bytes: STANDARD.encode(p.as_os_str().as_bytes()),
            },
        }
    }

    fn from_repr<E: serde::de::Error>(r: Repr<'_>) -> Result<PathBuf, E> {
        match r {
            Repr::Text(t) => Ok(PathBuf::from(t.into_owned())),
            Repr::Bytes { path_bytes } => STANDARD
                .decode(path_bytes)
                .map(|b| PathBuf::from(std::ffi::OsString::from_vec(b)))
                .map_err(E::custom),
        }
    }

    pub(super) fn serialize<S: Serializer>(p: &Path, s: S) -> Result<S::Ok, S::Error> {
        repr(p).serialize(s)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PathBuf, D::Error> {
        from_repr(Repr::deserialize(d).map_err(D::Error::custom)?)
    }

    pub(super) mod option {
        use super::*;

        pub(crate) fn serialize<S: Serializer>(
            p: &Option<PathBuf>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            p.as_deref().map(repr).serialize(s)
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> Result<Option<PathBuf>, D::Error> {
            Option::<Repr<'_>>::deserialize(d)?
                .map(from_repr)
                .transpose()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum JournalAction {
    /// File or directory moved to Dogfish trash (`trash_path` -> original `original_path`).
    TrashRemove {
        #[serde(with = "json_path")]
        original_path: PathBuf,
        #[serde(with = "json_path")]
        trash_path: PathBuf,
    },
    /// File or directory moved/renamed (`src` -> `dst`). `backup_path` holds whatever
    /// `dst` contained before it was overwritten, if anything.
    Move {
        #[serde(with = "json_path")]
        src: PathBuf,
        #[serde(with = "json_path")]
        dst: PathBuf,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "json_path::option"
        )]
        backup_path: Option<PathBuf>,
    },
    /// File or directory created via `cp` (`created_path`). `backup_path` holds the prior
    /// contents of `created_path` if the copy overwrote or merged into something.
    CopyCreate {
        #[serde(with = "json_path")]
        src: PathBuf,
        #[serde(with = "json_path")]
        created_path: PathBuf,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "json_path::option"
        )]
        backup_path: Option<PathBuf>,
    },
}

impl JournalAction {
    fn backup_path(&self) -> Option<&Path> {
        match self {
            Self::TrashRemove { .. } => None,
            Self::Move { backup_path, .. } | Self::CopyCreate { backup_path, .. } => {
                backup_path.as_deref()
            }
        }
    }

    /// Everything this entry keeps in the trash, deleted once the entry is dropped.
    fn payload_path(&self) -> Option<&Path> {
        match self {
            Self::TrashRemove { trash_path, .. } => Some(trash_path),
            _ => self.backup_path(),
        }
    }

    fn drop_backup(&mut self) -> Option<PathBuf> {
        match self {
            Self::TrashRemove { .. } => None,
            Self::Move { backup_path, .. } | Self::CopyCreate { backup_path, .. } => {
                backup_path.take()
            }
        }
    }
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub id: u64,
    pub timestamp_unix: u64,
    pub tool: String,
    pub action: JournalAction,
    /// Size of the overwrite backup held for this entry, in bytes.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub backup_bytes: u64,
    /// The overwrite backup was discarded to stay under the backup limit, so undo
    /// can no longer restore what this operation replaced.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub backup_purged: bool,
}

/// Manages `~/.dogfish/trash` and `~/.dogfish/journal.ndjson` for `dogfish undo` and
/// `dogfish history`.
///
/// The journal is append-only NDJSON, so recording an operation costs one small write
/// regardless of history length. Mutations hold an exclusive `flock` on `journal.lock`;
/// rewrites (undo, retention) replace the file atomically via a temp file and rename.
///
/// Retention keeps the newest [`DEFAULT_MAX_ENTRIES`] operations (compacting once the
/// journal exceeds that by a small slack); the trash payloads of
/// older ones are deleted with them. Overwrite backups are additionally capped at
/// [`DEFAULT_BACKUP_LIMIT`] bytes in total, oldest discarded first (the newest backup is
/// always kept).
pub struct UndoJournal {
    base_dir: PathBuf,
    max_entries: usize,
    backup_limit: u64,
}

/// Exclusive advisory lock on the journal, released on drop.
struct JournalLock(File);

impl Drop for JournalLock {
    fn drop(&mut self) {
        // SAFETY: flock takes an open descriptor (owned by self.0) and no pointers.
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

fn remove_any(p: &Path) -> io::Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(p),
        Ok(_) => fs::remove_file(p),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Apparent size of a file or tree, not following symlinks.
fn path_size(p: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(p) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    fs::read_dir(p)
        .map(|rd| rd.flatten().map(|e| path_size(&e.path())).sum())
        .unwrap_or(0)
}

fn corrupt(path: &Path, line: usize, err: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "corrupt undo journal '{}' (line {}): {}",
            path.display(),
            line,
            err
        ),
    )
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl UndoJournal {
    pub fn open() -> io::Result<Self> {
        let home = std::env::var_os("DOGFISH_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".dogfish")))
            .unwrap_or_else(|| PathBuf::from("/tmp/.dogfish"));
        fs::create_dir_all(home.join("trash"))?;
        let max_entries = std::env::var("DOGFISH_JOURNAL_MAX")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_ENTRIES)
            .max(1);
        let backup_limit = std::env::var("DOGFISH_BACKUP_LIMIT")
            .ok()
            .and_then(|v| parse_size_str(&v).ok())
            .unwrap_or(DEFAULT_BACKUP_LIMIT);
        let journal = Self {
            base_dir: home,
            max_entries,
            backup_limit,
        };
        journal.migrate_legacy()?;
        Ok(journal)
    }

    /// The journal `cp` and `mv` record into, or `None` when recording is off
    /// (`DOGFISH_UNDO=off`, `0`, `false` or `no`) or the journal can't be opened.
    ///
    /// Turning it off skips the backups `cp`/`mv` keep of what they overwrite, so
    /// overwrites cost what GNU's do — and can't be undone. `rm --safe` is an explicit
    /// request for the trash and keeps using [`UndoJournal::open`].
    pub fn open_for_recording() -> Option<Self> {
        let disabled = std::env::var("DOGFISH_UNDO").is_ok_and(|v| {
            matches!(
                v.to_ascii_lowercase().as_str(),
                "0" | "off" | "false" | "no"
            )
        });
        if disabled { None } else { Self::open().ok() }
    }

    fn journal_path(&self) -> PathBuf {
        self.base_dir.join("journal.ndjson")
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
        // SAFETY: flock takes an open descriptor (owned by `f`) and no pointers.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(JournalLock(f))
    }

    /// Converts a 0.1.0 `journal.json` (a JSON array) to NDJSON, keeping the original
    /// as `journal.json.migrated`.
    fn migrate_legacy(&self) -> io::Result<()> {
        let legacy = self.base_dir.join("journal.json");
        if !legacy.exists() {
            return Ok(());
        }
        let _lock = self.lock()?;
        if !legacy.exists() || self.journal_path().exists() {
            return Ok(());
        }
        let content = fs::read_to_string(&legacy)?;
        let entries: Vec<JournalEntry> = if content.trim().is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&content).map_err(|e| corrupt(&legacy, 1, e))?
        };
        self.save_entries(&entries)?;
        fs::rename(&legacy, self.base_dir.join("journal.json.migrated"))
    }

    /// Returns a fresh, unique path inside the trash directory for `original`.
    fn trash_slot(&self, original: &Path) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut slot = std::ffi::OsString::from(format!(
            "{}_{}_{}_",
            nanos,
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        slot.push(original.file_name().unwrap_or("item".as_ref()));
        self.trash_dir().join(slot)
    }

    fn read_journal(&self) -> io::Result<String> {
        match fs::read_to_string(self.journal_path()) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            res => res,
        }
    }

    /// Complete lines of the journal. Every record is written as one `line\n`, so a
    /// trailing fragment without a newline is an append cut short by a crash; it never
    /// became a valid entry and is ignored.
    fn complete_lines(content: &str) -> impl Iterator<Item = (usize, &str)> {
        let complete = match content.rfind('\n') {
            Some(i) => &content[..=i],
            None => "",
        };
        complete
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
            .map(|(i, l)| (i + 1, l))
    }

    fn parse_entries(&self, content: &str) -> io::Result<Vec<JournalEntry>> {
        // A corrupt journal is an error, not an empty one: silently defaulting would
        // let the next rewrite discard it and orphan everything in the trash.
        Self::complete_lines(content)
            .map(|(n, line)| {
                serde_json::from_str(line).map_err(|e| corrupt(&self.journal_path(), n, e))
            })
            .collect()
    }

    pub fn load_entries(&self) -> io::Result<Vec<JournalEntry>> {
        self.parse_entries(&self.read_journal()?)
    }

    fn save_entries(&self, entries: &[JournalEntry]) -> io::Result<()> {
        let mut buf = Vec::new();
        for e in entries {
            serde_json::to_writer(&mut buf, e)?;
            buf.push(b'\n');
        }
        let tmp = self
            .base_dir
            .join(format!("journal.ndjson.tmp.{}", std::process::id()));
        fs::write(&tmp, buf)?;
        fs::rename(&tmp, self.journal_path())
    }

    pub fn record(&self, tool: &str, action: JournalAction) -> io::Result<JournalEntry> {
        let _lock = self.lock()?;
        let content = self.read_journal()?;
        let last_id = match Self::complete_lines(&content).last() {
            Some((n, line)) => {
                serde_json::from_str::<JournalEntry>(line)
                    .map_err(|e| corrupt(&self.journal_path(), n, e))?
                    .id
            }
            None => 0,
        };
        let entry = JournalEntry {
            id: last_id + 1,
            timestamp_unix: now_unix(),
            tool: tool.to_string(),
            backup_bytes: action.backup_path().map_or(0, path_size),
            backup_purged: false,
            action,
        };

        let mut line = serde_json::to_vec(&entry)?;
        line.push(b'\n');
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.journal_path())?;
        if !content.is_empty() && !content.ends_with('\n') {
            // Drop a fragment left by a crashed append before adding a new line.
            let keep = content.rfind('\n').map_or(0, |i| i + 1);
            f.set_len(keep as u64)?;
        }
        f.write_all(&line)?;

        // Retention needs a full parse, so run it only when a limit can be exceeded:
        // past the entry cap, after adding a backup, or periodically to sweep orphans.
        // Over the cap, allow some slack before compacting back to it, so a full journal
        // is rewritten once every few dozen records instead of on every one.
        let entry_count = Self::complete_lines(&content).count() + 1;
        let slack = (self.max_entries / 16).max(1);
        if entry_count > self.max_entries + slack
            || entry.backup_bytes > 0
            || entry.id.is_multiple_of(64)
        {
            self.enforce_retention()?;
        }
        Ok(entry)
    }

    /// Applies the entry-count and backup-size limits, deleting the trash payloads of
    /// whatever no longer fits. Caller must hold the lock.
    fn enforce_retention(&self) -> io::Result<()> {
        let mut entries = self.load_entries()?;
        let mut changed = false;

        if entries.len() > self.max_entries {
            let excess = entries.len() - self.max_entries;
            for old in entries.drain(..excess) {
                if let Some(p) = old.action.payload_path() {
                    remove_any(p)?;
                }
            }
            changed = true;
        }

        let mut total: u64 = entries
            .iter()
            .filter(|e| e.action.backup_path().is_some())
            .map(|e| e.backup_bytes)
            .sum();
        let newest = entries.len().saturating_sub(1);
        for e in entries[..newest].iter_mut() {
            if total <= self.backup_limit {
                break;
            }
            if let Some(p) = e.action.drop_backup() {
                remove_any(&p)?;
                total -= e.backup_bytes;
                e.backup_purged = true;
                changed = true;
            }
        }

        if changed {
            self.save_entries(&entries)?;
        }
        self.sweep_orphans(&entries, now_unix() as i64 - ORPHAN_TTL_SECS)
    }

    /// Deletes trash items that no entry references and that entered the trash before
    /// `cutoff` (Unix seconds; normally now minus [`ORPHAN_TTL_SECS`]). Age is the inode change time, which moving an item into the
    /// trash updates (unlike its modification time).
    fn sweep_orphans(&self, entries: &[JournalEntry], cutoff: i64) -> io::Result<()> {
        use std::collections::HashSet;
        use std::os::unix::fs::MetadataExt;

        let referenced: HashSet<PathBuf> = entries
            .iter()
            .filter_map(|e| e.action.payload_path().map(Path::to_path_buf))
            .collect();
        for item in fs::read_dir(self.trash_dir())?.flatten() {
            let path = item.path();
            if referenced.contains(&path) {
                continue;
            }
            if item.metadata().is_ok_and(|m| m.ctime() < cutoff) {
                remove_any(&path)?;
            }
        }
        Ok(())
    }

    /// Preserves whatever currently lives at `target` (if anything) in the trash so an
    /// overwriting `cp`/`mv` can be undone. With `by_copy`, `target` is left in place
    /// (needed when the caller writes into it, e.g. `cp -r` merging into a directory).
    pub fn backup_existing(&self, target: &Path, by_copy: bool) -> io::Result<Option<PathBuf>> {
        if !exists_no_follow(target) {
            return Ok(None);
        }
        let slot = self.trash_slot(target);
        if by_copy {
            copy_tree(target, &slot)?;
        } else {
            move_path(target, &slot)?;
        }
        Ok(Some(slot))
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
                original_path: abs,
                trash_path: trash_target,
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
                let orig = original_path.as_path();
                refuse_existing(orig)?;
                if let Some(parent) = orig.parent() {
                    fs::create_dir_all(parent)?;
                }
                move_path(trash_path, orig)?;
                format!("Restored '{}' from trash", original_path.display())
            }
            JournalAction::Move {
                src,
                dst,
                backup_path,
            } => {
                refuse_existing(src)?;
                move_path(dst, src)?;
                if let Some(backup) = backup_path {
                    move_path(backup, dst)?;
                }
                let (src, dst) = (src.display(), dst.display());
                match backup_path {
                    Some(_) => {
                        format!(
                            "Moved '{}' back to '{}' and restored the original",
                            dst, src
                        )
                    }
                    None if entry.backup_purged => format!(
                        "Moved '{}' back to '{}'; the file it replaced was discarded \
                         (backup limit) and cannot be restored",
                        dst, src
                    ),
                    None => format!("Moved '{}' back to '{}'", dst, src),
                }
            }
            JournalAction::CopyCreate {
                src: _,
                created_path,
                backup_path,
            } => {
                let p = created_path.as_path();
                let created_path = created_path.display();
                if entry.backup_purged {
                    // The copy is now the only version left; removing it would lose data.
                    format!(
                        "Left '{}' as is: the contents it replaced were discarded \
                         (backup limit) and cannot be restored",
                        created_path
                    )
                } else {
                    // Into the trash rather than deleted, in case it was edited since the
                    // copy; unreferenced, it is swept after ORPHAN_TTL_SECS.
                    if exists_no_follow(p) {
                        move_path(p, &self.trash_slot(p))?;
                    }
                    match backup_path {
                        Some(backup) => {
                            move_path(backup, p)?;
                            format!("Restored '{}' to its state before the copy", created_path)
                        }
                        None => format!("Removed copied target '{}'", created_path),
                    }
                }
            }
        };

        self.save_entries(&entries)?;
        Ok(Some((entry, summary)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal_in(dir: &Path) -> UndoJournal {
        fs::create_dir_all(dir.join("trash")).unwrap();
        UndoJournal {
            base_dir: dir.to_path_buf(),
            max_entries: DEFAULT_MAX_ENTRIES,
            backup_limit: DEFAULT_BACKUP_LIMIT,
        }
    }

    #[test]
    fn sweep_removes_only_unreferenced_items_older_than_cutoff() {
        let tmp = tempfile::tempdir().unwrap();
        let j = journal_in(tmp.path());
        let kept = j.trash_dir().join("referenced");
        let orphan = j.trash_dir().join("orphan");
        fs::write(&kept, "k").unwrap();
        fs::create_dir(&orphan).unwrap();
        fs::write(orphan.join("inner"), "o").unwrap();
        let entries = vec![JournalEntry {
            id: 1,
            timestamp_unix: 0,
            tool: "rm".into(),
            action: JournalAction::TrashRemove {
                original_path: "/x".into(),
                trash_path: kept.clone(),
            },
            backup_bytes: 0,
            backup_purged: false,
        }];

        // Too young: nothing is touched.
        j.sweep_orphans(&entries, 0).unwrap();
        assert!(orphan.exists() && kept.exists());

        // Old enough: the orphan goes, the referenced payload stays.
        j.sweep_orphans(&entries, now_unix() as i64 + 1).unwrap();
        assert!(!orphan.exists());
        assert!(kept.exists());
    }

    #[test]
    fn paths_round_trip_losslessly() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let odd = PathBuf::from(OsStr::from_bytes(b"/tmp/caf\xe9"));
        let action = JournalAction::Move {
            src: odd.clone(),
            dst: "/tmp/plain".into(),
            backup_path: Some(PathBuf::from(OsStr::from_bytes(b"/tmp/b\xffk"))),
        };
        let json = serde_json::to_string(&action).unwrap();
        // UTF-8 paths stay plain strings; others are base64 bytes.
        assert!(json.contains(r#""dst":"/tmp/plain""#), "{json}");
        assert!(
            json.contains(r#""src":{"path_bytes":"L3RtcC9jYWbp"}"#),
            "{json}"
        );
        let back: JournalAction = serde_json::from_str(&json).unwrap();
        let JournalAction::Move {
            src,
            dst,
            backup_path,
        } = back
        else {
            panic!("{json}");
        };
        assert_eq!(src, odd);
        assert_eq!(dst, Path::new("/tmp/plain"));
        assert!(backup_path.is_some());
    }

    #[test]
    fn journals_written_with_string_paths_still_read() {
        let line = r#"{"id":3,"timestamp_unix":1,"tool":"cp","action":{"action":"copy_create","src":"/a","created_path":"/b"}}"#;
        let e: JournalEntry = serde_json::from_str(line).unwrap();
        let JournalAction::CopyCreate {
            src,
            created_path,
            backup_path,
        } = e.action
        else {
            panic!("{line}");
        };
        assert_eq!(
            (src.as_path(), created_path.as_path()),
            (Path::new("/a"), Path::new("/b"))
        );
        assert_eq!(backup_path, None);
    }
}
