//! chown — USER[:GROUP], :GROUP, USER:, numeric ids, -R/-h/-H/-L/-P, --from

use std::ffi::{CString, OsString};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::Result;
use dogfish_core::{DogfishContext, gnu_io_error, outln};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy, print_help, print_version, usage_error,
};
use crate::common::path::dir_names;
use crate::common::preserve_root::{is_root_dir, refuse_root};
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
    };
    let (mut deref, mut preserve_root, mut from, mut reference) = (None, false, None, None);
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
            From => from = v.as_deref().map(lossy),
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
        if files.is_empty() {
            return Ok(usage_error("chown", "missing operand"));
        }
        let spec = lossy(&files.remove(0));
        match parse_owner_spec(&spec) {
            Ok(s) => run.spec = s,
            Err(msg) => {
                eprintln!("chown: {msg}");
                return Ok(1);
            }
        }
        if files.is_empty() {
            return Ok(usage_error(
                "chown",
                format!("missing operand after {}", quote_locale(&spec)),
            ));
        }
    }
    if let Some(f) = &from {
        match parse_owner_spec(f) {
            Ok(s) => {
                run.from_uid = s.uid;
                run.from_gid = s.gid;
            }
            Err(msg) => {
                eprintln!("chown: {msg}");
                return Ok(1);
            }
        }
    }

    let mut ok = true;
    for f in &files {
        if run.recursive && preserve_root && is_root_dir(Path::new(f)) {
            refuse_root("chown", f);
            ok = false;
            continue;
        }
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
fn lookup_user(name: &str) -> Option<(u32, u32)> {
    let c = CString::new(name).ok()?;
    let pw = unsafe { libc::getpwnam(c.as_ptr()) };
    (!pw.is_null()).then(|| unsafe { ((*pw).pw_uid, (*pw).pw_gid) })
}

fn lookup_group(name: &str) -> Option<u32> {
    let c = CString::new(name).ok()?;
    let gr = unsafe { libc::getgrnam(c.as_ptr()) };
    (!gr.is_null()).then(|| unsafe { (*gr).gr_gid })
}

/// A numeric id as GNU accepts it (`-1` is reserved).
fn parse_id(s: &str) -> Option<u32> {
    s.parse::<u32>().ok().filter(|&id| id != u32::MAX)
}

/// Parses `[OWNER][:[GROUP]]` (GNU `parse_user_spec`); `.` is accepted as the
/// separator, with a warning, when the whole spec is not a user name.
fn parse_owner_spec(spec: &str) -> std::result::Result<OwnerSpec, String> {
    if let Some(i) = spec.find(':') {
        return parse_owner_parts(spec, &spec[..i], Some(&spec[i + 1..]));
    }
    let whole = parse_owner_parts(spec, spec, None);
    if whole.is_err()
        && let Some(i) = spec.find('.')
        && let Ok(s) = parse_owner_parts(spec, &spec[..i], Some(&spec[i + 1..]))
    {
        eprintln!("chown: warning: '.' should be ':': {}", quote_locale(spec));
        return Ok(s);
    }
    whole
}

fn parse_owner_parts(
    spec: &str,
    user: &str,
    group: Option<&str>,
) -> std::result::Result<OwnerSpec, String> {
    let mut s = OwnerSpec::default();
    let invalid = |what: &str| format!("invalid {what}: {}", quote_locale(spec));
    if !user.is_empty() {
        match lookup_user(user) {
            Some((uid, login_gid)) => {
                s.uid = Some(uid);
                if group == Some("") {
                    s.gid = Some(login_gid);
                    s.group = Some(gid_label(login_gid));
                }
            }
            None => {
                s.uid = Some(parse_id(user).ok_or_else(|| invalid("user"))?);
                if group == Some("") {
                    return Err(invalid("user"));
                }
            }
        }
        s.user = Some(user.to_string());
    }
    if let Some(g) = group.filter(|g| !g.is_empty()) {
        let by_name = lookup_group(g);
        s.gid = Some(
            by_name
                .or_else(|| parse_id(g))
                .ok_or_else(|| invalid("group"))?,
        );
        s.group = Some(g.to_string());
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
    let lmeta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => {
            if !run.quiet {
                eprintln!("chown: cannot access {}: {}", qa(path), gnu_io_error(&e));
            }
            describe_failure_without_old();
            return false;
        }
    };
    let follow = lmeta.is_symlink() && run.affect_referent;
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
        lmeta.clone()
    };

    let traverse_dir = run.recursive
        && meta.is_dir()
        && (!lmeta.is_symlink()
            || run.traverse == Traverse::All
            || (top_level && run.traverse == Traverse::CommandLine));
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
        let res = if follow || !lmeta.is_symlink() {
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
