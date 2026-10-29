//! chmod — octal and full symbolic modes, -R, -c/-v/-f, --reference

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;

use anyhow::Result;
use dogfish_core::{DogfishContext, gnu_io_error, outln};

use crate::common::dirfd::{EntryType, Lost, Names, Parent, WalkPath, fts_root_name};
use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy, print_help, print_version, usage_error,
};
use crate::common::mode::{
    CHMOD_MODE_BITS, ModeChange, mode_adjust, mode_compile, process_umask, strmode9,
};
use crate::common::preserve_root::{DevIno, is_root_dev_ino, refuse_root, root_dev_ino};
use crate::common::quote::{qa, qf, quote_locale};

const CHMOD_HELP: &str = "\
Usage: chmod [OPTION]... MODE[,MODE]... FILE...
  or:  chmod [OPTION]... OCTAL-MODE FILE...
  or:  chmod [OPTION]... --reference=RFILE FILE...
Change the mode of each FILE to MODE.
With --reference, change the mode of each FILE to that of RFILE.

  -c, --changes          like verbose but report only when a change is made
  -f, --silent, --quiet  suppress most error messages
  -v, --verbose          output a diagnostic for every file processed
      --no-preserve-root  do not treat '/' specially (the default)
      --preserve-root    fail to operate recursively on '/'
      --reference=RFILE  use RFILE's mode instead of specifying MODE values
  -R, --recursive        change files and directories recursively
      --help        display this help and exit
      --version     output version information and exit

Each MODE is of the form '[ugoa]*([-+=]([rwxXst]*|[ugo]))+|[-+=][0-7]+'.
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChmodOpt {
    Changes,
    Quiet,
    Verbose,
    NoPreserveRoot,
    PreserveRoot,
    Reference,
    Recursive,
    /// A mode that looks like an option (`-w`, `-rx`, `-0`).
    Mode,
    Help,
    Version,
}

/// A mode character that GNU's option string accepts as an option with an
/// optional argument, so that `chmod -w FILE` works.
const fn mode_char(c: u8) -> OptSpec<ChmodOpt> {
    OptSpec::short(c, HasArg::Optional, ChmodOpt::Mode)
}

/// GNU's `long_options` and its option string
/// `"Rcfvr::w::x::X::s::t::u::g::o::a::,::+::=::0::1::2::3::4::5::6::7::"`.
const SPECS: &[OptSpec<ChmodOpt>] = &[
    OptSpec::both(b'c', "changes", HasArg::No, ChmodOpt::Changes),
    OptSpec::both(b'R', "recursive", HasArg::No, ChmodOpt::Recursive),
    OptSpec::long("no-preserve-root", HasArg::No, ChmodOpt::NoPreserveRoot),
    OptSpec::long("preserve-root", HasArg::No, ChmodOpt::PreserveRoot),
    OptSpec::both(b'f', "quiet", HasArg::No, ChmodOpt::Quiet),
    OptSpec::long("reference", HasArg::Required, ChmodOpt::Reference),
    OptSpec::long("silent", HasArg::No, ChmodOpt::Quiet),
    OptSpec::both(b'v', "verbose", HasArg::No, ChmodOpt::Verbose),
    OptSpec::long("help", HasArg::No, ChmodOpt::Help),
    OptSpec::long("version", HasArg::No, ChmodOpt::Version),
    mode_char(b'r'),
    mode_char(b'w'),
    mode_char(b'x'),
    mode_char(b'X'),
    mode_char(b's'),
    mode_char(b't'),
    mode_char(b'u'),
    mode_char(b'g'),
    mode_char(b'o'),
    mode_char(b'a'),
    mode_char(b','),
    mode_char(b'+'),
    mode_char(b'='),
    mode_char(b'0'),
    mode_char(b'1'),
    mode_char(b'2'),
    mode_char(b'3'),
    mode_char(b'4'),
    mode_char(b'5'),
    mode_char(b'6'),
    mode_char(b'7'),
];

struct ChmodRun {
    changes: Option<Vec<ModeChange>>,
    reference_mode: u32,
    umask: u32,
    recursive: bool,
    report_changes: bool,
    verbose: bool,
    quiet: bool,
    diagnose_surprises: bool,
    /// `/`'s identity under `-R --preserve-root` (GNU `root_dev_ino`).
    root_dev_ino: Option<DevIno>,
}

