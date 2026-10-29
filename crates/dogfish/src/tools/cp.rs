//! cp — GNU semantics, CoW reflinks (FICLONE / APFS clonefile) and UndoJournal

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use anyhow::Result;
use dogfish_core::{
    CopyMethod, DogfishContext, JournalAction, UndoJournal, gnu_io_error, move_path, same_file,
};

use crate::common::backup::{BackupMode, backup_mode, backup_suffix, make_backup};
use crate::common::file_times::{Stamp, set_file_times};
use crate::common::mode::{
    CHMOD_MODE_BITS, S_IRWXUGO, S_ISVTX, process_umask, strmode9, u32_to_mode_t,
};
use crate::common::path::{
    Deref, absolute_lossy, ancestor_ends, c_path, concat_path, dir_names, operand_base, same_inode,
    strip_trailing_slashes,
};
use crate::common::prompt::{ask, write_protected};
use crate::common::quote::{qa, qf};
use crate::common::targets::{
    Clobber, SameFileOpts, Targets, resolve_for_containment, resolve_targets, same_file_ok,
};
use crate::common::tool_args::{HasArg, ToolOpt, argmatch, opt, parse_tool_args, usage_error};

const CP_HELP: &str = "\
Usage: cp [OPTION]... [-T] SOURCE DEST
  or:  cp [OPTION]... SOURCE... DIRECTORY
  or:  cp [OPTION]... -t DIRECTORY SOURCE...
Copy SOURCE to DEST, or multiple SOURCE(s) to DIRECTORY.
Copies use copy-on-write reflinks where the file system supports them, and
are recorded so that `dogfish undo` can reverse them.

  -a, --archive                same as -dR --preserve=all
      --attributes-only        don't copy the file data, just the attributes
      --backup[=CONTROL]       make a backup of each existing destination file
  -b                           like --backup but does not accept an argument
  -d                           same as --no-dereference --preserve=links
  -f, --force                  if an existing destination file cannot be
                                 opened, remove it and try again
  -i, --interactive            prompt before overwrite (overrides a previous -n
                                  option)
  -H                           follow command-line symbolic links in SOURCE
  -l, --link                   hard link files instead of copying
  -L, --dereference            always follow symbolic links in SOURCE
  -n, --no-clobber             do not overwrite an existing file (overrides a
                                 previous -i option)
  -P, --no-dereference         never follow symbolic links in SOURCE
  -p                           same as --preserve=mode,ownership,timestamps
      --preserve[=ATTR_LIST]   preserve the specified attributes
      --no-preserve=ATTR_LIST  don't preserve the specified attributes
      --parents                use full source file name under DIRECTORY
  -R, -r, --recursive          copy directories recursively
      --remove-destination     remove each existing destination file before
                                 attempting to open it (contrast with --force)
  -s, --symbolic-link          make symbolic links instead of copying
  -S, --suffix=SUFFIX          override the usual backup suffix
  -t, --target-directory=DIRECTORY  copy all SOURCE arguments into DIRECTORY
  -T, --no-target-directory    treat DEST as a normal file
      --update[=UPDATE]        control which existing files are updated;
                                 UPDATE={all,none,older(default)}
  -u                           equivalent to --update[=older]
  -v, --verbose                explain what is being done
  -x, --one-file-system        stay on this file system
      --help        display this help and exit
      --version     output version information and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum CpOpt {
    Archive,
    AttributesOnly,
    Backup,
    BackupArg,
    CopyContents,
    Debug,
    NoDerefLinks,
    Dereference,
    Force,
    Interactive,
    CmdlineDeref,
    Link,
    NoClobber,
    NoDereference,
    PreserveDefault,
    Preserve,
    NoPreserve,
    Parents,
    KeepDirSymlink,
    Recursive,
    RemoveDestination,
    Reflink,
    Sparse,
    StripSlashes,
    Suffix,
    Symbolic,
    TargetDir,
    NoTargetDir,
    UpdateOlder,
    Update,
    Verbose,
    OneFileSystem,
    Context,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PreserveAttr {
    Mode,
    Timestamps,
    Ownership,
    Links,
    Context,
    Xattr,
    All,
}

const PRESERVE_ARGS: &[(&str, PreserveAttr)] = &[
    ("mode", PreserveAttr::Mode),
    ("timestamps", PreserveAttr::Timestamps),
    ("ownership", PreserveAttr::Ownership),
    ("links", PreserveAttr::Links),
    ("context", PreserveAttr::Context),
    ("xattr", PreserveAttr::Xattr),
    ("all", PreserveAttr::All),
];

struct CpOpts {
    recursive: bool,
    deref: Deref,
    preserve_mode: bool,
    preserve_ownership: bool,
    preserve_timestamps: bool,
    preserve_links: bool,
    /// `--no-preserve=mode`: new files get default permissions, not the source's.
    no_preserve_mode: bool,
    clobber: Clobber,
    update_older: bool,
    backup: BackupMode,
    suffix: String,
    force: bool,
    remove_destination: bool,
    hard_link: bool,
    symbolic_link: bool,
    parents: bool,
    one_file_system: bool,
    verbose: bool,
    attributes_only: bool,
    keep_dir_symlink: bool,
}

/// What the undo journal needs to restore an overwritten destination.
struct UndoPlan {
    created: PathBuf,
    backup: Option<String>,
}

/// State for one `cp` invocation.
struct Copier<'a> {
    o: CpOpts,
    journal: Option<UndoJournal>,
    umask: u32,
    ctx: &'a DogfishContext,
    /// Command-line sources already copied, by name and inode (GNU warns about repeats).
    seen_sources: HashSet<(OsString, u64, u64)>,
    /// Destination of each multiply-linked source inode, for `--preserve=links`.
    linked: HashMap<(u64, u64), PathBuf>,
    top_dev: u64,
}

