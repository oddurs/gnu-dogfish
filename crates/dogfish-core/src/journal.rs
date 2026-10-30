use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::crash::crash_point;
use crate::format::parse_size_str;
use crate::io::{copy_tree, copy_tree_linked, move_path, remove_any};

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
        /// A move across file systems copied `src` completely to `dst`; what
        /// is still at `src` is a remnant of removing it.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        copied: bool,
        /// `dst` existed before the operation (recorded while it is pending, so
        /// recovery can tell an untouched original from a partial copy).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        existed: bool,
        /// Device and inode of `src` when the move began: while it is pending,
        /// the evidence that a file at `dst` is `src` renamed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        src_inode: Option<(u64, u64)>,
        /// A move across file systems of a non-directory with other hard
        /// links: a hard link to `src`'s inode, kept in the trash on `src`'s
        /// file system (named here before it is made, and before `src` is
        /// unlinked). Undo renames it back to `src`, so the other links share
        /// the file again, instead of copying `dst` back as a new inode.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "json_path::option"
        )]
        src_link: Option<PathBuf>,
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
        /// The copy wrote into `created_path` (a file or a merged directory)
        /// instead of replacing it: undo writes the backup back into it, so
        /// its inodes, and the hard links to them, stay as they were.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        in_place: bool,
        /// `created_path` existed before the operation (recorded while it is
        /// pending, so recovery leaves an unsaved original alone).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        existed: bool,
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
    fn payload_paths(&self) -> impl Iterator<Item = &Path> {
        let (main, link) = match self {
            Self::TrashRemove { trash_path, .. } => (Some(trash_path.as_path()), None),
            Self::Move { src_link, .. } => (self.backup_path(), src_link.as_deref()),
            Self::CopyCreate { .. } => (self.backup_path(), None),
        };
        main.into_iter().chain(link)
    }

    fn drop_backup(&mut self) -> Option<PathBuf> {
        match self {
            Self::TrashRemove { .. } => None,
            Self::Move { backup_path, .. } | Self::CopyCreate { backup_path, .. } => {
                backup_path.take()
            }
        }
    }

    /// Sets where the overwritten destination is saved (no-op for `TrashRemove`).
    pub fn set_backup(&mut self, path: Option<PathBuf>) {
        match self {
            Self::TrashRemove { .. } => {}
            Self::Move { backup_path, .. } | Self::CopyCreate { backup_path, .. } => {
                *backup_path = path;
            }
        }
    }
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

fn is_zero_u32(n: &u32) -> bool {
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
    /// Recorded before the operation touched anything and not yet completed:
    /// the operation is running, or it was interrupted (undo then recovers
    /// from whatever state it left).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
    /// The process running a pending operation.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub pid: u32,
    /// The operation recorded nothing to undo after all; the entry is ignored.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
}

/// What [`UndoJournal::undo_last`] did to one entry: a sentence for people,
/// and the paths it changed (empty when there was nothing left to undo).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Undone {
    pub summary: String,
    /// Paths given back what they held before the operation.
    pub restored: Vec<PathBuf>,
    /// Paths the operation had made, gone now (into the trash, or moved back).
    pub removed: Vec<PathBuf>,
}

impl From<String> for Undone {
    fn from(summary: String) -> Self {
        Self {
            summary,
            ..Self::default()
        }
    }
}

impl Undone {
    fn restored(mut self, p: &Path) -> Self {
        self.restored.push(p.to_path_buf());
        self
    }

    fn removed(mut self, p: &Path) -> Self {
        self.removed.push(p.to_path_buf());
        self
    }
}

/// Manages `~/.dogfish/trash` and `~/.dogfish/journal.ndjson` for `dogfish undo` and
/// `dogfish history`.
///
/// The journal is append-only NDJSON, so recording an operation costs one small write
/// regardless of history length. Mutations hold an exclusive `flock` on `journal.lock`;
/// rewrites (undo, retention) replace the file atomically via a temp file and rename.
///
/// # Write-ahead protocol
///
/// `cp`, `mv` and `rm --safe` record an operation as *pending* ([`UndoJournal::begin`])
/// before it changes anything, name the trash slot an overwritten destination will
/// go to ([`UndoJournal::take`]) before moving it there, and complete the entry
/// ([`UndoJournal::commit`] or [`UndoJournal::cancel`]) at the end. A later line with the
/// same id replaces an earlier one. So whatever is in the trash is referenced by an
/// entry from the moment it gets there, and when a process is killed part-way, undo
/// of its pending entry works out from what exists which step it reached:
/// [`move_path`] makes a destination appear only once complete, and a trash slot is
/// filled by one rename or by a copy that is renamed into place.
///
/// Retention keeps the newest [`DEFAULT_MAX_ENTRIES`] operations (compacting once the
/// journal exceeds that by a small slack); the trash payloads of
/// older ones are deleted with them. Overwrite backups are additionally capped at
/// [`DEFAULT_BACKUP_LIMIT`] bytes in total, oldest discarded first (the newest backup is
/// always kept, and so are those of pending operations).
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

