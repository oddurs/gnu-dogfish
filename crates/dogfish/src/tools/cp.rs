//! cp — GNU semantics (a port of GNU's copy.c), CoW reflinks (FICLONE /
//! APFS clonefile) and the UndoJournal

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::Result;
use dogfish_core::{DogfishContext, UndoJournal, gnu_io_error};

use crate::common::backup::{BackupMode, backup_mode, backup_name, backup_suffix};
use crate::common::copy::{Copier, CopyOptions, Interactive, Reflink, Sparse};
use crate::common::file_times::{Stamp, set_file_times};
use crate::common::getopt::{
    Getopt, HasArg, OptSpec, argmatch, getopt_error, lossy, print_help, print_version, usage_error,
};
use crate::common::mode::{CHMOD_MODE_BITS, S_IRWXUGO, process_umask};
use crate::common::path::{
    Deref, base_len, file_name_concat, last_component, strip_trailing_slashes,
};
use crate::common::quote::{qa, quote_locale};
use crate::common::targets::{Targets, resolve_targets};

const S_IRWXU: u32 = 0o700;
const S_IRWXG: u32 = 0o070;
const S_IRWXO: u32 = 0o007;
const S_IWGRP: u32 = 0o020;
const S_IWOTH: u32 = 0o002;

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
    Help,
    Version,
}

/// GNU 9.4's `long_opts` (in its order: it decides ambiguity diagnostics) and
/// `"abdfHilLnprst:uvxPRS:TZ"`.
const SPECS: &[OptSpec<CpOpt>] = &[
    OptSpec::both(b'a', "archive", HasArg::No, CpOpt::Archive),
    OptSpec::long("attributes-only", HasArg::No, CpOpt::AttributesOnly),
    OptSpec::long("backup", HasArg::Optional, CpOpt::BackupArg),
    OptSpec::long("copy-contents", HasArg::No, CpOpt::CopyContents),
    OptSpec::long("debug", HasArg::No, CpOpt::Debug),
    OptSpec::both(b'L', "dereference", HasArg::No, CpOpt::Dereference),
    OptSpec::both(b'f', "force", HasArg::No, CpOpt::Force),
    OptSpec::both(b'i', "interactive", HasArg::No, CpOpt::Interactive),
    OptSpec::both(b'l', "link", HasArg::No, CpOpt::Link),
    OptSpec::both(b'n', "no-clobber", HasArg::No, CpOpt::NoClobber),
    OptSpec::both(b'P', "no-dereference", HasArg::No, CpOpt::NoDereference),
    OptSpec::long("no-preserve", HasArg::Required, CpOpt::NoPreserve),
    OptSpec::both(b'T', "no-target-directory", HasArg::No, CpOpt::NoTargetDir),
    OptSpec::both(b'x', "one-file-system", HasArg::No, CpOpt::OneFileSystem),
    OptSpec::long("parents", HasArg::No, CpOpt::Parents),
    OptSpec::long("path", HasArg::No, CpOpt::Parents),
    OptSpec::long("preserve", HasArg::Optional, CpOpt::Preserve),
    OptSpec::both(b'R', "recursive", HasArg::No, CpOpt::Recursive),
    OptSpec::long("remove-destination", HasArg::No, CpOpt::RemoveDestination),
    OptSpec::long("sparse", HasArg::Required, CpOpt::Sparse),
    OptSpec::long("reflink", HasArg::Optional, CpOpt::Reflink),
    OptSpec::long("strip-trailing-slashes", HasArg::No, CpOpt::StripSlashes),
    OptSpec::both(b'S', "suffix", HasArg::Required, CpOpt::Suffix),
    OptSpec::both(b's', "symbolic-link", HasArg::No, CpOpt::Symbolic),
    OptSpec::both(b't', "target-directory", HasArg::Required, CpOpt::TargetDir),
    OptSpec::long("update", HasArg::Optional, CpOpt::Update),
    OptSpec::both(b'v', "verbose", HasArg::No, CpOpt::Verbose),
    OptSpec::long("context", HasArg::Optional, CpOpt::Context),
    OptSpec::long("help", HasArg::No, CpOpt::Help),
    OptSpec::long("version", HasArg::No, CpOpt::Version),
    OptSpec::short(b'b', HasArg::No, CpOpt::Backup),
    OptSpec::short(b'd', HasArg::No, CpOpt::NoDerefLinks),
    OptSpec::short(b'H', HasArg::No, CpOpt::CmdlineDeref),
    OptSpec::short(b'p', HasArg::No, CpOpt::PreserveDefault),
    OptSpec::short(b'r', HasArg::No, CpOpt::Recursive),
    OptSpec::short(b'u', HasArg::No, CpOpt::UpdateOlder),
    OptSpec::short(b'Z', HasArg::No, CpOpt::Context),
];

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
    deref: Option<Deref>,
    preserve_mode: bool,
    preserve_ownership: bool,
    preserve_timestamps: bool,
    preserve_links: bool,
    /// `--no-preserve=mode`: new files get default permissions, not the source's.
    no_preserve_mode: bool,
    /// `-p` or `--preserve`: failing to preserve an attribute is an error.
    require_preserve: bool,
    interactive: Interactive,
    update_older: bool,
    force: bool,
    remove_destination: bool,
    hard_link: bool,
    symbolic_link: bool,
    parents: bool,
    one_file_system: bool,
    verbose: bool,
    attributes_only: bool,
    copy_contents: bool,
    sparse: Sparse,
    reflink: Reflink,
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

