//! chown — `USER[:GROUP]`, `:GROUP`, `USER:`, numeric ids, -R/-h/-H/-L/-P, --from

use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::Result;
use dogfish_core::{DogfishContext, gnu_io_error, outln};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, print_help, print_version, usage_error,
};
use crate::common::path::dir_names;
use crate::common::preserve_root::{DevIno, is_root_dev_ino, refuse_root, root_dev_ino};
use crate::common::quote::{qa, quote_locale};
use crate::common::users::{get_groupname_by_gid, get_username_by_uid};

const CHOWN_HELP: &str = "\
Usage: chown [OPTION]... [OWNER][:[GROUP]] FILE...
  or:  chown [OPTION]... --reference=RFILE FILE...
Change the owner and/or group of each FILE to OWNER and/or GROUP.
With --reference, change the owner and group of each FILE to those of RFILE.

  -c, --changes          like verbose but report only when a change is made
  -f, --silent, --quiet  suppress most error messages
  -v, --verbose          output a diagnostic for every file processed
      --dereference      affect the referent of each symbolic link (this is
                         the default), rather than the symbolic link itself
  -h, --no-dereference   affect symbolic links instead of any referenced file
      --from=CURRENT_OWNER:CURRENT_GROUP
                         change the owner and/or group of each file only if
                         its current owner and/or group match those specified
      --no-preserve-root  do not treat '/' specially (the default)
      --preserve-root    fail to operate recursively on '/'
      --reference=RFILE  use RFILE's owner and group rather than specifying
                         OWNER:GROUP values
  -R, --recursive        operate on files and directories recursively

The following options modify how a hierarchy is traversed when the -R
option is also specified.  If more than one is specified, only the final
one takes effect.

  -H                     if a command line argument is a symbolic link
                         to a directory, traverse it
  -L                     traverse every symbolic link to a directory
                         encountered
  -P                     do not traverse any symbolic links (default)

      --help        display this help and exit
      --version     output version information and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChownOpt {
    Changes,
    Quiet,
    Verbose,
    Deref,
    NoDeref,
    From,
    NoPreserveRoot,
    PreserveRoot,
    Reference,
    Recursive,
    TraverseCmdline,
    TraverseAll,
    TraverseNone,
    Help,
    Version,
}

