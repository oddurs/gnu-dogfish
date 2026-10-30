//! The `-J` record fields that describe a file, shared by every tool that
//! reports on files (`ls`, `stat`), so they carry the same names and types
//! everywhere (the glossary in `doc/ndjson.md`).

use std::fs::Metadata;
use std::os::unix::fs::{FileTypeExt, MetadataExt};

use dogfish_core::Record;

use crate::common::file_info::{dev_major, dev_minor};
use crate::common::users::{get_groupname_by_gid, get_username_by_uid};

/// User and group names, looked up once per id in a row of files (usually
/// all of them share one owner).
#[derive(Default)]
pub(crate) struct Owners {
    user: Option<(u32, Option<String>)>,
    group: Option<(u32, Option<String>)>,
}

impl Owners {
    pub(crate) fn user(&mut self, uid: u32) -> Option<&str> {
        if self.user.as_ref().is_none_or(|(id, _)| *id != uid) {
            self.user = Some((uid, get_username_by_uid(uid)));
        }
        self.user.as_ref().and_then(|(_, n)| n.as_deref())
    }

    pub(crate) fn group(&mut self, gid: u32) -> Option<&str> {
        if self.group.as_ref().is_none_or(|(id, _)| *id != gid) {
            self.group = Some((gid, get_groupname_by_gid(gid)));
        }
        self.group.as_ref().and_then(|(_, n)| n.as_deref())
    }
}

/// What `stat(2)` says about a file: `type`, `size`, `blocks` (512-byte
/// units), `mode` and `mode_symbolic`, `nlink`, `uid`, `user`, `gid`,
/// `group`, `inode`, `dev`, `rdev_major`/`rdev_minor` for a device, and the
/// times `mtime`, `atime`, `ctime` and, where recorded, `btime` (each with
/// its `_ns` twin). `user` and `group` are left out when the id has no name.
pub(crate) fn file_fields(r: &mut Record<'_>, m: &Metadata, owners: &mut Owners) {
    let mode = m.mode();
    r.file_type(mode)
        .u64("size", m.size())
        .u64("blocks", m.blocks())
        .mode(mode)
        .u64("nlink", m.nlink())
        .u64("uid", u64::from(m.uid()));
    if let Some(user) = owners.user(m.uid()) {
        r.str("user", user);
    }
    r.u64("gid", u64::from(m.gid()));
    if let Some(group) = owners.group(m.gid()) {
        r.str("group", group);
    }
    r.u64("inode", m.ino()).u64("dev", m.dev());
    let ft = m.file_type();
    if ft.is_block_device() || ft.is_char_device() {
        r.u64("rdev_major", dev_major(m.rdev()))
            .u64("rdev_minor", dev_minor(m.rdev()));
    }
    r.time("mtime", m.mtime(), nsec(m.mtime_nsec()))
        .time("atime", m.atime(), nsec(m.atime_nsec()))
        .time("ctime", m.ctime(), nsec(m.ctime_nsec()));
    if let Ok(birth) = m.created()
        && let Ok(d) = birth.duration_since(std::time::UNIX_EPOCH)
    {
        r.time(
            "btime",
            i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
            d.subsec_nanos(),
        );
    }
}

/// A `st_*time_nsec` value, which the kernel keeps below 10^9.
fn nsec(n: i64) -> u32 {
    u32::try_from(n.clamp(0, 999_999_999)).unwrap_or(0)
}