pub fn run_cp(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    use CpOpt::*;
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    let mut o = CpOpts {
        recursive: false,
        deref: None,
        preserve_mode: false,
        preserve_ownership: false,
        preserve_timestamps: false,
        preserve_links: false,
        no_preserve_mode: false,
        require_preserve: false,
        interactive: crate::common::copy::Interactive::Unspecified,
        update_older: false,
        force: false,
        remove_destination: false,
        hard_link: false,
        symbolic_link: false,
        parents: false,
        one_file_system: false,
        verbose: false,
        attributes_only: false,
        copy_contents: false,
        sparse: crate::common::copy::Sparse::Auto,
        reflink: crate::common::copy::Reflink::Auto,
    };
    let (mut want_backup, mut backup_arg, mut suffix) = (false, None, None);
    let (mut target_dir, mut no_target_dir, mut strip_slashes) = (None, false, false);
    while let Some(opt) = opts.next() {
        let (opt, v) = match opt {
            Ok(o) => o,
            Err(e) => return Ok(getopt_error("cp", &e)),
        };
        let raw = v.clone().unwrap_or_default();
        let v = v.as_deref().map(lossy).unwrap_or_default();
        match opt {
            Help => return Ok(print_help(CP_HELP)),
            Version => return Ok(print_version("cp")),
            Archive => {
                o.deref = Some(Deref::Never);
                o.recursive = true;
                o.require_preserve = true;
                let _ = apply_preserve(&mut o, "all", true);
            }
            AttributesOnly => o.attributes_only = true,
            Backup => want_backup = true,
            BackupArg => {
                want_backup = true;
                backup_arg = (!v.is_empty()).then_some(v);
            }
            CopyContents => o.copy_contents = true,
            Debug | Context => {}
            Reflink => {
                const REFLINK: &[(&str, crate::common::copy::Reflink)] = &[
                    ("auto", crate::common::copy::Reflink::Auto),
                    ("always", crate::common::copy::Reflink::Always),
                    ("never", crate::common::copy::Reflink::Never),
                ];
                o.reflink = if v.is_empty() {
                    crate::common::copy::Reflink::Always
                } else {
                    match argmatch("cp", "--reflink", &v, REFLINK) {
                        Ok(r) => r,
                        Err(code) => return Ok(code),
                    }
                };
            }
            Sparse => {
                const SPARSE: &[(&str, crate::common::copy::Sparse)] = &[
                    ("never", crate::common::copy::Sparse::Never),
                    ("auto", crate::common::copy::Sparse::Auto),
                    ("always", crate::common::copy::Sparse::Always),
                ];
                o.sparse = match argmatch("cp", "--sparse", &v, SPARSE) {
                    Ok(s) => s,
                    Err(code) => return Ok(code),
                };
            }
            NoDerefLinks => {
                o.deref = Some(Deref::Never);
                o.preserve_links = true;
            }
            Dereference => o.deref = Some(Deref::Always),
            NoDereference => o.deref = Some(Deref::Never),
            CmdlineDeref => o.deref = Some(Deref::CommandLine),
            Force => o.force = true,
            Interactive => o.interactive = crate::common::copy::Interactive::AskUser,
            NoClobber => o.interactive = crate::common::copy::Interactive::AlwaysNo,
            Link => o.hard_link = true,
            PreserveDefault => {
                o.preserve_mode = true;
                o.preserve_ownership = true;
                o.preserve_timestamps = true;
                o.require_preserve = true;
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
                o.require_preserve = true;
            }
            NoPreserve => {
                if let Err(code) = apply_preserve(&mut o, &v, false) {
                    return Ok(code);
                }
            }
            Parents => o.parents = true,
            Recursive => o.recursive = true,
            RemoveDestination => o.remove_destination = true,
            StripSlashes => strip_slashes = true,
            Suffix => {
                want_backup = true;
                suffix = Some(v);
            }
            Symbolic => o.symbolic_link = true,
            TargetDir => {
                if target_dir.is_some() {
                    eprintln!("cp: multiple target directories specified");
                    return Ok(1);
                }
                target_dir = Some(raw);
            }
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
                if v.is_empty() {
                    o.update_older = true;
                } else if o.interactive != crate::common::copy::Interactive::AlwaysNo {
                    // -n takes precedence; otherwise the last --update wins.
                    let which = match argmatch("cp", "--update", &v, UPDATE) {
                        Ok(u) => u,
                        Err(code) => return Ok(code),
                    };
                    o.update_older = which == Upd::Older;
                    o.interactive = if which == Upd::None {
                        crate::common::copy::Interactive::AlwaysSkip
                    } else {
                        crate::common::copy::Interactive::Unspecified
                    };
                }
            }
            Verbose => o.verbose = true,
            OneFileSystem => o.one_file_system = true,
        }
    }
    // --sparse=never also rules out reflinks (and copy offload) that could
    // carry holes over.
    if o.reflink == crate::common::copy::Reflink::Auto
        && o.sparse == crate::common::copy::Sparse::Never
    {
        o.reflink = crate::common::copy::Reflink::Never;
    }
    if o.hard_link && o.symbolic_link {
        return Ok(usage_error(
            "cp",
            "cannot make both hard and symbolic links",
        ));
    }
    if o.interactive == crate::common::copy::Interactive::AlwaysNo {
        o.update_older = false;
    }
    if want_backup && o.interactive == crate::common::copy::Interactive::AlwaysNo {
        return Ok(usage_error(
            "cp",
            "options --backup and --no-clobber are mutually exclusive",
        ));
    }
    if o.reflink == crate::common::copy::Reflink::Always
        && o.sparse != crate::common::copy::Sparse::Auto
    {
        return Ok(usage_error(
            "cp",
            "--reflink can be used only with --sparse=auto",
        ));
    }
    let backup = if want_backup {
        match backup_mode("cp", backup_arg.as_deref()) {
            Ok(m) => m,
            Err(code) => return Ok(code),
        }
    } else {
        BackupMode::None
    };
    let x = copy_options(&o, backup, backup_suffix(suffix));
    let targets = match resolve_targets("cp", opts.operands(), target_dir, no_target_dir) {
        Ok(t) => t,
        Err(code) => return Ok(code),
    };
    if o.parents && matches!(targets, Targets::Single(..)) {
        return Ok(usage_error(
            "cp",
            "with --parents, the destination must be a directory",
        ));
    }
    let mut copier = Copier::new(
        "cp",
        x,
        UndoJournal::open_for_recording(),
        process_umask(),
        ctx,
    );
    let ok = match targets {
        Targets::Into(dir, sources) => {
            copy_into(&mut copier, &dir, sources, o.parents, strip_slashes)
        }
        Targets::Single(src, dst) => copy_single(&mut copier, &src, &dst, no_target_dir),
    };
    Ok(if ok { 0 } else { 1 })
}