fn apply_preserve(o: &mut CpOpts, list: &str, on: bool) -> std::result::Result<(), i32> {
    for item in list.split(',') {
        let attr = argmatch(
            "cp",
            if on { "--preserve" } else { "--no-preserve" },
            item,
            PRESERVE_ARGS,
        )?;
        if matches!(attr, PreserveAttr::Mode | PreserveAttr::All) {
            o.no_preserve_mode = !on;
        }
        match attr {
            PreserveAttr::Mode => o.preserve_mode = on,
            PreserveAttr::Timestamps => o.preserve_timestamps = on,
            PreserveAttr::Ownership => o.preserve_ownership = on,
            PreserveAttr::Links => o.preserve_links = on,
            PreserveAttr::Context | PreserveAttr::Xattr => {}
            PreserveAttr::All => {
                o.preserve_mode = on;
                o.preserve_timestamps = on;
                o.preserve_ownership = on;
                o.preserve_links = on;
            }
        }
    }
    Ok(())
}

pub fn run_cp(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    use CpOpt::*;
    const SPECS: &[ToolOpt<CpOpt>] = &[
        opt("a", "archive", HasArg::No, Archive),
        opt("", "attributes-only", HasArg::No, AttributesOnly),
        opt("b", "", HasArg::No, Backup),
        opt("", "backup", HasArg::Optional, BackupArg),
        opt("", "copy-contents", HasArg::No, CopyContents),
        opt("", "debug", HasArg::No, Debug),
        opt("d", "", HasArg::No, NoDerefLinks),
        opt("L", "dereference", HasArg::No, Dereference),
        opt("f", "force", HasArg::No, Force),
        opt("i", "interactive", HasArg::No, Interactive),
        opt("H", "", HasArg::No, CmdlineDeref),
        opt("l", "link", HasArg::No, Link),
        opt("n", "no-clobber", HasArg::No, NoClobber),
        opt("P", "no-dereference", HasArg::No, NoDereference),
        opt("", "no-preserve", HasArg::Required, NoPreserve),
        opt("", "no-target-directory", HasArg::No, NoTargetDir),
        opt("x", "one-file-system", HasArg::No, OneFileSystem),
        opt("", "parents", HasArg::No, Parents),
        opt("", "path", HasArg::No, Parents),
        opt("", "keep-directory-symlink", HasArg::No, KeepDirSymlink),
        opt("p", "", HasArg::No, PreserveDefault),
        opt("", "preserve", HasArg::Optional, Preserve),
        opt("rR", "recursive", HasArg::No, Recursive),
        opt("", "remove-destination", HasArg::No, RemoveDestination),
        opt("", "reflink", HasArg::Optional, Reflink),
        opt("", "sparse", HasArg::Required, Sparse),
        opt("", "strip-trailing-slashes", HasArg::No, StripSlashes),
        opt("S", "suffix", HasArg::Required, Suffix),
        opt("s", "symbolic-link", HasArg::No, Symbolic),
        opt("t", "target-directory", HasArg::Required, TargetDir),
        opt("T", "", HasArg::No, NoTargetDir),
        opt("u", "", HasArg::No, UpdateOlder),
        opt("", "update", HasArg::Optional, Update),
        opt("v", "verbose", HasArg::No, Verbose),
        opt("Z", "", HasArg::No, Context),
        opt("", "context", HasArg::Optional, Context),
    ];
    let parsed = match parse_tool_args("cp", &args, SPECS, CP_HELP) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let mut o = CpOpts {
        recursive: false,
        deref: Deref::Always,
        preserve_mode: false,
        preserve_ownership: false,
        preserve_timestamps: false,
        preserve_links: false,
        no_preserve_mode: false,
        clobber: Clobber::Yes,
        update_older: false,
        backup: BackupMode::None,
        suffix: String::new(),
        force: false,
        remove_destination: false,
        hard_link: false,
        symbolic_link: false,
        parents: false,
        one_file_system: false,
        verbose: false,
        attributes_only: false,
        keep_dir_symlink: false,
    };
    let mut deref = None;
    let (mut want_backup, mut backup_arg, mut suffix) = (false, None, None);
    let (mut target_dir, mut no_target_dir, mut strip_slashes) = (None, false, false);
    for (opt, v) in parsed.opts {
        let v = v.unwrap_or_default();
        match opt {
            Archive => {
                deref = Some(Deref::Never);
                o.recursive = true;
                let _ = apply_preserve(&mut o, "all", true);
            }
            AttributesOnly => o.attributes_only = true,
            Backup => want_backup = true,
            BackupArg => {
                want_backup = true;
                backup_arg = (!v.is_empty()).then_some(v);
            }
            CopyContents | Debug | Reflink | Context => {}
            Sparse => {
                const SPARSE: &[(&str, ())] = &[("never", ()), ("auto", ()), ("always", ())];
                if let Err(code) = argmatch("cp", "--sparse", &v, SPARSE) {
                    return Ok(code);
                }
            }
            NoDerefLinks => {
                deref = Some(Deref::Never);
                o.preserve_links = true;
            }
            Dereference => deref = Some(Deref::Always),
            NoDereference => deref = Some(Deref::Never),
            CmdlineDeref => deref = Some(Deref::CommandLine),
            Force => o.force = true,
            Interactive => o.clobber = Clobber::Ask,
            NoClobber => {
                o.clobber = Clobber::No;
                eprintln!(
                    "cp: warning: behavior of -n is non-portable and may change in future; use --update=none instead"
                );
            }
            Link => o.hard_link = true,
            PreserveDefault => {
                o.preserve_mode = true;
                o.preserve_ownership = true;
                o.preserve_timestamps = true;
            }
            Preserve => {
                let list = if v.is_empty() {
                    "mode,ownership,timestamps"
                } else {
                    &v
                };
                if let Err(code) = apply_preserve(&mut o, list, true) {
                    return Ok(code);
                }
            }
            NoPreserve => {
                if let Err(code) = apply_preserve(&mut o, &v, false) {
                    return Ok(code);
                }
            }
            Parents => o.parents = true,
            KeepDirSymlink => o.keep_dir_symlink = true,
            Recursive => o.recursive = true,
            RemoveDestination => o.remove_destination = true,
            StripSlashes => strip_slashes = true,
            Suffix => {
                want_backup = true;
                suffix = Some(v);
            }
            Symbolic => o.symbolic_link = true,
            TargetDir => target_dir = Some(v),
            NoTargetDir => no_target_dir = true,
            UpdateOlder => o.update_older = true,
            Update => {
                #[derive(Clone, Copy, PartialEq)]
                enum Upd {
                    All,
                    None,
                    Older,
                }
                const UPDATE: &[(&str, Upd)] = &[
                    ("all", Upd::All),
                    ("none", Upd::None),
                    ("older", Upd::Older),
                ];
                let which = if v.is_empty() {
                    Upd::Older
                } else {
                    match argmatch("cp", "--update", &v, UPDATE) {
                        Ok(u) => u,
                        Err(code) => return Ok(code),
                    }
                };
                o.update_older = which == Upd::Older;
                if which == Upd::None {
                    o.clobber = Clobber::Skip;
                }
            }
            Verbose => o.verbose = true,
            OneFileSystem => o.one_file_system = true,
        }
    }
    if o.hard_link && o.symbolic_link {
        return Ok(usage_error(
            "cp",
            "cannot make both hard and symbolic links",
        ));
    }
    if want_backup {
        o.backup = match backup_mode("cp", backup_arg.as_deref()) {
            Ok(m) => m,
            Err(code) => return Ok(code),
        };
    }
    o.suffix = backup_suffix(suffix);
    o.deref = deref.unwrap_or(if o.recursive && !o.hard_link {
        Deref::Never
    } else {
        Deref::Always
    });
    let targets = match resolve_targets("cp", parsed.operands, target_dir, no_target_dir) {
        Ok(t) => t,
        Err(code) => return Ok(code),
    };
    if o.parents && matches!(targets, Targets::Single(..)) {
        return Ok(usage_error(
            "cp",
            "with --parents, the destination must be a directory",
        ));
    }
    let mut copier = Copier {
        o,
        journal: UndoJournal::open_for_recording(),
        umask: process_umask(),
        ctx,
        seen_sources: HashSet::new(),
        linked: HashMap::new(),
        top_dev: 0,
    };
    let strip = |s: &str| {
        if strip_slashes {
            strip_trailing_slashes(s)
        } else {
            s.to_string()
        }
    };
    let mut ok = true;
    match targets {
        Targets::Into(dir, sources) => {
            for src in &sources {
                let src = strip(src);
                let dst = if copier.o.parents {
                    let rel = src.trim_start_matches('/');
                    if !copier.make_parent_dirs(&dir, rel) {
                        ok = false;
                        continue;
                    }
                    concat_path(&dir, OsStr::new(rel))
                } else {
                    concat_path(&dir, &operand_base(&src))
                };
                ok &= copier.copy_top(Path::new(&src), &dst, true);
            }
        }
        Targets::Single(src, dst) => {
            // Like GNU, --strip-trailing-slashes only shapes names formed in a directory.
            ok &= copier.copy_top(Path::new(&src), Path::new(&dst), false);
        }
    }
    Ok(if ok { 0 } else { 1 })
}