fn refuse_existing(p: &Path) -> io::Error {
    let quoted = format!("'{}'", p.display());
    // The error record of `undo -J` names the path.
    crate::ndjson::note_quoted(p.as_os_str().as_encoded_bytes(), quoted.as_bytes());
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("refusing to undo: {quoted} already exists and would be overwritten"),
    )
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

/// Whether `a` and `b` are equal trees: the same file types, permission bits,
/// file contents and symlink targets (not following symlinks).
fn same_tree(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let (Ok(ma), Ok(mb)) = (fs::symlink_metadata(a), fs::symlink_metadata(b)) else {
        return false;
    };
    let (ta, tb) = (ma.file_type(), mb.file_type());
    if ma.permissions().mode() != mb.permissions().mode() {
        return false;
    }
    if ta.is_symlink() && tb.is_symlink() {
        return fs::read_link(a)
            .ok()
            .is_some_and(|l| fs::read_link(b).ok() == Some(l));
    }
    if ta.is_file() && tb.is_file() {
        return ma.len() == mb.len()
            && fs::read(a)
                .ok()
                .is_some_and(|d| fs::read(b).ok() == Some(d));
    }
    if ta.is_dir() && tb.is_dir() {
        let names = |p: &Path| -> Option<Vec<std::ffi::OsString>> {
            let mut v: Vec<_> = fs::read_dir(p)
                .ok()?
                .map(|e| e.map(|e| e.file_name()))
                .collect::<io::Result<_>>()
                .ok()?;
            v.sort();
            Some(v)
        };
        let (Some(na), Some(nb)) = (names(a), names(b)) else {
            return false;
        };
        return na == nb && na.iter().all(|n| same_tree(&a.join(n), &b.join(n)));
    }
    // FIFOs, sockets, devices: the same kind of node is the same.
    ta == tb
}

/// Whether `a` and `b` are on different file systems: only then can an
/// interrupted move leave a complete copy of one beside the other.
fn on_other_fs(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
        (Ok(ma), Ok(mb)) => ma.dev() != mb.dev(),
        _ => false,
    }
}

/// Device and inode of `p`, not following symlinks.
pub fn inode(p: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(p).ok().map(|m| (m.dev(), m.ino()))
}

fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: kill with signal 0 only checks the process exists; no pointers.
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
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

/// The id of a journal line, read from its `{"id":N,` prefix (every line is
/// written with `id` first); `None` when the line does not start that way.
fn line_id(line: &str) -> Option<u64> {
    let rest = line.strip_prefix("{\"id\":")?;
    let end = rest.find(|c: char| !c.is_ascii_digit())?;
    rest[..end].parse().ok()
}

/// How [`UndoJournal::take`] saves a target in the trash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    /// Moved there: the caller removes or replaces the target anyway.
    Move,
    /// Copied there, the target left in place: the caller writes into it
    /// (e.g. `cp -r` merging into a directory).
    Copy,
    /// Hard-linked there (copied when it can't be), the target left in place:
    /// the caller replaces it with one `rename`, so the name never goes missing
    /// in between, as with GNU `mv`.
    Link,
}

/// Saves `src` as the new trash slot `slot` without taking it away: a hard link
/// (same inode, so its other links and timestamps stay as they are), or a copy
/// for a directory or where linking is refused.
fn link_into_slot(src: &Path, slot: &Path) -> io::Result<()> {
    let is_dir = fs::symlink_metadata(src)?.is_dir();
    if !is_dir {
        crash_point();
        if fs::hard_link(src, slot).is_ok() {
            return Ok(());
        }
    }
    copy_into_slot(src, slot)
}