/// The copy settings for cp's options (cp.c's `main`).
fn copy_options(o: &CpOpts, backup: BackupMode, suffix: String) -> CopyOptions {
    let mut x = CopyOptions::for_cp();
    x.backup = backup;
    x.suffix = suffix;
    x.dereference = o.deref.unwrap_or(if o.recursive && !o.hard_link {
        Deref::Never
    } else {
        Deref::Always
    });
    x.interactive = o.interactive;
    x.sparse = o.sparse;
    x.reflink = o.reflink;
    // With -R, special files are recreated unless --copy-contents.
    x.copy_as_regular = !o.recursive || o.copy_contents;
    x.unlink_dest_before_opening = o.remove_destination;
    x.unlink_dest_after_failed_open = o.force;
    x.hard_link = o.hard_link;
    x.symbolic_link = o.symbolic_link;
    x.one_file_system = o.one_file_system;
    x.preserve_ownership = o.preserve_ownership;
    x.preserve_mode = o.preserve_mode;
    x.preserve_timestamps = o.preserve_timestamps;
    x.explicit_no_preserve_mode = o.no_preserve_mode;
    x.preserve_links = o.preserve_links;
    x.data_copy_required = !o.attributes_only;
    x.require_preserve = o.require_preserve;
    x.recursive = o.recursive;
    x.update = o.update_older;
    x.verbose = o.verbose;
    x
}