impl Copier<'_> {
    /// `--parents`: creates the directories leading to `rel` under `dir`, copying
    /// the source directories' modes; prints `src -> dst` for each one created.
    fn make_parent_dirs(&mut self, dir: &str, rel: &str) -> bool {
        let ends = ancestor_ends(rel);
        for end in ends {
            let src_dir = &rel[..end];
            let dst_dir = concat_path(dir, OsStr::new(src_dir));
            match fs::metadata(&dst_dir) {
                Ok(m) if m.is_dir() => continue,
                Ok(_) => {
                    eprintln!("cp: {} exists but is not a directory", qa(&dst_dir));
                    return false;
                }
                Err(_) => {}
            }
            let src_meta = match fs::metadata(src_dir) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!(
                        "cp: failed to get attributes of {}: {}",
                        qa(src_dir),
                        gnu_io_error(&e)
                    );
                    return false;
                }
            };
            let mode = if self.o.preserve_mode {
                src_meta.mode() & CHMOD_MODE_BITS
            } else {
                src_meta.mode() & S_IRWXUGO & !self.umask
            };
            if let Err(e) = fs::create_dir(&dst_dir) {
                eprintln!(
                    "cp: cannot make directory {}: {}",
                    qa(&dst_dir),
                    gnu_io_error(&e)
                );
                return false;
            }
            let _ = fs::set_permissions(&dst_dir, fs::Permissions::from_mode(mode));
            if self.o.verbose {
                println!("{src_dir} -> {}", dst_dir.display());
            }
        }
        true
    }

    fn src_metadata(&self, src: &Path, command_line: bool) -> io::Result<fs::Metadata> {
        let follow = match self.o.deref {
            Deref::Always => true,
            Deref::CommandLine => command_line,
            Deref::Never => false,
        };
        if follow {
            fs::metadata(src)
        } else {
            fs::symlink_metadata(src)
        }
    }

    /// Copies one command-line operand; `into_dir` is set when the destination was
    /// formed inside a target directory.
    fn copy_top(&mut self, src: &Path, dst: &Path, into_dir: bool) -> bool {
        let src_meta = match self.src_metadata(src, true) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("cp: cannot stat {}: {}", qa(src), gnu_io_error(&e));
                return false;
            }
        };
        if src_meta.is_dir() {
            if !self.o.recursive {
                eprintln!("cp: -r not specified; omitting directory {}", qa(src));
                return false;
            }
            let dst_lmeta = fs::symlink_metadata(dst).ok();
            if let Some(dm) = &dst_lmeta
                && same_inode(&src_meta, dm)
            {
                eprintln!("cp: {} and {} are the same file", qa(src), qa(dst));
                return false;
            }
            // Refuse up front (GNU notices only once it has copied part of the tree).
            // An existing non-directory is reported by `copy_entry` instead.
            if dst_lmeta.as_ref().is_none_or(|m| m.is_dir())
                && let Ok(abs_src) = fs::canonicalize(src)
                && resolve_for_containment(dst).starts_with(&abs_src)
            {
                eprintln!(
                    "cp: cannot copy a directory, {}, into itself, {}",
                    qa(src),
                    qa(dst)
                );
                return false;
            }
        } else if into_dir && self.o.backup == BackupMode::None {
            let key = (
                src.as_os_str().to_os_string(),
                src_meta.dev(),
                src_meta.ino(),
            );
            if !self.seen_sources.insert(key) {
                eprintln!(
                    "cp: warning: source file {} specified more than once",
                    qa(src)
                );
                return true;
            }
        }
        self.top_dev = src_meta.dev();
        self.copy_entry(src, dst, &src_meta, false, true)
    }

    /// Saves what `created` currently holds so `dogfish undo` can restore it.
    /// `by_copy` keeps it in place (it is written into, not replaced).
    fn journal_prepare(&self, created: &Path, by_copy: bool) -> std::result::Result<UndoPlan, ()> {
        let backup = match &self.journal {
            Some(j) => j.backup_existing(created, by_copy).map_err(|e| {
                eprintln!(
                    "cp: cannot back up {} before overwriting: {}",
                    qa(created),
                    gnu_io_error(&e)
                );
            })?,
            None => None,
        };
        Ok(UndoPlan {
            created: created.to_path_buf(),
            backup,
        })
    }

    fn journal_commit(&self, src: &Path, plan: UndoPlan) {
        if let Some(j) = &self.journal {
            let _ = j.record(
                "cp",
                JournalAction::CopyCreate {
                    src: absolute_lossy(src),
                    created_path: absolute_lossy(&plan.created),
                    backup_path: plan.backup,
                },
            );
        }
    }

    /// Removes an existing destination before it is replaced; with a journal it is
    /// moved to the trash instead, so the replacement can be undone.
    fn remove_existing(
        &self,
        dst: &Path,
        plan: &mut Option<UndoPlan>,
        top: bool,
    ) -> io::Result<()> {
        if top && self.journal.is_some() && plan.as_ref().is_none_or(|p| p.backup.is_none()) {
            if let Some(j) = &self.journal
                && let Some(b) = j.backup_existing(dst, false)?
            {
                *plan = Some(UndoPlan {
                    created: dst.to_path_buf(),
                    backup: Some(b),
                });
            }
            return Ok(());
        }
        match fs::remove_file(dst) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    fn emit_verbose(&self, src: &Path, dst: &Path, backup: Option<&Path>) {
        if !self.o.verbose {
            return;
        }
        match backup {
            Some(b) => println!("{} -> {} (backup: {})", qa(src), qa(dst), qa(b)),
            None => println!("{} -> {}", qa(src), qa(dst)),
        }
    }

    /// Copies `src` (already stat'ed as `src_meta`) to `dst`, printing GNU's
    /// diagnostics. `new_dst` means `dst` is known not to exist (inside a freshly
    /// created directory); `top` marks a command-line operand.
    fn copy_entry(
        &mut self,
        src: &Path,
        dst: &Path,
        src_meta: &fs::Metadata,
        new_dst: bool,
        top: bool,
    ) -> bool {
        let src_is_dir = src_meta.is_dir();
        let src_is_link = src_meta.is_symlink();
        let dst_lmeta = if new_dst {
            None
        } else {
            match fs::symlink_metadata(dst) {
                Ok(m) => Some(m),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => {
                    eprintln!("cp: cannot stat {}: {}", qa(dst), gnu_io_error(&e));
                    return false;
                }
            }
        };
        let dst_meta = dst_lmeta.as_ref().and_then(|_| fs::metadata(dst).ok());
        let mut backup_made: Option<PathBuf> = None;
        let mut plan: Option<UndoPlan> = None;

        if let Some(dl) = &dst_lmeta {
            // GNU skips with -n / --update=none before any other check.
            let exists = src_is_link || dst_meta.is_some();
            if !src_is_dir && exists && matches!(self.o.clobber, Clobber::No | Clobber::Skip) {
                return true;
            }
            // Same-file check (GNU's same_file_ok): never truncate or unlink
            // the source itself. A regular file is written through a
            // destination symlink, so it compares with what that resolves to,
            // unless the destination is replaced rather than written.
            let backup = self.o.backup != BackupMode::None;
            let use_lstat = !src_meta.is_file()
                || self.o.symbolic_link
                || self.o.hard_link
                || backup
                || self.o.remove_destination;
            let x = SameFileOpts {
                move_mode: false,
                deref_never: self.o.deref == Deref::Never,
                hard_link: self.o.hard_link,
                symbolic_link: self.o.symbolic_link,
                backup,
                unlink_dest_before_opening: self.o.remove_destination,
            };
            let dst_sb = if use_lstat {
                Some(dl)
            } else {
                dst_meta.as_ref()
            };
            if let Some(dst_sb) = dst_sb {
                match same_file_ok(src, src_meta, dst, dst_sb, &x) {
                    (false, _) => {
                        eprintln!("cp: {} and {} are the same file", qa(src), qa(dst));
                        return false;
                    }
                    (true, true) => return true,
                    (true, false) => {}
                }
            }
            // A symlink to a directory only counts as one with --keep-directory-symlink.
            let dst_is_dir = dl.is_dir()
                || (dl.is_symlink()
                    && dst_meta.as_ref().is_some_and(|m| m.is_dir())
                    && (self.o.keep_dir_symlink || !src_is_dir));
            if src_is_dir {
                if !dst_is_dir {
                    eprintln!(
                        "cp: cannot overwrite non-directory {} with directory {}",
                        qa(dst),
                        qa(src)
                    );
                    return false;
                }
            } else {
                if dst_is_dir {
                    eprintln!(
                        "cp: cannot overwrite directory {} with non-directory",
                        qa(dst)
                    );
                    return false;
                }
                if self.o.update_older
                    && let Some(dm) = dst_meta.as_ref().or(src_is_link.then_some(dl))
                    && (dm.mtime(), dm.mtime_nsec()) >= (src_meta.mtime(), src_meta.mtime_nsec())
                {
                    return true;
                }
                match self.o.clobber {
                    Clobber::No | Clobber::Skip => return true,
                    Clobber::Ask => {
                        let protected = !dl.is_symlink() && write_protected(dst);
                        let q = if protected {
                            let m = dl.mode() & CHMOD_MODE_BITS;
                            format!(
                                "replace {}, overriding mode {m:04o} ({})? ",
                                qa(dst),
                                strmode9(m)
                            )
                        } else {
                            format!("overwrite {}? ", qa(dst))
                        };
                        if !ask("cp", &q) {
                            return false;
                        }
                    }
                    Clobber::Yes => {}
                }
                if self.o.backup != BackupMode::None {
                    match make_backup(
                        dst,
                        self.o.backup,
                        &self.o.suffix,
                        self.journal.as_ref(),
                        "cp",
                    ) {
                        Ok(b) => backup_made = Some(b),
                        Err(e) => {
                            eprintln!("cp: cannot backup {}: {}", qa(dst), gnu_io_error(&e));
                            return false;
                        }
                    }
                } else if self.o.remove_destination
                    // Only a data copy may replace the file: --attributes-only
                    // must leave the destination's contents (and links) alone.
                    || (!self.o.attributes_only
                        && ((self.o.preserve_links && dl.nlink() > 1)
                            || (self.o.deref == Deref::Never && !src_meta.is_file())))
                {
                    if let Err(e) = self.remove_existing(dst, &mut plan, top) {
                        eprintln!("cp: cannot remove {}: {}", qa(dst), gnu_io_error(&e));
                        return false;
                    }
                    if self.o.verbose {
                        println!("removed {}", qa(dst));
                    }
                }
            }
        }
        // Whatever still exists at `dst` now is about to be written to.
        let dst_exists = backup_made.is_none() && fs::symlink_metadata(dst).is_ok();

        if !src_is_dir
            && !(self.ctx.stdout_is_tty
                && src_meta.is_file()
                && !self.o.hard_link
                && !self.o.symbolic_link)
        {
            self.emit_verbose(src, dst, backup_made.as_deref());
        }

        // --preserve=links: a source inode seen before becomes a hard link.
        if self.o.preserve_links && !src_is_dir && src_meta.nlink() > 1 {
            let key = (src_meta.dev(), src_meta.ino());
            if let Some(first) = self.linked.get(&key).cloned() {
                if dst_exists && let Err(e) = self.remove_existing(dst, &mut plan, top) {
                    eprintln!("cp: cannot remove {}: {}", qa(dst), gnu_io_error(&e));
                    return false;
                }
                return match fs::hard_link(&first, dst) {
                    Ok(()) => true,
                    Err(e) => {
                        eprintln!(
                            "cp: cannot create hard link {} to {}: {}",
                            qa(dst),
                            qa(&first),
                            gnu_io_error(&e)
                        );
                        false
                    }
                };
            }
            self.linked.insert(key, dst.to_path_buf());
        }

        if self.o.symbolic_link && !src_is_dir {
            return self.make_link(src, dst, dst_exists, top, &mut plan, true);
        }
        if self.o.hard_link && !src_is_dir {
            return self.make_link(src, dst, dst_exists, top, &mut plan, false);
        }
        if src_is_dir {
            return self.copy_dir(src, dst, src_meta, dst_lmeta.is_some(), top);
        }
        if src_is_link {
            let target = match fs::read_link(src) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!(
                        "cp: cannot read symbolic link {}: {}",
                        qa(src),
                        gnu_io_error(&e)
                    );
                    return false;
                }
            };
            if top && plan.is_none() {
                plan = Some(UndoPlan {
                    created: dst.to_path_buf(),
                    backup: None,
                });
            }
            if let Err(e) = symlink(&target, dst) {
                eprintln!(
                    "cp: cannot create symbolic link {}: {}",
                    qa(dst),
                    gnu_io_error(&e)
                );
                return false;
            }
            self.preserve_attrs(dst, src_meta, true);
            if let Some(p) = plan {
                self.journal_commit(src, p);
            }
            return true;
        }
        self.copy_file(src, dst, src_meta, dst_exists, top, plan)
    }

    /// `cp -s` / `cp -l`.
    fn make_link(
        &self,
        src: &Path,
        dst: &Path,
        dst_exists: bool,
        top: bool,
        plan: &mut Option<UndoPlan>,
        symbolic: bool,
    ) -> bool {
        if symbolic && !src.is_absolute() {
            let parent = match dst.parent() {
                Some(p) if !p.as_os_str().is_empty() => p,
                _ => Path::new("."),
            };
            if !same_file(parent, Path::new(".")) {
                eprintln!(
                    "cp: {}: can make relative symbolic links only in current directory",
                    qf(dst)
                );
                return false;
            }
        }
        let create = || {
            if symbolic {
                symlink(src, dst)
            } else if self.o.deref == Deref::Always {
                fs::canonicalize(src).and_then(|real| fs::hard_link(real, dst))
            } else {
                fs::hard_link(src, dst)
            }
        };
        let mut res = create();
        if dst_exists
            && self.o.force
            && res
                .as_ref()
                .is_err_and(|e| e.kind() == io::ErrorKind::AlreadyExists)
        {
            res = self.remove_existing(dst, plan, top).and_then(|()| create());
        }
        match res {
            Ok(()) => {
                if let Some(p) = plan.take() {
                    self.journal_commit(src, p);
                }
                true
            }
            Err(e) => {
                let kind = if symbolic { "symbolic" } else { "hard" };
                eprintln!(
                    "cp: cannot create {kind} link {} to {}: {}",
                    qa(dst),
                    qa(src),
                    gnu_io_error(&e)
                );
                false
            }
        }
    }

    fn copy_dir(
        &mut self,
        src: &Path,
        dst: &Path,
        src_meta: &fs::Metadata,
        dst_existed: bool,
        top: bool,
    ) -> bool {
        let mut plan = None;
        let created = !dst_existed;
        if created {
            if let Err(e) = fs::DirBuilder::new().mode(0o700).create(dst) {
                eprintln!(
                    "cp: cannot create directory {}: {}",
                    qa(dst),
                    gnu_io_error(&e)
                );
                return false;
            }
            if self.o.verbose {
                println!("{} -> {}", qa(src), qa(dst));
            }
            if top {
                plan = Some(UndoPlan {
                    created: dst.to_path_buf(),
                    backup: None,
                });
            }
        } else if top {
            // Merging into an existing tree: keep a copy of it for `dogfish undo`.
            match self.journal_prepare(dst, true) {
                Ok(p) => plan = Some(p),
                Err(()) => return false,
            }
        }

        let mut ok = true;
        let descend = !(self.o.one_file_system && !top && src_meta.dev() != self.top_dev);
        if descend {
            match dir_names(src, true) {
                Ok(names) => {
                    for name in names {
                        let (s, d) = (src.join(&name), dst.join(&name));
                        match self.src_metadata(&s, false) {
                            Ok(m) => ok &= self.copy_entry(&s, &d, &m, created, false),
                            Err(e) => {
                                eprintln!("cp: cannot stat {}: {}", qa(&s), gnu_io_error(&e));
                                ok = false;
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("cp: cannot access {}: {}", qa(src), gnu_io_error(&e));
                    ok = false;
                }
            }
        }

        if created || self.o.preserve_mode {
            let mode = if self.o.preserve_mode {
                src_meta.mode() & CHMOD_MODE_BITS
            } else if self.o.no_preserve_mode {
                S_IRWXUGO & !self.umask
            } else {
                src_meta.mode() & (S_IRWXUGO | S_ISVTX) & !self.umask
            };
            if let Err(e) = fs::set_permissions(dst, fs::Permissions::from_mode(mode)) {
                eprintln!(
                    "cp: setting permissions for {}: {}",
                    qa(dst),
                    gnu_io_error(&e)
                );
                ok = false;
            }
        }
        self.preserve_attrs(dst, src_meta, false);
        if let Some(p) = plan {
            // Recorded even after a partial failure, so undo can restore the tree.
            self.journal_commit(src, p);
        }
        ok
    }

    fn copy_file(
        &self,
        src: &Path,
        dst: &Path,
        src_meta: &fs::Metadata,
        dst_exists: bool,
        top: bool,
        mut plan: Option<UndoPlan>,
    ) -> bool {
        use std::os::unix::fs::FileTypeExt;
        if !src_meta.is_file() && !(src_meta.file_type().is_fifo() && !self.o.recursive) {
            return self.copy_special(dst, src_meta, dst_exists);
        }
        let mut input = match fs::File::open(src) {
            Ok(f) => f,
            Err(e) => {
                eprintln!(
                    "cp: cannot open {} for reading: {}",
                    qa(src),
                    gnu_io_error(&e)
                );
                return false;
            }
        };
        // Writing through a symlink changes its referent: that is what to back up.
        let dangling = dst_exists
            && fs::symlink_metadata(dst).is_ok_and(|m| m.is_symlink())
            && fs::metadata(dst).is_err();
        let mut existing = dst_exists;
        if dangling {
            if !self.o.force {
                eprintln!("cp: not writing through dangling symlink {}", qa(dst));
                return false;
            }
            if let Err(e) = self.remove_existing(dst, &mut plan, top) {
                eprintln!("cp: cannot remove {}: {}", qa(dst), gnu_io_error(&e));
                return false;
            }
            existing = false;
        }
        if top && existing && plan.is_none() {
            let real = fs::canonicalize(dst).unwrap_or_else(|_| dst.to_path_buf());
            match self.journal_prepare(&real, true) {
                Ok(p) => plan = Some(p),
                Err(()) => return false,
            }
        }

        // APFS: a new file can be a zero-copy clone.
        #[cfg(target_os = "macos")]
        if !existing && !self.o.attributes_only && clone_new_file(src, dst) {
            if !self.o.preserve_timestamps {
                // clonefile(2) copies the source's times, but a copy is a new file.
                let _ = set_file_times(dst, Stamp::Now, Stamp::Now, false);
            }
            let len = src_meta.len();
            return self.finish_copy(
                src,
                dst,
                src_meta,
                (top, existing),
                plan,
                (len, CopyMethod::ClonefileCoW),
            );
        }

        let opened = if existing {
            let truncate = !self.o.attributes_only;
            match fs::OpenOptions::new()
                .write(true)
                .truncate(truncate)
                .open(dst)
            {
                Ok(f) => Ok(f),
                Err(e) if self.o.force => match self.remove_existing(dst, &mut plan, top) {
                    Ok(()) => {
                        if self.o.verbose {
                            println!("removed {}", qa(dst));
                        }
                        existing = false;
                        self.create_new(dst, src_meta)
                    }
                    Err(_) => Err(format!(
                        "cannot create regular file {}: {}",
                        qa(dst),
                        gnu_io_error(&e)
                    )),
                },
                Err(e) => Err(format!(
                    "cannot create regular file {}: {}",
                    qa(dst),
                    gnu_io_error(&e)
                )),
            }
        } else {
            self.create_new(dst, src_meta)
        };
        let output = match opened {
            Ok(f) => f,
            Err(msg) => {
                eprintln!("cp: {msg}");
                return false;
            }
        };
        let copied = if self.o.attributes_only {
            Ok((0, CopyMethod::StandardCopy))
        } else {
            copy_data(&mut input, &output)
        };
        match copied {
            Ok(stats) => {
                drop(output);
                self.finish_copy(src, dst, src_meta, (top, existing), plan, stats)
            }
            Err(e) => {
                eprintln!(
                    "cp: error copying {} to {}: {}",
                    qa(src),
                    qa(dst),
                    gnu_io_error(&e)
                );
                drop(output);
                // Put the saved original back so a failed copy never loses it.
                if let Some(UndoPlan {
                    created,
                    backup: Some(b),
                }) = &plan
                {
                    let _ = fs::remove_file(created);
                    let _ = move_path(Path::new(b), created);
                }
                false
            }
        }
    }

    /// After the data of a regular file is in place: telemetry, permissions,
    /// preserved attributes and the undo journal. `(top, existing)` says whether
    /// `dst` is a command-line operand and whether it existed before.
    fn finish_copy(
        &self,
        src: &Path,
        dst: &Path,
        src_meta: &fs::Metadata,
        (top, existing): (bool, bool),
        plan: Option<UndoPlan>,
        (bytes, method): (u64, CopyMethod),
    ) -> bool {
        let t = &self.ctx.telemetry;
        t.set_strategy(method.as_str());
        t.add_read(bytes);
        t.add_written(bytes);
        t.add_items(1);
        if self.o.verbose && self.ctx.stdout_is_tty {
            // Dogfish extra, only on a terminal: how the data was copied.
            println!("{} -> {} ({})", qa(src), qa(dst), method.as_str());
        }
        if !existing && !self.o.preserve_mode {
            // New files get the source's permissions minus the umask (as with open(2)).
            let base = if self.o.no_preserve_mode {
                0o666
            } else {
                src_meta.mode() & S_IRWXUGO
            };
            let _ = fs::set_permissions(dst, fs::Permissions::from_mode(base & !self.umask));
        }
        self.preserve_attrs(dst, src_meta, false);
        let plan = plan.or_else(|| {
            top.then(|| UndoPlan {
                created: dst.to_path_buf(),
                backup: None,
            })
        });
        if let Some(p) = plan {
            self.journal_commit(src, p);
        }
        true
    }

    fn create_new(
        &self,
        dst: &Path,
        src_meta: &fs::Metadata,
    ) -> std::result::Result<fs::File, String> {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(src_meta.mode() & S_IRWXUGO)
            .open(dst)
            .map_err(|e| {
                format!(
                    "cannot create regular file {}: {}",
                    qa(dst),
                    gnu_io_error(&e)
                )
            })
    }

    /// FIFOs (with -R); other special files are not supported.
    fn copy_special(&self, dst: &Path, src_meta: &fs::Metadata, dst_exists: bool) -> bool {
        use std::os::unix::fs::FileTypeExt;
        if dst_exists && let Err(e) = fs::remove_file(dst) {
            eprintln!("cp: cannot remove {}: {}", qa(dst), gnu_io_error(&e));
            return false;
        }
        let res = if src_meta.file_type().is_fifo() {
            c_path(dst).and_then(|c| {
                let mode = u32_to_mode_t(src_meta.mode() & S_IRWXUGO);
                if unsafe { libc::mkfifo(c.as_ptr(), mode) } == 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            })
        } else {
            Err(io::Error::from_raw_os_error(libc::ENOTSUP))
        };
        match res {
            Ok(()) => {
                self.preserve_attrs(dst, src_meta, false);
                true
            }
            Err(e) => {
                let what = if src_meta.file_type().is_fifo() {
                    "fifo"
                } else {
                    "special file"
                };
                eprintln!("cp: cannot create {what} {}: {}", qa(dst), gnu_io_error(&e));
                false
            }
        }
    }

    /// Applies `-p` / `--preserve` attributes (ownership, then mode, then times).
    fn preserve_attrs(&self, dst: &Path, src_meta: &fs::Metadata, is_link: bool) {
        if self.o.preserve_ownership {
            // Like GNU, failing to change ownership as a regular user is not an error.
            let _ = std::os::unix::fs::lchown(dst, Some(src_meta.uid()), Some(src_meta.gid()));
        }
        if self.o.preserve_mode && !is_link {
            let _ = fs::set_permissions(
                dst,
                fs::Permissions::from_mode(src_meta.mode() & CHMOD_MODE_BITS),
            );
        }
        if self.o.preserve_timestamps {
            let ns = |n: i64| u32::try_from(n).unwrap_or(0);
            let _ = set_file_times(
                dst,
                Stamp::At(src_meta.atime(), ns(src_meta.atime_nsec())),
                Stamp::At(src_meta.mtime(), ns(src_meta.mtime_nsec())),
                false,
            );
        }
    }
}

/// `clonefile(2)`: an instant copy-on-write clone on APFS (`dst` must not exist).
#[cfg(target_os = "macos")]
fn clone_new_file(src: &Path, dst: &Path) -> bool {
    match (c_path(src), c_path(dst)) {
        (Ok(s), Ok(d)) => unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) == 0 },
        _ => false,
    }
}

/// Copies all of `src` into the empty `dst`, as a CoW reflink where the file
/// system supports it.
fn copy_data(src: &mut fs::File, dst: &fs::File) -> io::Result<(u64, CopyMethod)> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        // FICLONE: instant copy-on-write on Btrfs, XFS, bcachefs.
        const FICLONE: libc::c_ulong = 0x40049409;
        if unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE as _, src.as_raw_fd()) } == 0 {
            return Ok((src.metadata()?.len(), CopyMethod::FicloneCoW));
        }
    }
    // std specializes File-to-File `io::copy` to copy_file_range(2) / fcopyfile(3).
    let bytes = io::copy(src, &mut &*dst)?;
    Ok((bytes, CopyMethod::StandardCopy))
}
