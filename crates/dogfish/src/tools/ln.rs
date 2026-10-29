//! `ln` — a port of GNU ln.c

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};

use anyhow::Result;
use dogfish_core::{DogfishContext, gnu_io_error, outln};

use crate::common::backup::{BackupMode, backup_mode, backup_name, backup_suffix};
use crate::common::canonicalize::{CanMode, canonicalize_mode, relpath};
use crate::common::copy::{force_linkat, force_symlinkat};
use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy, print_help, print_version, usage_error,
    utf8_operands,
};
use crate::common::path::{
    base_len, c_path, dir_name, file_name_concat, last_component, same_inode, same_name,
};
use crate::common::prompt::ask;
use crate::common::quote::{qa, qf};

const LN_HELP: &str = "\
Usage: ln [OPTION]... [-T] TARGET LINK_NAME
  or:  ln [OPTION]... TARGET
  or:  ln [OPTION]... TARGET... DIRECTORY
  or:  ln [OPTION]... -t DIRECTORY TARGET...
Create links to each TARGET; hard links by default, symbolic links with -s.

      --backup[=CONTROL]      make a backup of each existing destination file
  -b                          like --backup but does not accept an argument
  -d, -F, --directory         (hard links to directories are not allowed)
  -f, --force                 remove existing destination files
  -i, --interactive           prompt whether to remove destinations
  -L, --logical               dereference TARGETs that are symbolic links
  -n, --no-dereference        treat LINK_NAME as a normal file if
                                it is a symbolic link to a directory
  -P, --physical              make hard links directly to symbolic links
  -r, --relative              with -s, create links relative to link location
  -s, --symbolic              make symbolic links instead of hard links
  -S, --suffix=SUFFIX         override the usual backup suffix
  -t, --target-directory=DIRECTORY  specify the DIRECTORY in which to create
                                the links
  -T, --no-target-directory   treat LINK_NAME as a normal file always
  -v, --verbose               print name of each linked file
      --help        display this help and exit
      --version     output version information and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum LnOpt {
    Backup,
    BackupArg,
    Directory,
    Force,
    Interactive,
    Logical,
    NoDeref,
    Physical,
    Relative,
    Symbolic,
    Suffix,
    TargetDir,
    NoTargetDir,
    Verbose,
    Help,
    Version,
}

/// GNU's `long_options` and `"bdfinrst:vFLPS:T"`.
const SPECS: &[OptSpec<LnOpt>] = &[
    OptSpec::long("backup", HasArg::Optional, LnOpt::BackupArg),
    OptSpec::both(b'F', "directory", HasArg::No, LnOpt::Directory),
    OptSpec::both(b'n', "no-dereference", HasArg::No, LnOpt::NoDeref),
    OptSpec::both(b'T', "no-target-directory", HasArg::No, LnOpt::NoTargetDir),
    OptSpec::both(b'f', "force", HasArg::No, LnOpt::Force),
    OptSpec::both(b'i', "interactive", HasArg::No, LnOpt::Interactive),
    OptSpec::both(b'S', "suffix", HasArg::Required, LnOpt::Suffix),
    OptSpec::both(b't', "target-directory", HasArg::Required, LnOpt::TargetDir),
    OptSpec::both(b'L', "logical", HasArg::No, LnOpt::Logical),
    OptSpec::both(b'P', "physical", HasArg::No, LnOpt::Physical),
    OptSpec::both(b'r', "relative", HasArg::No, LnOpt::Relative),
    OptSpec::both(b's', "symbolic", HasArg::No, LnOpt::Symbolic),
    OptSpec::both(b'v', "verbose", HasArg::No, LnOpt::Verbose),
    OptSpec::long("help", HasArg::No, LnOpt::Help),
    OptSpec::long("version", HasArg::No, LnOpt::Version),
    OptSpec::short(b'b', HasArg::No, LnOpt::Backup),
    OptSpec::short(b'd', HasArg::No, LnOpt::Directory),
];

struct LnOpts {
    backup: BackupMode,
    suffix: String,
    force: bool,
    interactive: bool,
    logical: bool,
    relative: bool,
    symbolic: bool,
    verbose: bool,
    /// `-d`: hard links to directories may be attempted.
    hard_dir_link: bool,
}

/// State for one `ln` invocation (ln.c's globals).
struct Linker<'a> {
    o: LnOpts,
    ctx: &'a DogfishContext,
    /// The hard links made so far (GNU's `dest_set`), to refuse replacing one
    /// with another in the same run.
    dest_set: Option<HashMap<(u64, u64), Vec<PathBuf>>>,
}