/// `cp SOURCE DEST`. As a special case, `cp --force --backup F F` backs up
/// the regular file F by copying it to its backup name.
fn copy_single(copier: &mut Copier<'_>, src: &OsStr, dst: &OsStr, no_target_dir: bool) -> bool {
    // GNU knows DEST is missing (or a dangling symlink) from trying to open it
    // as the target directory, which it does not do with -T.
    let new_dst =
        !no_target_dir && fs::metadata(dst).is_err_and(|e| e.kind() == io::ErrorKind::NotFound);
    let mut dest = PathBuf::from(dst);
    if copier.x.unlink_dest_after_failed_open
        && copier.x.backup != BackupMode::None
        && src == dst
        && !new_dst
        && fs::metadata(dst).is_ok_and(|m| m.is_file())
    {
        dest = backup_name(Path::new(dst), copier.x.backup, &copier.x.suffix);
        copier.x.backup = BackupMode::None;
    }
    let ok = copier
        .copy(Path::new(src), &dest, if new_dst { -1 } else { 0 })
        .ok;
    copier.commit_top(Path::new(src), None);
    ok
}

/// `cp SOURCE... DIRECTORY`, with `--parents` creating the directories that
/// lead to each SOURCE.
fn copy_into(
    copier: &mut Copier<'_>,
    dir: &OsStr,
    sources: Vec<OsString>,
    parents: bool,
    strip_slashes: bool,
) -> bool {
    if sources.len() >= 2 {
        copier.track_destinations();
        copier.track_sources();
    }
    let mut ok = true;
    // GNU carries this from one operand to the next (make_dir_parents_private
    // does not always set it).
    let mut new_dst = false;
    for arg in sources {
        // Trailing slashes are meaningful only in the source names.
        let arg = if strip_slashes {
            strip_trailing_slashes(&arg)
        } else {
            arg
        };
        let (dst, base_offset) = if parents {
            file_name_concat(dir.as_bytes(), strip_trailing_slashes(&arg).as_bytes())
        } else {
            let last = last_component(arg.as_bytes());
            let mut base = &last[..base_len(last)];
            // `cp -R src/.. dest` copies into dest/., not dest/..
            if base == b".." {
                base = b".";
            }
            file_name_concat(dir.as_bytes(), base)
        };
        let mut attrs = Vec::new();
        if parents && !make_dir_parents(copier, &dst, base_offset, &mut attrs, &mut new_dst) {
            ok = false;
            continue;
        }
        let src = Path::new(&arg);
        ok &= copier.copy(src, &dst, i32::from(new_dst)).ok;
        copier.commit_top(src, None);
        if parents {
            ok &= re_protect(copier, &dst, &attrs);
        }
    }
    ok
}