/// Copies `src` into the new trash slot `slot`, which appears only once complete.
fn copy_into_slot(src: &Path, slot: &Path) -> io::Result<()> {
    let mut part = slot.as_os_str().to_os_string();
    part.push(".partial");
    let part = PathBuf::from(part);
    if let Err(e) = copy_tree(src, &part) {
        let _ = remove_any(&part);
        return Err(e);
    }
    crash_point();
    fs::rename(&part, slot)
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
        self.trash_dir_for(original).join(slot)
    }

    /// The trash for `original`: the one in `DOGFISH_HOME` when `original` is on
    /// the same file system, else `.dogfish-trash-<uid>-<home hash>` at the top of
    /// `original`'s file system, as the freedesktop trash does with
    /// `$topdir/.Trash-$uid`. Trashing is then one rename, which keeps inodes,
    /// hard links and timestamps and costs nothing for large trees, instead of a
    /// copy. Falls back to the home trash when that directory can't be made, or is
    /// not a private directory of ours (someone else could have planted it).
    fn trash_dir_for(&self, original: &Path) -> PathBuf {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        let home_trash = self.trash_dir();
        let parent = std::path::absolute(original)
            .ok()
            .and_then(|a| a.parent().map(Path::to_path_buf))
            .and_then(|p| fs::canonicalize(p).ok());
        let (Some(parent), Ok(home)) = (parent, fs::metadata(&home_trash)) else {
            return home_trash;
        };
        let Ok(dev) = fs::metadata(&parent).map(|m| m.dev()) else {
            return home_trash;
        };
        if dev == home.dev() {
            return home_trash;
        }
        let mut top = parent.as_path();
        while let Some(up) = top.parent() {
            if fs::metadata(up).map(|m| m.dev()).ok() != Some(dev) {
                break;
            }
            top = up;
        }
        // SAFETY: geteuid has no preconditions and cannot fail.
        let uid = unsafe { libc::geteuid() };
        let dir = top.join(format!(".dogfish-trash-{uid}-{:08x}", self.home_hash()));
        let usable = |m: &fs::Metadata| {
            m.file_type().is_dir() && m.uid() == uid && m.mode() & 0o077 == 0 && m.dev() == dev
        };
        match fs::symlink_metadata(&dir) {
            Ok(m) if usable(&m) => dir,
            Ok(_) => home_trash,
            Err(_) => {
                crash_point();
                if fs::DirBuilder::new().mode(0o700).create(&dir).is_err()
                    || !fs::symlink_metadata(&dir).is_ok_and(|m| usable(&m))
                {
                    return home_trash;
                }
                self.register_trash_dir(&dir);
                dir
            }
        }
    }

    /// FNV-1a of the `DOGFISH_HOME` path: separate homes keep separate trash
    /// directories on other file systems.
    fn home_hash(&self) -> u32 {
        self.base_dir
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .fold(0x811c_9dc5_u32, |h, &b| {
                (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
            })
    }

    /// Remembers a trash directory on another file system, for the orphan sweep.
    fn register_trash_dir(&self, dir: &Path) {
        use std::os::unix::ffi::OsStrExt;
        if self.trash_dirs().iter().any(|d| d == dir) {
            return;
        }
        let mut line = dir.as_os_str().as_bytes().to_vec();
        line.push(b'\n');
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.base_dir.join("trash-dirs"))
        {
            let _ = f.write_all(&line);
        }
    }

    /// Every trash directory: the home one, then those on other file systems.
    pub fn trash_dirs(&self) -> Vec<PathBuf> {
        use std::os::unix::ffi::OsStrExt;
        let mut dirs = vec![self.trash_dir()];
        if let Ok(list) = fs::read(self.base_dir.join("trash-dirs")) {
            dirs.extend(
                list.split(|&b| b == b'\n')
                    .filter(|l| !l.is_empty())
                    .map(|l| PathBuf::from(std::ffi::OsStr::from_bytes(l))),
            );
        }
        dirs
    }

    /// Moves `p` into a trash slot no entry refers to (swept after
    /// [`ORPHAN_TTL_SECS`]): what undo displaces is set aside, never deleted.
    fn set_aside(&self, p: &Path) -> io::Result<()> {
        move_path(p, &self.trash_slot(p))
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

    /// The entries, in the order they were last written: a later line with the same
    /// id replaces an earlier one, and cancelled entries are left out.
    ///
    /// Last-written order is the order undo must reverse: an operation that records
    /// another one inside it (`cp -b` renames the destination to its backup name
    /// before copying) completes the inner one first, so the inner one is undone
    /// last, whatever order they were begun in.
    fn parse_entries(&self, content: &str) -> io::Result<Vec<JournalEntry>> {
        // A corrupt journal is an error, not an empty one: silently defaulting would
        // let the next rewrite discard it and orphan everything in the trash.
        let mut latest: std::collections::HashMap<u64, (usize, JournalEntry)> =
            std::collections::HashMap::new();
        for (n, line) in Self::complete_lines(content) {
            let e: JournalEntry =
                serde_json::from_str(line).map_err(|e| corrupt(&self.journal_path(), n, e))?;
            latest.insert(e.id, (n, e));
        }
        let mut entries: Vec<(usize, JournalEntry)> =
            latest.into_values().filter(|(_, e)| !e.cancelled).collect();
        entries.sort_by_key(|(n, _)| *n);
        Ok(entries.into_iter().map(|(_, e)| e).collect())
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
        crash_point();
        fs::write(&tmp, buf)?;
        crash_point();
        fs::rename(&tmp, self.journal_path())
    }

    /// Appends `entry` as one line, dropping a fragment a crashed append left.
    /// Caller holds the lock; `content` is the journal as read under it.
    fn append(&self, content: &str, entry: &JournalEntry) -> io::Result<()> {
        let mut line = serde_json::to_vec(entry)?;
        line.push(b'\n');
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.journal_path())?;
        if !content.is_empty() && !content.ends_with('\n') {
            let keep = content.rfind('\n').map_or(0, |i| i + 1);
            crash_point();
            f.set_len(keep as u64)?;
        }
        crash_point();
        f.write_all(&line)
    }

    fn add(&self, tool: &str, action: JournalAction, pending: bool) -> io::Result<JournalEntry> {
        let _lock = self.lock()?;
        let content = self.read_journal()?;
        let mut last_id = 0;
        for (n, line) in Self::complete_lines(&content) {
            let id = match line_id(line) {
                Some(id) => id,
                None => {
                    serde_json::from_str::<JournalEntry>(line)
                        .map_err(|e| corrupt(&self.journal_path(), n, e))?
                        .id
                }
            };
            last_id = last_id.max(id);
        }
        let entry = JournalEntry {
            id: last_id + 1,
            timestamp_unix: now_unix(),
            tool: tool.to_string(),
            backup_bytes: action.backup_path().map_or(0, path_size),
            backup_purged: false,
            pending,
            pid: if pending { std::process::id() } else { 0 },
            cancelled: false,
            action,
        };
        self.append(&content, &entry)?;
        self.after_append(&content, &entry)?;
        Ok(entry)
    }

    /// Retention needs a full parse, so run it only when a limit can be exceeded:
    /// past the entry cap, after adding a backup, or periodically to sweep orphans.
    /// Over the cap, allow some slack before compacting back to it, so a full journal
    /// is rewritten once every few dozen records instead of on every one.
    fn after_append(&self, content: &str, entry: &JournalEntry) -> io::Result<()> {
        let line_count = Self::complete_lines(content).count() + 1;
        let slack = (self.max_entries / 16).max(1);
        if line_count > self.max_entries + slack
            || (entry.backup_bytes > 0 && !entry.pending)
            || (entry.id.is_multiple_of(64) && !entry.pending)
        {
            self.enforce_retention()?;
        }
        Ok(())
    }

    /// Records a completed operation.
    pub fn record(&self, tool: &str, action: JournalAction) -> io::Result<JournalEntry> {
        self.add(tool, action, false)
    }

    /// Records an operation that is about to start (see the write-ahead protocol on
    /// [`UndoJournal`]). Finish it with [`UndoJournal::commit`] or
    /// [`UndoJournal::cancel`].
    pub fn begin(&self, tool: &str, action: JournalAction) -> io::Result<JournalEntry> {
        self.add(tool, action, true)
    }

    /// Replaces the recorded version of `entry` (same id) with `entry`.
    pub fn update(&self, entry: &mut JournalEntry) -> io::Result<()> {
        entry.backup_bytes = entry.action.backup_path().map_or(0, path_size);
        let _lock = self.lock()?;
        let content = self.read_journal()?;
        self.append(&content, entry)?;
        self.after_append(&content, entry)
    }

    /// Marks a pending `entry` as completed, with its final action.
    pub fn commit(&self, entry: &mut JournalEntry) -> io::Result<()> {
        entry.pending = false;
        entry.pid = 0;
        self.update(entry)
    }

    /// Withdraws a pending `entry`: the operation left nothing to undo.
    pub fn cancel(&self, entry: &mut JournalEntry) -> io::Result<()> {
        entry.cancelled = true;
        self.update(entry)
    }

    /// Applies the entry-count and backup-size limits, deleting the trash payloads of
    /// whatever no longer fits. Caller must hold the lock. Pending entries (an
    /// operation in progress) are never dropped or purged.
    fn enforce_retention(&self) -> io::Result<()> {
        let all = self.load_entries()?;
        let raw_lines = Self::complete_lines(&self.read_journal()?).count();
        let mut changed = all.len() != raw_lines;
        let mut entries = Vec::with_capacity(all.len());
        let mut excess = all.len().saturating_sub(self.max_entries);
        for old in all {
            if excess > 0 && !old.pending {
                excess -= 1;
                for p in old.action.payload_paths() {
                    crash_point();
                    remove_any(p)?;
                }
                changed = true;
            } else {
                entries.push(old);
            }
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
            if e.pending {
                continue;
            }
            if let Some(p) = e.action.drop_backup() {
                crash_point();
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
            .flat_map(|e| e.action.payload_paths().map(Path::to_path_buf))
            .collect();
        for (i, dir) in self.trash_dirs().iter().enumerate() {
            let items = match fs::read_dir(dir) {
                Ok(rd) => rd,
                // A trash directory on a file system that is not mounted now.
                Err(_) if i > 0 => continue,
                Err(e) => return Err(e),
            };
            for item in items.flatten() {
                let path = item.path();
                if referenced.contains(&path) {
                    continue;
                }
                if item.metadata().is_ok_and(|m| m.ctime() < cutoff) {
                    remove_any(&path)?;
                }
            }
        }
        Ok(())
    }

    /// Saves whatever is at `target` (if anything) in the trash, as the backup of the
    /// pending `entry`, so an overwriting `cp`/`mv` can be undone. The trash slot is
    /// recorded in the journal before anything moves; `how` says whether `target`
    /// moves there or stays in place. Returns the slot, or `None` when there is
    /// nothing to save.
    pub fn take(
        &self,
        entry: &mut JournalEntry,
        target: &Path,
        how: Take,
    ) -> io::Result<Option<PathBuf>> {
        if !exists_no_follow(target) {
            return Ok(None);
        }
        let slot = self.trash_slot(target);
        entry.action.set_backup(Some(slot.clone()));
        self.update(entry)?;
        let res = match how {
            Take::Move => move_path(target, &slot),
            Take::Copy => copy_into_slot(target, &slot),
            Take::Link => link_into_slot(target, &slot),
        };
        match res {
            Ok(()) => {
                // Now that it is there, record its size for the backup limit.
                entry.backup_bytes = path_size(&slot);
                Ok(Some(slot))
            }
            // Recorded but not taken: undo of the pending entry knows to leave the
            // target alone (the slot is empty), so failing to say so is harmless.
            Err(e) if !exists_no_follow(&slot) => {
                entry.action.set_backup(None);
                let _ = self.update(entry);
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    /// mv across file systems, about to unlink `src` (a copy of it is complete
    /// at the destination): when `src` is a non-directory with other hard
    /// links, keeps a hard link to it in the trash on its file system, as the
    /// `src_link` of the pending `entry` (a `Move`). The slot is recorded in
    /// the journal before it is made. Returns it, or `None` when there is
    /// nothing to keep or it can't be linked there (undo then copies the
    /// destination back, as for a file with no other links).
    pub fn keep_link(&self, entry: &mut JournalEntry, src: &Path) -> io::Result<Option<PathBuf>> {
        use std::os::unix::fs::MetadataExt;
        let JournalAction::Move { src_link, .. } = &mut entry.action else {
            return Ok(None);
        };
        let Ok(m) = fs::symlink_metadata(src) else {
            return Ok(None);
        };
        if m.is_dir() || m.nlink() < 2 {
            return Ok(None);
        }
        let slot = self.trash_slot(src);
        *src_link = Some(slot.clone());
        self.update(entry)?;
        crash_point();
        if fs::hard_link(src, &slot).is_ok() {
            return Ok(Some(slot));
        }
        // Recorded but not made: undo finds no slot and copies back, so
        // failing to say so is harmless.
        if let JournalAction::Move { src_link, .. } = &mut entry.action {
            *src_link = None;
        }
        let _ = self.update(entry);
        Ok(None)
    }

    /// Stages a file or directory into `~/.dogfish/trash` and records a `TrashRemove` entry.
    pub fn stage_to_trash(&self, target: &Path) -> io::Result<JournalEntry> {
        // `absolute`, not `canonicalize`: trashing a symlink must restore the link
        // itself, not something at the location it points to.
        let abs = std::path::absolute(target)?;
        let trash_target = self.trash_slot(target);
        let mut entry = self.begin(
            "rm",
            JournalAction::TrashRemove {
                original_path: abs,
                trash_path: trash_target.clone(),
            },
        )?;
        if let Err(e) = move_path(target, &trash_target) {
            // A complete copy in the trash beside a partly removed original stays
            // pending: undo restores the copy.
            if !exists_no_follow(&trash_target) {
                let _ = self.cancel(&mut entry);
            }
            return Err(e);
        }
        self.commit(&mut entry)?;
        Ok(entry)
    }

    /// Pops and reverses the most recent journal entry.
    ///
    /// Never overwrites: if something now occupies a path the undo would restore to,
    /// the entry is left in the journal and an error is returned. Paths an undo
    /// removes (copies, overwritten targets) go to the trash rather than being deleted.
    ///
    /// Undo is idempotent: a pending entry (an operation that was interrupted) and an
    /// entry whose undo was itself interrupted are reversed from whatever state they
    /// left, so running `dogfish undo` again always makes progress.
    pub fn undo_last(&self) -> io::Result<Option<(JournalEntry, Undone)>> {
        let _lock = self.lock()?;
        let mut entries = self.load_entries()?;
        let Some(entry) = entries.pop() else {
            return Ok(None);
        };
        if entry.pending
            && entry.pid != 0
            && entry.pid != std::process::id()
            && process_alive(entry.pid)
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "refusing to undo: operation #{} ({}) is still running (pid {})",
                    entry.id, entry.tool, entry.pid
                ),
            ));
        }
        let summary = match &entry.action {
            JournalAction::TrashRemove {
                original_path,
                trash_path,
            } => self.undo_trash(&entry, original_path, trash_path)?,
            JournalAction::Move {
                src,
                dst,
                backup_path,
                copied,
                existed,
                src_inode,
                src_link,
            } => self.undo_move(
                &entry,
                src,
                dst,
                backup_path.as_deref(),
                (*copied, *existed, *src_inode, src_link.as_deref()),
            )?,
            JournalAction::CopyCreate {
                created_path,
                backup_path,
                in_place,
                existed,
                ..
            } => self.undo_copy(
                &entry,
                created_path,
                backup_path.as_deref(),
                *in_place,
                *existed,
            )?,
        };

        self.save_entries(&entries)?;
        Ok(Some((entry, summary)))
    }

    fn undo_trash(&self, entry: &JournalEntry, orig: &Path, trash: &Path) -> io::Result<Undone> {
        let shown = orig.display();
        match (exists_no_follow(orig), exists_no_follow(trash)) {
            (true, false) if entry.pending => {
                Ok(format!("Nothing to restore: '{shown}' was never removed (interrupted)").into())
            }
            // An earlier undo restored it and was interrupted before saving the journal.
            (true, false) => Ok(format!("'{shown}' is already restored").into()),
            (false, false) => {
                Ok(format!("Cannot restore '{shown}': its copy in the trash is gone").into())
            }
            (exists, true) => {
                if exists {
                    if entry.pending {
                        // Interrupted while removing it across file systems: the copy
                        // in the trash is complete, what is left here is not.
                        self.set_aside(orig)?;
                    } else if on_other_fs(orig, trash) && same_tree(orig, trash) {
                        // An earlier undo copied it back across file systems and was
                        // interrupted before removing the trash copy.
                        return Ok(format!("'{shown}' is already restored").into());
                    } else {
                        return Err(refuse_existing(orig));
                    }
                }
                if let Some(parent) = orig.parent() {
                    crash_point();
                    fs::create_dir_all(parent)?;
                }
                move_path(trash, orig)?;
                Ok(Undone::from(format!("Restored '{shown}' from trash")).restored(orig))
            }
        }
    }

    fn undo_move(
        &self,
        entry: &JournalEntry,
        src: &Path,
        dst: &Path,
        backup: Option<&Path>,
        (copied, existed, src_inode, src_link): (bool, bool, Option<(u64, u64)>, Option<&Path>),
    ) -> io::Result<Undone> {
        let (s, t) = (exists_no_follow(src), exists_no_follow(dst));
        let b = backup.is_some_and(exists_no_follow);
        if src == dst {
            // Recorded by an older version for a move that left its source alone.
            return Ok(format!("Nothing to move back to '{}'", src.display()).into());
        }
        // Work out every step before taking any, so a refusal changes nothing.
        let (mut set_aside_src, mut move_back, mut set_aside_dst) = (false, false, false);
        if !s && t {
            // Interrupted, the move happened only if `dst` is `src` renamed
            // (or a complete copy of it).
            move_back =
                !entry.pending || copied || src_inode.is_some_and(|i| inode(dst) == Some(i));
        } else if s && t {
            if copied && fs::symlink_metadata(src).is_ok_and(|m| !m.is_dir()) {
                // Interrupted before unlinking `src` (one step for a
                // non-directory): it is intact, so keep it (its inode and
                // hard links) and drop the copy.
                set_aside_dst = true;
                if let Some(link) = src_link
                    && inode(link).is_some_and(|i| inode(src) == Some(i))
                {
                    crash_point();
                    fs::remove_file(link)?;
                }
            } else if copied {
                set_aside_src = true;
                move_back = true;
            } else if entry.pending {
                // Interrupted before `src` replaced `dst`: what is at `dst` is a
                // partial copy if the original was saved or there was none, else
                // the untouched original.
                set_aside_dst = b || !existed;
            } else if backup.is_some() && !b {
                // An earlier undo finished and was interrupted before saving.
                return Ok(format!("'{}' is already moved back", src.display()).into());
            } else if on_other_fs(src, dst) && same_tree(src, dst) {
                // An earlier undo copied `dst` back across file systems and was
                // interrupted before removing it.
                set_aside_dst = true;
            } else {
                return Err(refuse_existing(src));
            }
        }
        if set_aside_src {
            self.set_aside(src)?;
        }
        if move_back {
            if let Some(parent) = src.parent() {
                crash_point();
                fs::create_dir_all(parent)?;
            }
            match src_link.filter(|l| Self::same_file_kept(l, dst)) {
                // The inode `src` was, with its other hard links: put it back
                // and set the copy aside.
                Some(link) => {
                    crash_point();
                    fs::rename(link, src)?;
                    set_aside_dst = true;
                }
                None => move_path(dst, src)?,
            }
        }
        if set_aside_dst {
            self.set_aside(dst)?;
        }
        if b && let Some(backup) = backup {
            if exists_no_follow(dst) {
                return Err(refuse_existing(dst));
            }
            move_path(backup, dst)?;
        }
        // What changed: `src` is back; `dst` holds its original again, or
        // else what the move put there is gone.
        let mut changed = Undone::default();
        if move_back {
            changed = changed.restored(src);
        }
        if b {
            changed = changed.restored(dst);
        } else if move_back || set_aside_dst {
            changed = changed.removed(dst);
        }
        let (src, dst) = (src.display(), dst.display());
        changed.summary = if b {
            format!("Moved '{dst}' back to '{src}' and restored the original")
        } else if entry.backup_purged {
            format!(
                "Moved '{dst}' back to '{src}'; the file it replaced was discarded \
                 (backup limit) and cannot be restored"
            )
        } else if move_back {
            format!("Moved '{dst}' back to '{src}'")
        } else {
            format!("Nothing to move back to '{src}'")
        };
        Ok(changed)
    }

    /// Whether the hard link kept of a moved file (`link`) can stand in for
    /// the copy at `dst`: both are non-directories, with the same contents
    /// and permission bits. If either was changed since the move, undo copies
    /// `dst` back instead, as a new file: the other links keep what they hold
    /// and `link` stays in the trash, so neither version is lost.
    fn same_file_kept(link: &Path, dst: &Path) -> bool {
        fs::symlink_metadata(link).is_ok_and(|m| !m.is_dir()) && same_tree(link, dst)
    }

    fn undo_copy(
        &self,
        entry: &JournalEntry,
        created: &Path,
        backup: Option<&Path>,
        in_place: bool,
        existed: bool,
    ) -> io::Result<Undone> {
        let shown = created.display();
        if entry.backup_purged {
            // The copy is now the only version left; removing it would lose data.
            return Ok(format!(
                "Left '{shown}' as is: the contents it replaced were discarded \
                 (backup limit) and cannot be restored"
            )
            .into());
        }
        let exists = exists_no_follow(created);
        match backup {
            Some(b) if exists_no_follow(b) => {
                if exists && in_place {
                    self.restore_in_place(b, created, &mut Relinks::default())?;
                    crash_point();
                    remove_any(b)?;
                } else {
                    // Into the trash rather than deleted, in case it was edited since the
                    // copy; unreferenced, it is swept after ORPHAN_TTL_SECS.
                    if exists {
                        self.set_aside(created)?;
                    }
                    move_path(b, created)?;
                }
                Ok(
                    Undone::from(format!("Restored '{shown}' to its state before the copy"))
                        .restored(created),
                )
            }
            // Never saved (interrupted first), or restored by an interrupted undo.
            Some(_) => Ok(format!("Nothing to restore: '{shown}' is as it was").into()),
            None if entry.pending && existed => {
                Ok(format!("Nothing to undo: '{shown}' was not changed (interrupted)").into())
            }
            None if exists => {
                self.set_aside(created)?;
                Ok(Undone::from(format!("Removed copied target '{shown}'")).removed(created))
            }
            None => Ok(format!("Nothing to remove: '{shown}' is gone").into()),
        }
    }

    /// Makes `target` equal to `backup` while keeping the inodes of what both
    /// have: files are rewritten in place (so hard links to them see the old
    /// contents again), directories are reconciled entry by entry. What `target`
    /// holds that `backup` does not is set aside in the trash, and so is the
    /// current version of each file rewritten. Copies out of `backup` (it is
    /// removed by the caller once this succeeds), so it can run again after an
    /// interruption.
    ///
    /// Files that are hard links of each other in `backup` end up hard links of
    /// each other in `target`, and files that are not do not share an inode:
    /// a `cp --preserve=links` replaces a destination with more than one link
    /// rather than write through it, which equal contents would not show.
    fn restore_in_place(&self, backup: &Path, target: &Path, re: &mut Relinks) -> io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let (mb, mt) = (fs::symlink_metadata(backup)?, fs::symlink_metadata(target)?);
        if mb.is_file() && mt.is_file() {
            let (kb, kt) = ((mb.dev(), mb.ino()), (mt.dev(), mt.ino()));
            let first = re.copies.get(&kb).cloned();
            let shared = re.restored.get(&kt).is_some_and(|k| *k != kb);
            if let Some(first) = first {
                // A link of a file already restored: link it again.
                if !fs::symlink_metadata(&first).is_ok_and(|m| (m.dev(), m.ino()) == kt) {
                    self.set_aside(target)?;
                    crash_point();
                    fs::hard_link(&first, target)?;
                }
                return Ok(());
            }
            if shared {
                // Linked to a file restored from another one: a copy of its own.
                self.set_aside(target)?;
                crash_point();
                copy_tree_linked(backup, target, &mut re.copies)?;
                return Ok(());
            }
            if !same_tree(backup, target) {
                if fs::read(backup)? != fs::read(target)? {
                    use std::os::unix::fs::PermissionsExt;
                    copy_into_slot(target, &self.trash_slot(target))?;
                    let data = fs::read(backup)?;
                    // Writable for the rewrite even if neither version is.
                    let mode = mt.permissions().mode();
                    crash_point();
                    fs::set_permissions(target, fs::Permissions::from_mode(mode | 0o200))?;
                    crash_point();
                    let mut f = OpenOptions::new().write(true).truncate(true).open(target)?;
                    f.write_all(&data)?;
                }
                crash_point();
                fs::set_permissions(target, mb.permissions())?;
            }
            if mb.nlink() > 1 {
                re.copies.insert(kb, target.to_path_buf());
            }
            re.restored.insert(kt, kb);
        } else if mb.is_dir() && mt.is_dir() {
            for e in fs::read_dir(target)? {
                let name = e?.file_name();
                if !exists_no_follow(&backup.join(&name)) {
                    self.set_aside(&target.join(&name))?;
                }
            }
            for e in fs::read_dir(backup)? {
                let name = e?.file_name();
                let (b, t) = (backup.join(&name), target.join(&name));
                if exists_no_follow(&t) {
                    self.restore_in_place(&b, &t, re)?;
                } else {
                    copy_tree_linked(&b, &t, &mut re.copies)?;
                }
            }
            crash_point();
            fs::set_permissions(target, mb.permissions())?;
        } else {
            self.set_aside(target)?;
            copy_tree_linked(backup, target, &mut re.copies)?;
        }
        Ok(())
    }
}