pub fn run_ln(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    use LnOpt::*;
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    let mut o = LnOpts {
        backup: BackupMode::None,
        suffix: String::new(),
        force: false,
        interactive: false,
        logical: false,
        relative: false,
        symbolic: false,
        verbose: false,
        hard_dir_link: false,
    };
    let (mut want_backup, mut backup_arg, mut suffix) = (false, None, None);
    let (mut no_deref, mut target_dir, mut no_target_dir) = (false, None::<String>, false);
    while let Some(opt) = opts.next() {
        let (opt, v) = match opt {
            Ok(o) => o,
            Err(e) => return Ok(getopt_error("ln", &e)),
        };
        let v = v.as_deref().map(lossy);
        match opt {
            Help => return Ok(print_help(LN_HELP)),
            Version => return Ok(print_version("ln")),
            Backup => want_backup = true,
            BackupArg => {
                want_backup = true;
                backup_arg = v;
            }
            Directory => o.hard_dir_link = true,
            Force => {
                o.force = true;
                o.interactive = false;
            }
            Interactive => {
                o.force = false;
                o.interactive = true;
            }
            Logical => o.logical = true,
            Physical => o.logical = false,
            NoDeref => no_deref = true,
            Relative => o.relative = true,
            Symbolic => o.symbolic = true,
            Suffix => {
                want_backup = true;
                suffix = v;
            }
            TargetDir => {
                // GNU checks -t while parsing, so this comes before any
                // diagnostic for a later option.
                if target_dir.is_some() {
                    eprintln!("ln: multiple target directories specified");
                    return Ok(1);
                }
                let d = v.unwrap_or_default();
                match fs::metadata(&d) {
                    Err(e) => {
                        eprintln!("ln: failed to access {}: {}", qa(&d), gnu_io_error(&e));
                        return Ok(1);
                    }
                    Ok(m) if !m.is_dir() => {
                        eprintln!("ln: target {} is not a directory", qa(&d));
                        return Ok(1);
                    }
                    Ok(_) => {}
                }
                target_dir = Some(d);
            }
            NoTargetDir => no_target_dir = true,
            Verbose => o.verbose = true,
        }
    }
    let mut files = match utf8_operands("ln", opts.operands()) {
        Ok(o) => o,
        Err(code) => return Ok(code),
    };
    if files.is_empty() {
        return Ok(usage_error("ln", "missing file operand"));
    }
    if o.relative && !o.symbolic {
        eprintln!("ln: cannot do --relative without --symbolic");
        return Ok(1);
    }

    // `None`: no attempt at the link yet.
    let mut link_res: Option<io::Result<()>> = None;
    if no_target_dir {
        if target_dir.is_some() {
            eprintln!("ln: cannot combine --target-directory and --no-target-directory");
            return Ok(1);
        }
        if files.len() != 2 {
            let msg = if files.len() < 2 {
                format!("missing destination file operand after {}", qa(&files[0]))
            } else {
                format!("extra operand {}", qa(&files[2]))
            };
            return Ok(usage_error("ln", msg));
        }
    } else if files.len() < 2 && target_dir.is_none() {
        target_dir = Some(".".to_string());
    } else {
        // With two operands, try the link first: DEST is a directory only if
        // that fails.
        if files.len() == 2 && target_dir.is_none() {
            link_res = atomic_link(&o, Path::new(&files[0]), Path::new(&files[1]));
        }
        let retry_as_dir = match &link_res {
            None => true,
            Some(Ok(())) => false,
            Some(Err(e)) => matches!(
                e.raw_os_error(),
                Some(libc::EEXIST | libc::ENOTDIR | libc::EINVAL)
            ),
        };
        if retry_as_dir {
            let d = target_dir
                .clone()
                .unwrap_or_else(|| files[files.len() - 1].clone());
            match open_dir(Path::new(&d), no_deref) {
                Ok(()) => {
                    if target_dir.is_none() {
                        files.pop();
                    }
                    target_dir = Some(d);
                }
                Err(e) => {
                    if !(files.len() == 2 && target_dir.is_none()) {
                        eprintln!("ln: target {}: {}", qa(&d), gnu_io_error(&e));
                        return Ok(1);
                    }
                }
            }
        }
    }

    if want_backup {
        o.backup = match backup_mode("ln", backup_arg.as_deref()) {
            Ok(m) => m,
            Err(code) => return Ok(code),
        };
    }
    o.suffix = backup_suffix(suffix);

    let mut ln = Linker {
        o,
        ctx,
        dest_set: None,
    };
    let ok = match target_dir {
        Some(dir) => {
            // Only replacing one just-made hard link with another loses data.
            if files.len() >= 2
                && ln.o.force
                && !ln.o.symbolic
                && ln.o.backup != BackupMode::Numbered
            {
                ln.dest_set = Some(HashMap::new());
            }
            let mut ok = true;
            for src in &files {
                let last = last_component(src.as_bytes());
                let (dest, _) = file_name_concat(dir.as_bytes(), &last[..base_len(last)]);
                ok &= ln.do_link(Path::new(src), &dest, None);
            }
            ok
        }
        None => ln.do_link(Path::new(&files[0]), Path::new(&files[1]), link_res),
    };
    Ok(if ok { 0 } else { 1 })
}