pub fn run_chmod(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    use ChmodOpt::*;
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    let mut run = ChmodRun {
        changes: None,
        reference_mode: 0,
        umask: process_umask(),
        recursive: false,
        report_changes: false,
        verbose: false,
        quiet: false,
        diagnose_surprises: false,
        root_dev_ino: None,
    };
    let (mut preserve_root, mut reference) = (false, None);
    // Raw bytes, so a diagnostic quotes the mode exactly as given.
    let mut mode_pieces: Vec<OsString> = Vec::new();
    while let Some(opt) = opts.next() {
        let (o, v) = match opt {
            Ok(o) => o,
            Err(e) => return Ok(getopt_error("chmod", &e)),
        };
        match o {
            Changes => run.report_changes = true,
            Quiet => run.quiet = true,
            Verbose => run.verbose = true,
            NoPreserveRoot => preserve_root = false,
            PreserveRoot => preserve_root = true,
            Reference => reference = v,
            Recursive => run.recursive = true,
            // Like GNU, the whole argument the mode character came from is
            // the mode (`-rwx`), joined to earlier ones with a comma.
            Mode => {
                mode_pieces.push(opts.last_word().unwrap_or_default().to_os_string());
                run.diagnose_surprises = true;
            }
            Help => return Ok(print_help(CHMOD_HELP)),
            Version => return Ok(print_version("chmod")),
        }
    }
    let mut files = opts.operands();
    let mut mode = (!mode_pieces.is_empty()).then(|| mode_pieces.join(OsStr::new(",")));
    if reference.is_none() && mode.is_none() {
        if files.is_empty() {
            return Ok(usage_error("chmod", "missing operand"));
        }
        mode = Some(files.remove(0));
    }
    if files.is_empty() {
        return Ok(match (&mode, &reference) {
            (Some(m), None) => usage_error(
                "chmod",
                format!("missing operand after {}", quote_locale(m)),
            ),
            _ => usage_error("chmod", "missing operand"),
        });
    }
    if let Some(r) = &reference {
        match fs::metadata(r) {
            Ok(m) => run.reference_mode = m.mode() & CHMOD_MODE_BITS,
            Err(e) => {
                eprintln!(
                    "chmod: failed to get attributes of {}: {}",
                    qa(r),
                    gnu_io_error(&e)
                );
                return Ok(1);
            }
        }
    } else if let Some(m) = &mode {
        match mode_compile(&lossy(m)) {
            Some(c) => run.changes = Some(c),
            None => {
                return Ok(usage_error(
                    "chmod",
                    format!("invalid mode: {}", quote_locale(m)),
                ));
            }
        }
    }

    if run.recursive && preserve_root {
        match root_dev_ino() {
            Ok(r) => run.root_dev_ino = Some(r),
            Err(e) => {
                eprintln!(
                    "chmod: failed to get attributes of '/': {}",
                    gnu_io_error(&e)
                );
                return Ok(1);
            }
        }
    }

    let mut ok = true;
    for f in &files {
        // Named as fts names it: `d//` is `d/`, in messages too.
        let f = fts_root_name(f.as_bytes());
        let Ok(c_path) = CString::new(f) else {
            continue;
        };
        let mut path = WalkPath::new(f);
        match chmod_path(&mut Parent::Cwd, &c_path, &mut path, 0, &run, ctx) {
            Ok(o) => ok &= o,
            Err(Lost) => {
                ok = false;
                break;
            }
        }
    }
    Ok(if ok { 0 } else { 1 })
}