/// What [`UndoJournal::restore_in_place`] has restored so far, to put hard
/// links back as the backup has them.
#[derive(Default)]
struct Relinks {
    /// (device, inode) of a backup file with more than one link → the
    /// target it was restored to.
    copies: std::collections::HashMap<(u64, u64), PathBuf>,
    /// (device, inode) of a target restored in place → the backup file's.
    restored: std::collections::HashMap<(u64, u64), (u64, u64)>,
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

    fn inode(p: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        fs::symlink_metadata(p).unwrap().ino()
    }

    #[test]
    fn restore_in_place_puts_back_hard_links_that_were_replaced() {
        // `cp -a` replaced `b`, one of two links, with a file of the same
        // contents: undo links it to `c` again.
        let tmp = tempfile::tempdir().unwrap();
        let j = journal_in(&tmp.path().join("home"));
        let (backup, target) = (tmp.path().join("backup"), tmp.path().join("t"));
        for d in [&backup, &target] {
            fs::create_dir(d).unwrap();
            fs::write(d.join("b"), "same").unwrap();
        }
        fs::hard_link(backup.join("b"), backup.join("c")).unwrap();
        fs::write(target.join("c"), "same").unwrap();
        j.restore_in_place(&backup, &target, &mut Relinks::default())
            .unwrap();
        assert_eq!(inode(&target.join("b")), inode(&target.join("c")));
        assert_eq!(fs::read(target.join("c")).unwrap(), b"same");
    }

    #[test]
    fn restore_in_place_separates_files_that_were_not_linked() {
        // The target's `b` and `c` share an inode the backup's do not: each
        // gets its own contents back, and the other is not rewritten with it.
        let tmp = tempfile::tempdir().unwrap();
        let j = journal_in(&tmp.path().join("home"));
        let (backup, target) = (tmp.path().join("backup"), tmp.path().join("t"));
        fs::create_dir(&backup).unwrap();
        fs::write(backup.join("b"), "bee").unwrap();
        fs::write(backup.join("c"), "sea").unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(target.join("b"), "new").unwrap();
        fs::hard_link(target.join("b"), target.join("c")).unwrap();
        j.restore_in_place(&backup, &target, &mut Relinks::default())
            .unwrap();
        assert_ne!(inode(&target.join("b")), inode(&target.join("c")));
        assert_eq!(fs::read(target.join("b")).unwrap(), b"bee");
        assert_eq!(fs::read(target.join("c")).unwrap(), b"sea");
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
            pending: false,
            pid: 0,
            cancelled: false,
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
            copied: false,
            existed: false,
            src_inode: None,
            src_link: None,
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
            ..
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
            ..
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