/// An intermediate directory `--parents` may have to fix up afterwards.
struct DirAttr {
    /// The source directory's status.
    st: fs::Metadata,
    /// Mode to give the copy, if it must change after the copying.
    mode: u32,
    /// Where the directory's name ends in the destination name.
    end: usize,
    restore_mode: bool,
}

fn prefix(p: &Path, end: usize) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(&p.as_os_str().as_bytes()[..end]))
}

/// GNU `make_dir_parents_private`: creates the missing directories leading
/// to `dst` (whose source name starts at `src_offset`), giving each the
/// source directory's mode with permissions others could misuse withheld,
/// and lists those whose attributes `re_protect` sets afterwards.
fn make_dir_parents(
    copier: &Copier<'_>,
    dst: &Path,
    src_offset: usize,
    attrs: &mut Vec<DirAttr>,
    new_dst: &mut bool,
) -> bool {
    let x = &copier.x;
    let bytes = dst.as_os_str().as_bytes();
    // gnulib `dir_len`.
    let root = usize::from(bytes.first() == Some(&b'/'));
    let mut dirlen = bytes.len() - last_component(bytes).len();
    while dirlen > root && bytes[dirlen - 1] == b'/' {
        dirlen -= 1;
    }
    if dirlen <= src_offset {
        return true;
    }
    let dst_dir = prefix(dst, dirlen);
    match fs::metadata(&dst_dir) {
        Ok(m) if m.is_dir() => {
            *new_dst = false;
            return true;
        }
        Ok(_) => {
            eprintln!("cp: {} exists but is not a directory", qa(&dst_dir));
            return false;
        }
        Err(_) => {}
    }
    // A parent is missing: make each missing one, component by component.
    let mut pos = src_offset;
    while pos < bytes.len() && bytes[pos] == b'/' {
        pos += 1;
    }
    while let Some(rel) = bytes[pos..].iter().position(|&b| b == b'/') {
        let end = pos + rel;
        let dir = prefix(dst, end);
        let src = PathBuf::from(OsStr::from_bytes(&bytes[src_offset..end]));
        let stats = fs::metadata(&dir);
        let missing = stats.is_err();
        let mut attr_index = None;
        if missing || x.preserve_ownership || x.preserve_mode || x.preserve_timestamps {
            let st = match fs::metadata(&src) {
                Ok(m) if m.is_dir() => m,
                Ok(_) => {
                    let e = io::Error::from_raw_os_error(libc::ENOTDIR);
                    eprintln!(
                        "cp: failed to get attributes of {}: {}",
                        qa(&src),
                        gnu_io_error(&e)
                    );
                    return false;
                }
                Err(e) => {
                    eprintln!(
                        "cp: failed to get attributes of {}: {}",
                        qa(&src),
                        gnu_io_error(&e)
                    );
                    return false;
                }
            };
            let mode = st.mode();
            attrs.insert(
                0,
                DirAttr {
                    st,
                    mode,
                    end,
                    restore_mode: false,
                },
            );
            attr_index = Some(0);
        }
        match (&stats, attr_index) {
            (Err(_), Some(i)) => {
                *new_dst = true;
                let src_mode = attrs[i].st.mode();
                let mut omitted = src_mode
                    & if x.preserve_ownership {
                        S_IRWXG | S_IRWXO
                    } else if x.preserve_mode {
                        S_IWGRP | S_IWOTH
                    } else {
                        0
                    };
                let mkdir_mode = if x.explicit_no_preserve_mode {
                    S_IRWXUGO
                } else {
                    src_mode
                } & CHMOD_MODE_BITS
                    & !omitted;
                if let Err(e) = fs::DirBuilder::new().mode(mkdir_mode).create(&dir) {
                    eprintln!(
                        "cp: cannot make directory {}: {}",
                        qa(&dir),
                        gnu_io_error(&e)
                    );
                    return false;
                }
                if x.verbose {
                    // GNU prints both names as they are, unquoted.
                    let mut line = src.as_os_str().as_bytes().to_vec();
                    line.extend_from_slice(b" -> ");
                    line.extend_from_slice(dir.as_os_str().as_bytes());
                    line.push(b'\n');
                    let _ = io::Write::write_all(&mut dogfish_core::stdout::stdout(), &line);
                }
                let made = match fs::symlink_metadata(&dir) {
                    Ok(m) => m.mode(),
                    Err(e) => {
                        eprintln!(
                            "cp: failed to get attributes of {}: {}",
                            qa(&dir),
                            gnu_io_error(&e)
                        );
                        return false;
                    }
                };
                if !x.preserve_mode {
                    if omitted & !made != 0 {
                        omitted &= !copier.umask;
                    }
                    if omitted & !made != 0 || made & S_IRWXU != S_IRWXU {
                        attrs[i].mode = made | omitted;
                        attrs[i].restore_mode = true;
                    }
                }
                // Searchable and writable while its contents are copied.
                let accessible = made | S_IRWXU;
                if made != accessible
                    && let Err(e) = fs::set_permissions(
                        &dir,
                        fs::Permissions::from_mode(accessible & CHMOD_MODE_BITS),
                    )
                {
                    eprintln!(
                        "cp: setting permissions for {}: {}",
                        qa(&dir),
                        gnu_io_error(&e)
                    );
                    return false;
                }
            }
            (Ok(m), _) if !m.is_dir() => {
                eprintln!("cp: {} exists but is not a directory", qa(&dir));
                return false;
            }
            _ => *new_dst = false,
        }
        pos = end + 1;
        while pos < bytes.len() && bytes[pos] == b'/' {
            pos += 1;
        }
    }
    true
}