/// Changes the mode of `name` in `parent` (printed as `path`), then with `-R`
/// of everything below it, each entry relative to its open directory. `depth`
/// 0 is an operand, whose symlinks are followed; below, symlinks are left
/// alone.
fn chmod_path(
    parent: &mut Parent,
    name: &CStr,
    path: &mut WalkPath,
    depth: usize,
    run: &ChmodRun,
    ctx: &DogfishContext,
) -> Result<bool, Lost> {
    let top_level = depth == 0;
    let at = parent.at(name);
    let lstat = match at.stat(false) {
        Ok(st) => st,
        Err(e) => {
            if !run.quiet {
                eprintln!(
                    "chmod: cannot access {}: {}",
                    qa(path.as_path()),
                    gnu_io_error(&e)
                );
            }
            if run.verbose {
                outln!("{} could not be accessed", qa(path.as_path()));
            }
            return Ok(false);
        }
    };
    let st = if EntryType::of(&lstat) == EntryType::Symlink {
        if !top_level {
            // During -R traversal, symbolic links are not followed or changed.
            if run.verbose {
                outln!(
                    "neither symbolic link {} nor referent has been changed",
                    qa(path.as_path())
                );
            }
            return Ok(true);
        }
        match at.stat(true) {
            Ok(st) => st,
            Err(_) => {
                if !run.quiet {
                    eprintln!(
                        "chmod: cannot operate on dangling symlink {}",
                        qa(path.as_path())
                    );
                }
                if run.verbose {
                    outln!("{} could not be accessed", qa(path.as_path()));
                }
                return Ok(false);
            }
        }
    } else {
        lstat
    };
    // GNU `process_file`: checked on the stat the walk already made, so a
    // command-line symlink to `/` (followed, as FTS_COMFOLLOW does) is refused
    // too, and no directory below an operand can be `/` unnoticed.
    if is_root_dev_ino(run.root_dev_ino, DevIno::of_stat(&st)) {
        refuse_root("chmod", path.as_path());
        return Ok(false);
    }

    #[allow(clippy::unnecessary_cast)] // `mode_t` is `u16` on macOS
    let old = st.st_mode as u32 & CHMOD_MODE_BITS;
    let is_dir = EntryType::of(&st) == EntryType::Dir;
    let new = match &run.changes {
        Some(c) => mode_adjust(old, is_dir, run.umask, c).0,
        None => run.reference_mode,
    };
    let mut ok = true;
    let applied = match at.chmod(new) {
        Ok(()) => {
            ctx.telemetry.add_items(1);
            true
        }
        Err(e) => {
            if !run.quiet {
                eprintln!(
                    "chmod: changing permissions of {}: {}",
                    qa(path.as_path()),
                    gnu_io_error(&e)
                );
            }
            ok = false;
            false
        }
    };
    let changed = applied && old != new;
    if run.verbose || (run.report_changes && changed) {
        let (o, n) = (strmode9(old), strmode9(new));
        let p = qa(path.as_path());
        if !applied {
            outln!("failed to change mode of {p} from {old:04o} ({o}) to {new:04o} ({n})");
        } else if changed {
            outln!("mode of {p} changed from {old:04o} ({o}) to {new:04o} ({n})");
        } else {
            outln!("mode of {p} retained as {new:04o} ({n})");
        }
    }
    if applied
        && run.diagnose_surprises
        && let Some(c) = &run.changes
    {
        let naive = mode_adjust(old, is_dir, 0, c).0;
        if new & !naive != 0 {
            eprintln!(
                "chmod: {}: new permissions are {}, not {}",
                qf(path.as_path()),
                strmode9(new),
                strmode9(naive)
            );
            ok = false;
        }
    }

    if run.recursive && is_dir {
        ok &= chmod_dir(parent, name, path, depth, run, ctx)?;
    }
    Ok(ok)
}