/// GNU's `long_options` and `"HLPRcfhv"`.
const SPECS: &[OptSpec<ChownOpt>] = &[
    OptSpec::both(b'R', "recursive", HasArg::No, ChownOpt::Recursive),
    OptSpec::both(b'c', "changes", HasArg::No, ChownOpt::Changes),
    OptSpec::long("dereference", HasArg::No, ChownOpt::Deref),
    OptSpec::long("from", HasArg::Required, ChownOpt::From),
    OptSpec::both(b'h', "no-dereference", HasArg::No, ChownOpt::NoDeref),
    OptSpec::long("no-preserve-root", HasArg::No, ChownOpt::NoPreserveRoot),
    OptSpec::long("preserve-root", HasArg::No, ChownOpt::PreserveRoot),
    OptSpec::both(b'f', "quiet", HasArg::No, ChownOpt::Quiet),
    OptSpec::long("silent", HasArg::No, ChownOpt::Quiet),
    OptSpec::long("reference", HasArg::Required, ChownOpt::Reference),
    OptSpec::both(b'v', "verbose", HasArg::No, ChownOpt::Verbose),
    OptSpec::long("help", HasArg::No, ChownOpt::Help),
    OptSpec::long("version", HasArg::No, ChownOpt::Version),
    OptSpec::short(b'H', HasArg::No, ChownOpt::TraverseCmdline),
    OptSpec::short(b'L', HasArg::No, ChownOpt::TraverseAll),
    OptSpec::short(b'P', HasArg::No, ChownOpt::TraverseNone),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Traverse {
    None,
    CommandLine,
    All,
}

/// A parsed `[OWNER][:[GROUP]]`: ids to set and the names to report.
#[derive(Default)]
struct OwnerSpec {
    uid: Option<u32>,
    gid: Option<u32>,
    user: Option<String>,
    group: Option<String>,
}

struct ChownRun {
    spec: OwnerSpec,
    from_uid: Option<u32>,
    from_gid: Option<u32>,
    recursive: bool,
    traverse: Traverse,
    affect_referent: bool,
    report_changes: bool,
    verbose: bool,
    quiet: bool,
    /// `/`'s identity under `-R --preserve-root` (GNU `chopt.root_dev_ino`).
    root_dev_ino: Option<DevIno>,
}

pub fn run_chown(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    use ChownOpt::*;
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    let mut run = ChownRun {
        spec: OwnerSpec::default(),
        from_uid: None,
        from_gid: None,
        recursive: false,
        traverse: Traverse::None,
        affect_referent: true,
        report_changes: false,
        verbose: false,
        quiet: false,
        root_dev_ino: None,
    };
    let (mut deref, mut preserve_root, mut reference) = (None, false, None);
    while let Some(opt) = opts.next() {
        let (o, v) = match opt {
            Ok(o) => o,
            Err(e) => return Ok(getopt_error("chown", &e)),
        };
        match o {
            Help => return Ok(print_help(CHOWN_HELP)),
            Version => return Ok(print_version("chown")),
            Changes => run.report_changes = true,
            Quiet => run.quiet = true,
            Verbose => run.verbose = true,
            Deref => deref = Some(true),
            NoDeref => deref = Some(false),
            // GNU parses --from as it meets it, before any operand check.
            From => match parse_owner_spec(&v.unwrap_or_default()) {
                Ok(s) => {
                    run.from_uid = s.uid;
                    run.from_gid = s.gid;
                }
                Err(msg) => {
                    eprintln!("chown: {msg}");
                    return Ok(1);
                }
            },
            NoPreserveRoot => preserve_root = false,
            PreserveRoot => preserve_root = true,
            Reference => reference = v,
            Recursive => run.recursive = true,
            TraverseCmdline => run.traverse = Traverse::CommandLine,
            TraverseAll => run.traverse = Traverse::All,
            TraverseNone => run.traverse = Traverse::None,
        }
    }
    if run.recursive && run.traverse == Traverse::None {
        if deref == Some(true) {
            eprintln!("chown: -R --dereference requires either -H or -L");
            return Ok(1);
        }
        deref = Some(false);
    }
    if !run.recursive {
        run.traverse = Traverse::None;
    }
    run.affect_referent = deref != Some(false);

    let mut files = opts.operands();
    if files.len() < if reference.is_some() { 1 } else { 2 } {
        return Ok(match files.last() {
            None => usage_error("chown", "missing operand"),
            Some(last) => usage_error(
                "chown",
                format!("missing operand after {}", quote_locale(last)),
            ),
        });
    }
    if let Some(r) = &reference {
        match fs::metadata(r) {
            Ok(m) => {
                run.spec = OwnerSpec {
                    uid: Some(m.uid()),
                    gid: Some(m.gid()),
                    user: Some(uid_label(m.uid())),
                    group: Some(gid_label(m.gid())),
                };
            }
            Err(e) => {
                eprintln!(
                    "chown: failed to get attributes of {}: {}",
                    qa(r),
                    gnu_io_error(&e)
                );
                return Ok(1);
            }
        }
    } else {
        match parse_owner_spec(&files.remove(0)) {
            Ok(s) => run.spec = s,
            Err(msg) => {
                eprintln!("chown: {msg}");
                return Ok(1);
            }
        }
    }
    if run.recursive && preserve_root {
        match root_dev_ino() {
            Ok(r) => run.root_dev_ino = Some(r),
            Err(e) => {
                eprintln!(
                    "chown: failed to get attributes of '/': {}",
                    gnu_io_error(&e)
                );
                return Ok(1);
            }
        }
    }

    let mut ok = true;
    for f in &files {
        ok &= chown_path(Path::new(f), true, &run, ctx);
    }
    Ok(if ok { 0 } else { 1 })
}

fn uid_label(uid: u32) -> String {
    get_username_by_uid(uid).unwrap_or_else(|| uid.to_string())
}

fn gid_label(gid: u32) -> String {
    get_groupname_by_gid(gid).unwrap_or_else(|| gid.to_string())
}

/// `getpwnam`: (uid, login gid) of a user name.
fn lookup_user(name: &[u8]) -> Option<(u32, u32)> {
    let c = CString::new(name).ok()?;
    // SAFETY: `c` is a NUL-terminated name that outlives the call.
    let pw = unsafe { libc::getpwnam(c.as_ptr()) };
    // SAFETY: a non-null getpwnam result points at a valid passwd record
    // until the next getpw* call; the IDs are copied out at once.
    (!pw.is_null()).then(|| unsafe { ((*pw).pw_uid, (*pw).pw_gid) })
}

fn lookup_group(name: &[u8]) -> Option<u32> {
    let c = CString::new(name).ok()?;
    // SAFETY: `c` is a NUL-terminated name that outlives the call.
    let gr = unsafe { libc::getgrnam(c.as_ptr()) };
    // SAFETY: a non-null getgrnam result points at a valid group record
    // until the next getgr* call; the ID is copied out at once.
    (!gr.is_null()).then(|| unsafe { (*gr).gr_gid })
}

/// A numeric id as GNU accepts it (`-1` is reserved).
fn parse_id(s: &[u8]) -> Option<u32> {
    std::str::from_utf8(s)
        .ok()?
        .parse::<u32>()
        .ok()
        .filter(|&id| id != u32::MAX)
}

/// Parses `[OWNER][:[GROUP]]` (GNU `parse_user_spec`); `.` is accepted as the
/// separator, with a warning, when the whole spec is not a user name. The
/// spec is raw bytes: names are looked up exactly as given.
fn parse_owner_spec(spec: &OsStr) -> std::result::Result<OwnerSpec, String> {
    let b = spec.as_bytes();
    if let Some(i) = b.iter().position(|&c| c == b':') {
        return parse_owner_parts(spec, &b[..i], Some(&b[i + 1..]));
    }
    let whole = parse_owner_parts(spec, b, None);
    if whole.is_err()
        && let Some(i) = b.iter().position(|&c| c == b'.')
        && let Ok(s) = parse_owner_parts(spec, &b[..i], Some(&b[i + 1..]))
    {
        eprintln!("chown: warning: '.' should be ':': {}", quote_locale(spec));
        return Ok(s);
    }
    whole
}

fn parse_owner_parts(
    spec: &OsStr,
    user: &[u8],
    group: Option<&[u8]>,
) -> std::result::Result<OwnerSpec, String> {
    let mut s = OwnerSpec::default();
    let invalid = |what: &str| format!("invalid {what}: {}", quote_locale(spec));
    if !user.is_empty() {
        match lookup_user(user) {
            Some((uid, login_gid)) => {
                s.uid = Some(uid);
                if group == Some(b"") {
                    s.gid = Some(login_gid);
                    s.group = Some(gid_label(login_gid));
                }
            }
            None => {
                s.uid = Some(parse_id(user).ok_or_else(|| invalid("user"))?);
                if group == Some(b"") {
                    return Err(invalid("user"));
                }
            }
        }
        s.user = Some(String::from_utf8_lossy(user).into_owned());
    }
    if let Some(g) = group.filter(|g| !g.is_empty()) {
        let by_name = lookup_group(g);
        s.gid = Some(
            by_name
                .or_else(|| parse_id(g))
                .ok_or_else(|| invalid("group"))?,
        );
        s.group = Some(String::from_utf8_lossy(g).into_owned());
        if by_name.is_some() && s.user.is_none() {
            // GNU reports `:GROUP` (by name) as an ownership change.
            s.user = Some(String::new());
        }
    }
    Ok(s)
}

fn user_group_str(user: Option<&str>, group: Option<&str>) -> Option<String> {
    match (user, group) {
        (Some(u), Some(g)) => Some(format!("{u}:{g}")),
        (Some(u), None) => Some(u.to_string()),
        (None, Some(g)) => Some(g.to_string()),
        (None, None) => None,
    }
}

fn chown_path(path: &Path, top_level: bool, run: &ChownRun, ctx: &DogfishContext) -> bool {
    let spec = &run.spec;
    let new_spec = user_group_str(spec.user.as_deref(), spec.group.as_deref());
    let describe_failure_without_old = || {
        if run.verbose {
            let what = if spec.user.is_some() {
                "ownership"
            } else {
                "group"
            };
            match &new_spec {
                Some(s) => outln!("failed to change {what} of {} to {s}", qa(path)),
                None => outln!("failed to change ownership of {}", qa(path)),
            }
        }
    };
    // The stat fts makes for the walk (`fts_statp`): `-L` follows every symlink,
    // `-H` those on the command line; a dangling one falls back to the link.
    // Whether a directory is entered depends on this alone, never on `-h`.
    let walk_follows =
        run.traverse == Traverse::All || (top_level && run.traverse == Traverse::CommandLine);
    let walked = if walk_follows {
        fs::metadata(path).or_else(|e| match fs::symlink_metadata(path) {
            Ok(m) if e.kind() == std::io::ErrorKind::NotFound => Ok(m),
            _ => Err(e),
        })
    } else {
        fs::symlink_metadata(path)
    };
    let walked = match walked {
        Ok(m) => m,
        Err(e) => {
            if !run.quiet {
                eprintln!("chown: cannot access {}: {}", qa(path), gnu_io_error(&e));
            }
            describe_failure_without_old();
            return false;
        }
    };
    let traverse_dir = run.recursive && walked.is_dir();
    // GNU `change_file_owner`, FTS_D: every directory the walk would enter is
    // checked, the command-line operands and those reached through `-L` alike.
    if traverse_dir && is_root_dev_ino(run.root_dev_ino, &walked) {
        refuse_root("chown", path);
        return false;
    }
    let follow = walked.is_symlink() && run.affect_referent;
    let meta = if follow {
        match fs::metadata(path) {
            Ok(m) => m,
            Err(e) => {
                if !run.quiet {
                    eprintln!(
                        "chown: cannot dereference {}: {}",
                        qa(path),
                        gnu_io_error(&e)
                    );
                }
                describe_failure_without_old();
                return false;
            }
        }
    } else {
        walked
    };

    let mut ok = true;
    // Like GNU (fts post-order), a directory's contents are changed before it.
    if traverse_dir {
        match dir_names(path, false) {
            Ok(names) => {
                for name in names {
                    ok &= chown_path(&path.join(name), false, run, ctx);
                }
            }
            Err(e) => {
                if !run.quiet {
                    eprintln!(
                        "chown: cannot read directory {}: {}",
                        qa(path),
                        gnu_io_error(&e)
                    );
                }
                ok = false;
            }
        }
    }

    let (old_uid, old_gid) = (meta.uid(), meta.gid());
    let matches =
        run.from_uid.is_none_or(|u| u == old_uid) && run.from_gid.is_none_or(|g| g == old_gid);
    let old_spec = user_group_str(
        spec.user.as_ref().map(|_| uid_label(old_uid)).as_deref(),
        spec.group.as_ref().map(|_| gid_label(old_gid)).as_deref(),
    );
    let mut status_changed = false;
    let mut failed = false;
    if matches && (spec.uid.is_some() || spec.gid.is_some()) {
        let res = if run.affect_referent {
            std::os::unix::fs::chown(path, spec.uid, spec.gid)
        } else {
            std::os::unix::fs::lchown(path, spec.uid, spec.gid)
        };
        match res {
            Ok(()) => {
                ctx.telemetry.add_items(1);
                status_changed = spec.uid.is_some_and(|u| u != old_uid)
                    || spec.gid.is_some_and(|g| g != old_gid);
            }
            Err(e) => {
                failed = true;
                ok = false;
                if !run.quiet {
                    let what = if spec.uid.is_some() {
                        "ownership"
                    } else {
                        "group"
                    };
                    eprintln!(
                        "chown: changing {what} of {}: {}",
                        qa(path),
                        gnu_io_error(&e)
                    );
                }
            }
        }
    }
    if run.verbose || (run.report_changes && status_changed) {
        let what = if spec.user.is_some() {
            "ownership"
        } else if spec.group.is_some() {
            "group"
        } else {
            ""
        };
        let file = qa(path);
        let (old_s, new_s) = (old_spec.unwrap_or_default(), new_spec.unwrap_or_default());
        if failed {
            if what.is_empty() {
                outln!("failed to change ownership of {file}");
            } else {
                outln!("failed to change {what} of {file} from {old_s} to {new_s}");
            }
        } else if status_changed {
            outln!("changed {what} of {file} from {old_s} to {new_s}");
        } else if what.is_empty() {
            outln!("ownership of {file} retained");
        } else {
            outln!("{what} of {file} retained as {old_s}");
        }
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    // The --preserve-root walk is tested against a temporary directory standing
    // in for `/`, never against the real root (AGENTS.md). The group is "changed"
    // to the files' own group, which every owner may do.
    fn run(traverse: Traverse, affect_referent: bool, root: Option<DevIno>, gid: u32) -> ChownRun {
        ChownRun {
            spec: OwnerSpec {
                uid: None,
                gid: Some(gid),
                user: None,
                group: Some(gid.to_string()),
            },
            from_uid: None,
            from_gid: None,
            recursive: true,
            traverse,
            affect_referent,
            report_changes: false,
            verbose: false,
            quiet: true,
            root_dev_ino: root,
        }
    }

    #[test]
    fn preserve_root_checks_every_directory_the_walk_enters() {
        let tmp = tempfile::tempdir().unwrap();
        let fake_root = tmp.path().join("root");
        fs::create_dir(&fake_root).unwrap();
        let d = tmp.path().join("d");
        fs::create_dir(&d).unwrap();
        symlink(&fake_root, d.join("slink-to-root")).unwrap();
        let top_link = tmp.path().join("top-link");
        symlink(&fake_root, &top_link).unwrap();
        let ctx = DogfishContext::new("chown");
        let root = Some(DevIno::of(&fs::metadata(&fake_root).unwrap()));
        let gid = fs::metadata(&d).unwrap().gid();
        let (all, cmdline, none) = (Traverse::All, Traverse::CommandLine, Traverse::None);

        // -RLh and -RL: the walk follows d/slink-to-root, so it is refused.
        assert!(!chown_path(&d, true, &run(all, false, root, gid), &ctx));
        assert!(!chown_path(&d, true, &run(all, true, root, gid), &ctx));
        // -RHh and -RP: the link below the operand is not followed.
        assert!(chown_path(&d, true, &run(cmdline, false, root, gid), &ctx));
        assert!(chown_path(&d, true, &run(none, false, root, gid), &ctx));
        // -RH on a command-line link to root, with or without -h: refused.
        assert!(!chown_path(
            &top_link,
            true,
            &run(cmdline, true, root, gid),
            &ctx
        ));
        assert!(!chown_path(
            &top_link,
            true,
            &run(cmdline, false, root, gid),
            &ctx
        ));
        // -RP: a command-line link is just a link.
        assert!(chown_path(
            &top_link,
            true,
            &run(none, false, root, gid),
            &ctx
        ));
        // Without --preserve-root, -RLh walks through.
        assert!(chown_path(&d, true, &run(all, false, None, gid), &ctx));
    }
}