/// GNU `re_protect`: after copying with `--parents`, gives the intermediate
/// directories the source directories' attributes.
fn re_protect(copier: &Copier<'_>, dst: &Path, attrs: &[DirAttr]) -> bool {
    let x = &copier.x;
    for p in attrs {
        let dir = prefix(dst, p.end);
        if x.preserve_timestamps {
            let ns = |n: i64| u32::try_from(n).unwrap_or(0);
            if let Err(e) = set_file_times(
                &dir,
                Stamp::At(p.st.atime(), ns(p.st.atime_nsec())),
                Stamp::At(p.st.mtime(), ns(p.st.mtime_nsec())),
                true,
            ) {
                eprintln!(
                    "cp: failed to preserve times for {}: {}",
                    qa(&dir),
                    gnu_io_error(&e)
                );
                return false;
            }
        }
        if x.preserve_ownership
            && let Err(e) = std::os::unix::fs::lchown(&dir, Some(p.st.uid()), Some(p.st.gid()))
        {
            // SAFETY: geteuid has no preconditions and cannot fail.
            let privileged = unsafe { libc::geteuid() } == 0;
            if !(matches!(e.raw_os_error(), Some(libc::EPERM | libc::EINVAL)) && !privileged) {
                eprintln!(
                    "cp: failed to preserve ownership for {}: {}",
                    qa(&dir),
                    gnu_io_error(&e)
                );
                return false;
            }
            let _ = std::os::unix::fs::lchown(&dir, None, Some(p.st.gid()));
        }
        if x.preserve_mode {
            if let Err(e) = fs::set_permissions(
                &dir,
                fs::Permissions::from_mode(p.st.mode() & CHMOD_MODE_BITS),
            ) {
                eprintln!(
                    "cp: preserving permissions for {}: {}",
                    quote_locale(&dir),
                    gnu_io_error(&e)
                );
                return false;
            }
        } else if p.restore_mode
            && let Err(e) =
                fs::set_permissions(&dir, fs::Permissions::from_mode(p.mode & CHMOD_MODE_BITS))
        {
            eprintln!(
                "cp: failed to preserve permissions for {}: {}",
                qa(&dir),
                gnu_io_error(&e)
            );
            return false;
        }
    }
    true
}

/// cp's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("cp", CP_HELP, SPECS)
        .choices("backup", crate::common::backup::backup_words())
        .choices("reflink", ["auto", "always", "never"])
        .choices("sparse", ["never", "auto", "always"])
        .choices("update", ["all", "none", "older"])
        .choices("preserve", PRESERVE_ARGS.iter().map(|(w, _)| *w))
        .choices("no-preserve", PRESERVE_ARGS.iter().map(|(w, _)| *w))
}