/// With `-R`, changes the modes of the entries of directory `name` in
/// `parent` (printed as `path`), `depth` levels below an operand. A
/// directory deep enough lets go of `parent` meanwhile and gets it back
/// through its own `..`, so every entry is reached by one name from its open
/// directory, at any depth.
fn chmod_dir(
    parent: &mut Parent,
    name: &CStr,
    path: &mut WalkPath,
    depth: usize,
    run: &ChmodRun,
    ctx: &DogfishContext,
) -> Result<bool, Lost> {
    let mut names = Names::default();
    // An operand that is a symlink to a directory is followed.
    let this = parent
        .at(name)
        .open_dir(depth > 0)
        .and_then(|d| d.read_names(&mut names).map(|()| d))
        .and_then(|d| Parent::dir(d, depth));
    let mut this = match this {
        Ok(this) => this,
        Err(e) => {
            if !run.quiet {
                eprintln!(
                    "chmod: cannot read directory {}: {}",
                    qa(path.as_path()),
                    gnu_io_error(&e)
                );
            }
            return Ok(false);
        }
    };
    let released = parent.release_deep(depth);
    let mut ok = true;
    for i in 0..names.len() {
        let (child, _, _) = names.get(i);
        let mark = path.push(child.to_bytes());
        let status = chmod_path(&mut this, child, path, depth + 1, run, ctx);
        path.truncate(mark);
        ok &= status?;
    }
    if released {
        let got_back = match this.open_dir() {
            Some(own) => parent.reacquire(own),
            None => Err(std::io::Error::from_raw_os_error(libc::EBADF)),
        };
        if let Err(e) = got_back {
            if !run.quiet {
                eprintln!("chmod: fts_read failed: {}", gnu_io_error(&e));
            }
            return Err(Lost);
        }
    }
    Ok(ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;

    // The --preserve-root walk is tested against a temporary directory standing
    // in for `/`, never against the real root (AGENTS.md).
    fn chmod_with_root(p: &Path, root: Option<DevIno>) -> bool {
        let run = ChmodRun {
            changes: None,
            reference_mode: 0o700,
            umask: 0o022,
            recursive: true,
            report_changes: false,
            verbose: false,
            quiet: true,
            diagnose_surprises: false,
            root_dev_ino: root,
        };
        let c_path = CString::new(p.as_os_str().as_bytes()).unwrap();
        let mut path = WalkPath::new(p.as_os_str().as_bytes());
        let ctx = DogfishContext::new("chmod");
        chmod_path(&mut Parent::Cwd, &c_path, &mut path, 0, &run, &ctx).unwrap_or(false)
    }

    #[test]
    fn preserve_root_refuses_a_followed_command_line_symlink_to_root() {
        let tmp = tempfile::tempdir().unwrap();
        let fake_root = tmp.path().join("root");
        fs::create_dir(&fake_root).unwrap();
        fs::set_permissions(&fake_root, fs::Permissions::from_mode(0o755)).unwrap();
        let inner = fake_root.join("f");
        fs::write(&inner, "x").unwrap();
        fs::set_permissions(&inner, fs::Permissions::from_mode(0o644)).unwrap();
        let link = tmp.path().join("link");
        symlink(&fake_root, &link).unwrap();
        let above = tmp.path().join("above");
        fs::create_dir(&above).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().mode() & 0o7777;
        let root = Some(DevIno::of(&fs::metadata(&fake_root).unwrap()));

        // Through the link (followed on the command line, like FTS_COMFOLLOW)
        // and named directly: refused, nothing changed.
        assert!(!chmod_with_root(&link, root));
        assert!(!chmod_with_root(&fake_root, root));
        assert_eq!((mode(&fake_root), mode(&inner)), (0o755, 0o644));
        // A directory reached below an operand is checked too.
        assert!(!chmod_with_root(tmp.path(), root));
        assert_eq!((mode(&fake_root), mode(&inner)), (0o755, 0o644));
        // Elsewhere the walk is unaffected.
        assert!(chmod_with_root(&above, root));

        // Without --preserve-root the same walk goes ahead.
        assert!(chmod_with_root(&link, None));
        assert_eq!((mode(&fake_root), mode(&inner)), (0o700, 0o700));
    }

    #[test]
    fn a_directory_moved_while_let_go_of_stops_the_walk() {
        use crate::common::dirfd::{At, MAX_OPEN_DIRS};
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("f"), "x").unwrap();
        let run = ChmodRun {
            changes: None,
            reference_mode: 0o700,
            umask: 0o022,
            recursive: true,
            report_changes: false,
            verbose: false,
            quiet: true,
            diagnose_surprises: false,
            root_dev_ino: None,
        };
        let ctx = DogfishContext::new("chmod");
        let top = CString::new(tmp.path().as_os_str().as_bytes()).unwrap();
        let fd = At::cwd(&top).open_dir(false).unwrap();
        // Deep enough to be let go of while `sub` is read; its `..` then
        // turns out to be another directory than the one let go of, as if
        // it had been moved meanwhile.
        let meta = fs::metadata(tmp.path()).unwrap();
        let mut parent = Parent::Dir {
            fd: Some(fd),
            id: Some((meta.dev(), meta.ino() + 1)),
        };
        let mut path = WalkPath::new(sub.as_os_str().as_bytes());
        let walked = chmod_dir(&mut parent, c"sub", &mut path, MAX_OPEN_DIRS, &run, &ctx);
        assert!(walked.is_err());
        // What was reached before is changed; the directory is not got back.
        assert_eq!(fs::metadata(sub.join("f")).unwrap().mode() & 0o7777, 0o700);
        assert!(parent.open_dir().is_none());
    }
}