/// Opens `d` as a directory (GNU's `O_PATHSEARCH | O_DIRECTORY`), not
/// following a final symlink with `-n`.
fn open_dir(d: &Path, no_deref: bool) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    let base = libc::O_PATH;
    #[cfg(not(target_os = "linux"))]
    let base = libc::O_RDONLY;
    let flags =
        base | libc::O_DIRECTORY | libc::O_CLOEXEC | if no_deref { libc::O_NOFOLLOW } else { 0 };
    let c = c_path(d)?;
    let fd = unsafe { libc::open(c.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe { libc::close(fd) };
    Ok(())
}

/// GNU `atomic_link`: the link, when it can be made in one step (the
/// destination does not exist and no `-r` conversion is needed). `None`
/// when it cannot be tried that way.
fn atomic_link(o: &LnOpts, src: &Path, dest: &Path) -> Option<io::Result<()>> {
    if o.symbolic {
        (!o.relative).then(|| symlink(src, dest))
    } else {
        Some(link_at(src, dest, o.logical))
    }
}

fn link_at(src: &Path, dest: &Path, follow: bool) -> io::Result<()> {
    let (s, d) = (c_path(src)?, c_path(dest)?);
    let flags = if follow { libc::AT_SYMLINK_FOLLOW } else { 0 };
    if unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            s.as_ptr(),
            libc::AT_FDCWD,
            d.as_ptr(),
            flags,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// GNU `convert_abs_rel`: `from` relative to the directory that will hold
/// `target`. Only that directory is resolved: `target` itself may be an
/// existing symlink that is about to be replaced.
fn convert_abs_rel(from: &Path, target: &Path) -> PathBuf {
    let dir = dir_name(target);
    match (
        canonicalize_mode(&dir, CanMode::Missing, false),
        canonicalize_mode(from, CanMode::Missing, false),
    ) {
        (Ok(realdest), Ok(realfrom)) => relpath(&realfrom, &realdest),
        _ => from.to_path_buf(),
    }
}

impl Linker<'_> {
    fn seen(&self, dest: &Path, sb: &fs::Metadata) -> bool {
        self.dest_set.as_ref().is_some_and(|set| {
            set.get(&(sb.dev(), sb.ino()))
                .is_some_and(|names| names.iter().any(|n| same_name(n, dest)))
        })
    }

    fn record(&mut self, dest: &Path, sb: &fs::Metadata) {
        if let Some(set) = &mut self.dest_set {
            set.entry((sb.dev(), sb.ino()))
                .or_default()
                .push(dest.to_path_buf());
        }
    }

    /// GNU `do_link`: links `source` at `dest`. `tried` is the result of an
    /// earlier attempt at the plain link.
    fn do_link(&mut self, source: &Path, dest: &Path, tried: Option<io::Result<()>>) -> bool {
        let o = &self.o;
        let symbolic = o.symbolic;
        let mut link_res = tried.or_else(|| atomic_link(o, source, dest));
        let stat_source = |follow: bool| {
            if follow {
                fs::metadata(source)
            } else {
                fs::symlink_metadata(source)
            }
        };
        let mut source_stats = None;
        if (!matches!(link_res, Some(Ok(()))) || self.dest_set.is_some()) && !symbolic {
            match stat_source(o.logical) {
                Ok(m) => source_stats = Some(m),
                Err(e) => {
                    eprintln!("ln: failed to access {}: {}", qa(source), gnu_io_error(&e));
                    return false;
                }
            }
        }

        let mut source: PathBuf = source.to_path_buf();
        let mut backup: Option<PathBuf> = None;
        if !matches!(link_res, Some(Ok(()))) {
            if !symbolic
                && !o.hard_dir_link
                && source_stats.as_ref().is_some_and(fs::Metadata::is_dir)
            {
                eprintln!("ln: {}: hard link not allowed for directory", qf(&source));
                return false;
            }
            if o.relative {
                source = convert_abs_rel(&source, dest);
            }
            let mut force = o.force || o.interactive || o.backup != BackupMode::None;
            if force {
                match fs::symlink_metadata(dest) {
                    Err(e) => {
                        if e.kind() != io::ErrorKind::NotFound {
                            eprintln!("ln: failed to access {}: {}", qa(dest), gnu_io_error(&e));
                            return false;
                        }
                        force = false;
                    }
                    Ok(ds) if ds.is_dir() => {
                        eprintln!("ln: {}: cannot overwrite directory", qf(dest));
                        return false;
                    }
                    Ok(ds) if self.seen(dest, &ds) => {
                        eprintln!(
                            "ln: will not overwrite just-created {} with {}",
                            qa(dest),
                            qa(&source)
                        );
                        return false;
                    }
                    Ok(ds) => {
                        // Removing DEST must not remove SOURCE: with backups
                        // that matters for hard links, otherwise for -f.
                        let check = if o.backup != BackupMode::None {
                            !symbolic
                        } else {
                            o.force
                        };
                        if check {
                            let ss = match &source_stats {
                                Some(s) => Some(s.clone()),
                                None => fs::metadata(&source).ok(),
                            };
                            if ss.is_some_and(|s| {
                                same_inode(&s, &ds) && (s.nlink() == 1 || same_name(&source, dest))
                            }) {
                                eprintln!("ln: {} and {} are the same file", qa(&source), qa(dest));
                                return false;
                            }
                        }
                        // Dogfish also refuses to replace a file with a
                        // symlink to itself, which would lose its contents.
                        let self_loop = symbolic
                            && o.backup == BackupMode::None
                            && !ds.file_type().is_symlink()
                            && fs::metadata(dir_name(dest).join(&source))
                                .is_ok_and(|t| same_inode(&t, &ds));
                        if self_loop && o.force {
                            eprintln!("ln: {} and {} are the same file", qa(&source), qa(dest));
                            return false;
                        }
                        let unset_or_exists = match &link_res {
                            None => true,
                            Some(Err(e)) => e.raw_os_error() == Some(libc::EEXIST),
                            Some(Ok(())) => false,
                        };
                        if unset_or_exists {
                            if o.interactive {
                                let yes = ask("ln", &format!("replace {}? ", qa(dest)));
                                if !yes || self_loop {
                                    return yes;
                                }
                            }
                            if o.backup != BackupMode::None {
                                let name = backup_name(dest, o.backup, &o.suffix);
                                match fs::rename(dest, &name) {
                                    Ok(()) => backup = Some(name),
                                    Err(e) if e.kind() == io::ErrorKind::NotFound => force = false,
                                    Err(e) => {
                                        eprintln!(
                                            "ln: cannot backup {}: {}",
                                            qa(dest),
                                            gnu_io_error(&e)
                                        );
                                        return false;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Try to link before removing DEST, so a failing link never costs
            // DEST. DEST is replaced even after a backup: when it and the
            // backup were links to one file, the rename left it in place.
            let res = if symbolic {
                force_symlinkat(&source, dest, force, link_res.take())
            } else {
                force_linkat(&source, dest, o.logical, force, link_res.take())
            };
            link_res = Some(res.map(|_| ()));
        }

        match link_res {
            Some(Ok(())) | None => {
                if !symbolic && let Some(s) = &source_stats {
                    let s = s.clone();
                    self.record(dest, &s);
                }
                self.ctx.telemetry.add_items(1);
                if self.o.verbose {
                    let arrow = if symbolic { "->" } else { "=>" };
                    match &backup {
                        Some(b) => outln!("{} ~ {} {arrow} {}", qa(b), qa(dest), qa(&source)),
                        None => outln!("{} {arrow} {}", qa(dest), qa(&source)),
                    }
                }
                true
            }
            Some(Err(e)) => {
                let errno = e.raw_os_error().unwrap_or(0);
                let msg = if symbolic {
                    if errno != libc::ENAMETOOLONG && !source.as_os_str().is_empty() {
                        format!("failed to create symbolic link {}", qa(dest))
                    } else {
                        format!(
                            "failed to create symbolic link {} -> {}",
                            qa(dest),
                            qa(&source)
                        )
                    }
                } else if errno == libc::EMLINK {
                    format!("failed to create hard link to {}", qa(&source))
                } else if [libc::EDQUOT, libc::EEXIST, libc::ENOSPC, libc::EROFS].contains(&errno) {
                    format!("failed to create hard link {}", qa(dest))
                } else {
                    format!("failed to create hard link {} => {}", qa(dest), qa(&source))
                };
                eprintln!("ln: {msg}: {}", gnu_io_error(&e));
                if let Some(b) = backup
                    && let Err(e) = fs::rename(&b, dest)
                {
                    eprintln!("ln: cannot un-backup {}: {}", qa(dest), gnu_io_error(&e));
                }
                false
            }
        }
    }
}
