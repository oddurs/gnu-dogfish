//! Terminal-only output: `--json` entries and `--tree`.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::SystemTime;

use dogfish_core::{DogfishContext, Style, format_mode_rwx, format_relative_time};
use serde::Serialize;

use crate::sys::{get_groupname_by_gid, get_username_by_uid};
use crate::tools::ls::lister::LsFile;

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

pub(crate) fn ls_json_entry(f: &LsFile) -> LsEntry {
    let name = String::from_utf8_lossy(&f.name).into_owned();
    let path = f.path.to_string_lossy().into_owned();
    let Some(meta) = &f.meta else {
        return LsEntry {
            name,
            path,
            kind: "unknown".into(),
            size: 0,
            mode: "?????????".into(),
            mode_octal: "?".into(),
            user: "?".into(),
            group: "?".into(),
            modified_unix: 0,
            modified_relative: "?".into(),
        };
    };
    let mode = meta.mode();
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
    let (uid, gid) = (meta.uid(), meta.gid());
    LsEntry {
        name,
        path,
        kind: kind.to_string(),
        size: meta.len(),
        mode: format_mode_rwx(mode),
        mode_octal: format!("{:04o}", mode & 0o7777),
        user: get_username_by_uid(uid).unwrap_or_else(|| uid.to_string()),
        group: get_groupname_by_gid(gid).unwrap_or_else(|| gid.to_string()),
        modified_unix: meta.mtime(),
        modified_relative: format_relative_time(mtime),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_tree<W: Write>(
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
