//! id — With `--json` and colorized interactive output

use std::ffi::{CStr, CString};
use std::io::Write;

use anyhow::Result;
use dogfish_core::{DogfishContext, RecordWriter, stdout};
use serde::Serialize;

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, Order, getopt_error, print_help, print_version,
};
use crate::common::json_opt::{check_json_version, json_long, json_short};
use crate::common::quote::quote_locale;
use crate::common::users::{get_groupname_by_gid, get_username_by_uid};

const ID_HELP: &str = "\
Print user and group information for each specified USER, or (when USER omitted) for the current
process

Usage: id [OPTIONS] [USERS]...

Arguments:
  [USERS]...  

Options:
  -a             Ignore, for compatibility with other versions
  -Z, --context  Print only the security context of the process
  -g, --group    Print only the effective group ID
  -G, --groups   Print all group IDs
  -n, --name     Print a name instead of a number, for -u, -g, -G
  -r, --real     Print the real ID instead of the effective ID, with -u, -g, -G
  -u, --user     Print only the effective user ID
  -z, --zero     Delimit entries with NUL characters, not whitespace
  -J, --json[=VERSION]
                 One NDJSON record per user: ids, names and groups
      --help     Display this help and exit
";

#[derive(Default)]
struct IdArgs {
    context: bool,
    group: bool,
    groups: bool,
    name: bool,
    real: bool,
    user: bool,
    zero: bool,
    json: bool,
    ignored: bool,
    users: Vec<std::ffi::OsString>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum IdArgsOpt {
    Context,
    Group,
    Groups,
    Name,
    Real,
    User,
    Zero,
    Json,
    Ignored,
    Help,
    Version,
}

const SPECS: &[OptSpec<IdArgsOpt>] = &[
    OptSpec::both(b'Z', "context", HasArg::No, IdArgsOpt::Context),
    OptSpec::both(b'g', "group", HasArg::No, IdArgsOpt::Group),
    OptSpec::both(b'G', "groups", HasArg::No, IdArgsOpt::Groups),
    OptSpec::both(b'n', "name", HasArg::No, IdArgsOpt::Name),
    OptSpec::both(b'r', "real", HasArg::No, IdArgsOpt::Real),
    OptSpec::both(b'u', "user", HasArg::No, IdArgsOpt::User),
    OptSpec::both(b'z', "zero", HasArg::No, IdArgsOpt::Zero),
    json_short(IdArgsOpt::Json),
    json_long(IdArgsOpt::Json),
    OptSpec::long("help", HasArg::No, IdArgsOpt::Help),
    OptSpec::long("version", HasArg::No, IdArgsOpt::Version),
    OptSpec::short(b'a', HasArg::No, IdArgsOpt::Ignored),
];

/// Parses the command line; `Err` is the exit status after `--help`,
/// `--version` or a usage diagnostic.
fn parse_id_args(args: Vec<std::ffi::OsString>) -> std::result::Result<IdArgs, i32> {
    let mut parsed = IdArgs::default();
    let mut opts = Getopt::with_order(args.into_iter().skip(1).collect(), SPECS, Order::Permute);
    while let Some(opt) = opts.next() {
        let (o, v) = opt.map_err(|e| getopt_error("id", &e))?;
        match o {
            IdArgsOpt::Context => parsed.context = true,
            IdArgsOpt::Group => parsed.group = true,
            IdArgsOpt::Groups => parsed.groups = true,
            IdArgsOpt::Name => parsed.name = true,
            IdArgsOpt::Real => parsed.real = true,
            IdArgsOpt::User => parsed.user = true,
            IdArgsOpt::Zero => parsed.zero = true,
            IdArgsOpt::Json => {
                check_json_version("id", v.as_deref())?;
                parsed.json = true;
            }
            IdArgsOpt::Ignored => parsed.ignored = true,
            IdArgsOpt::Help => return Err(print_help(ID_HELP)),
            IdArgsOpt::Version => return Err(print_version("id")),
        }
    }
    parsed.users = opts.operands();
    Ok(parsed)
}

/// One entry of a `-J` record's `groups`.
#[derive(Serialize)]
struct GroupEntry {
    gid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

/// Looks `spec` up as a user name, falling back to a numeric user ID.
fn lookup_user(spec: &std::ffi::OsStr) -> Option<(u32, u32, CString)> {
    use std::os::unix::ffi::OsStrExt;
    let cname = CString::new(spec.as_bytes()).ok()?;
    // SAFETY: getpwnam/getpwuid return null or a pointer to static storage that
    // stays valid until the next lookup; the fields are copied out immediately.
    unsafe {
        let mut pw = libc::getpwnam(cname.as_ptr());
        if pw.is_null()
            && let Some(uid) = spec.to_str().and_then(|s| s.parse::<u32>().ok())
        {
            pw = libc::getpwuid(uid);
        }
        if pw.is_null() {
            return None;
        }
        Some((
            (*pw).pw_uid,
            (*pw).pw_gid,
            CStr::from_ptr((*pw).pw_name).to_owned(),
        ))
    }
}

/// The group list GNU's `mgetgroups` reports: `primary` first, then the user's
/// (or the process's) supplementary groups, minus duplicates of the first
/// entry and of their predecessor.
fn group_list(user: Option<&CStr>, primary: u32) -> Vec<u32> {
    let mut groups: Vec<u32> = vec![primary];
    match user {
        // SAFETY: getgroups is called with a correctly sized buffer.
        None => unsafe {
            let n = libc::getgroups(0, std::ptr::null_mut());
            if n > 0 {
                let mut buf = vec![0 as libc::gid_t; n as usize];
                let n = libc::getgroups(n, buf.as_mut_ptr());
                if n > 0 {
                    groups.extend_from_slice(&buf[..n as usize]);
                }
            }
        },
        Some(name) => {
            let mut size: libc::c_int = 64;
            loop {
                let mut buf = vec![0 as libc::gid_t; size as usize];
                let mut n = size;
                // SAFETY: `buf` holds `n` entries; getgrouplist updates `n`.
                #[cfg(not(target_vendor = "apple"))]
                let rc =
                    unsafe { libc::getgrouplist(name.as_ptr(), primary, buf.as_mut_ptr(), &mut n) };
                // SAFETY: as above; Apple's prototype uses `int` group IDs.
                #[cfg(target_vendor = "apple")]
                let rc = unsafe {
                    libc::getgrouplist(
                        name.as_ptr(),
                        primary as libc::c_int,
                        buf.as_mut_ptr().cast::<libc::c_int>(),
                        &mut n,
                    )
                };
                if rc >= 0 {
                    groups = buf[..n.max(0) as usize].to_vec();
                    groups.insert(0, primary);
                    break;
                }
                if size >= 1 << 16 {
                    break;
                }
                size = (size * 2).max(n);
            }
        }
    }
    let first = groups[0];
    let mut deduped = vec![first];
    for &g in &groups[1..] {
        if g != first && Some(&g) != deduped.last() {
            deduped.push(g);
        }
    }
    deduped
}

/// The identity `id` reports on: a named user, or the current process.
struct Identity {
    ruid: u32,
    euid: u32,
    rgid: u32,
    egid: u32,
    name: Option<CString>,
}

impl Identity {
    fn current() -> Self {
        // SAFETY: these getters have no preconditions and cannot fail.
        unsafe {
            Self {
                ruid: libc::getuid(),
                euid: libc::geteuid(),
                rgid: libc::getgid(),
                egid: libc::getegid(),
                name: None,
            }
        }
    }

    fn groups(&self) -> Vec<u32> {
        group_list(self.name.as_deref(), self.egid)
    }
}

/// Appends a user or group as GNU does with `-n`: the name, or the number plus
/// a diagnostic when there is none. Returns false if the name was missing.
fn push_id(out: &mut Vec<u8>, id: u32, use_name: bool, is_user: bool) -> bool {
    if !use_name {
        out.extend_from_slice(id.to_string().as_bytes());
        return true;
    }
    let name = if is_user {
        get_username_by_uid(id)
    } else {
        get_groupname_by_gid(id)
    };
    match name {
        Some(n) => {
            out.extend_from_slice(n.as_bytes());
            true
        }
        None => {
            let kind = if is_user { "user" } else { "group" };
            eprintln!("id: cannot find name for {kind} ID {id}");
            out.extend_from_slice(id.to_string().as_bytes());
            false
        }
    }
}

fn id_full_format(who: &Identity, st: dogfish_core::Style) -> String {
    let user = |id: u32| match get_username_by_uid(id) {
        Some(n) => format!("{}({})", st.bold_cyan(&id.to_string()), st.bold_green(&n)),
        None => st.bold_cyan(&id.to_string()),
    };
    let group = |id: u32, bold: bool| match get_groupname_by_gid(id) {
        Some(n) if bold => format!("{}({})", st.bold_cyan(&id.to_string()), st.bold_green(&n)),
        Some(n) => format!("{}({})", st.bold_cyan(&id.to_string()), st.green(&n)),
        None => st.bold_cyan(&id.to_string()),
    };
    let mut line = format!("uid={} gid={}", user(who.ruid), group(who.rgid, true));
    if who.euid != who.ruid {
        line.push_str(&format!(" euid={}", user(who.euid)));
    }
    if who.egid != who.rgid {
        line.push_str(&format!(" egid={}", group(who.egid, true)));
    }
    let groups: Vec<String> = who.groups().into_iter().map(|g| group(g, false)).collect();
    if !groups.is_empty() {
        line.push_str(&format!(" groups={}", groups.join(",")));
    }
    line
}

pub fn run_id(args: Vec<std::ffi::OsString>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_id_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let _ = parsed.ignored;
    // There is no SELinux support, so -Z can never be satisfied.
    if parsed.context {
        eprintln!("id: --context (-Z) works only on an SELinux-enabled kernel");
        return Ok(1);
    }
    let only = [parsed.user, parsed.group, parsed.groups];
    if only.iter().filter(|&&b| b).count() > 1 {
        eprintln!("id: cannot print \"only\" of more than one choice");
        return Ok(1);
    }
    let default_format = !only.contains(&true);
    if !parsed.json && default_format && (parsed.real || parsed.name) {
        eprintln!("id: cannot print only names or real IDs in default format");
        return Ok(1);
    }
    if !parsed.json && default_format && parsed.zero {
        eprintln!("id: option --zero not permitted in default format");
        return Ok(1);
    }

    // Each user, or the operand that names no user (reported in turn, as GNU does).
    let mut targets: Vec<std::result::Result<Identity, &std::ffi::OsStr>> = Vec::new();
    let mut status = 0;
    if parsed.users.is_empty() {
        targets.push(Ok(Identity::current()));
    } else {
        for spec in &parsed.users {
            targets.push(match lookup_user(spec) {
                Some((uid, gid, name)) => Ok(Identity {
                    ruid: uid,
                    euid: uid,
                    rgid: gid,
                    egid: gid,
                    name: Some(name),
                }),
                None => Err(spec),
            });
        }
    }

    let mut out = stdout::stdout();
    let st = ctx.style();
    let term = if parsed.zero { b'\0' } else { b'\n' };
    for target in targets {
        let who = match target {
            Ok(who) => who,
            Err(spec) => {
                out.flush()?;
                eprintln!("id: {}: no such user", quote_locale(spec));
                status = 1;
                continue;
            }
        };
        if parsed.json {
            // One record per user: real and effective ids, names where the
            // ids have them, and every group.
            let groups: Vec<GroupEntry> = who
                .groups()
                .into_iter()
                .map(|gid| GroupEntry {
                    gid,
                    name: get_groupname_by_gid(gid),
                })
                .collect();
            let mut w = RecordWriter::new(stdout::stdout());
            w.record(|r| {
                r.u64("uid", u64::from(who.ruid));
                if let Some(n) = get_username_by_uid(who.ruid) {
                    r.str("user", &n);
                }
                r.u64("gid", u64::from(who.rgid));
                if let Some(n) = get_groupname_by_gid(who.rgid) {
                    r.str("group", &n);
                }
                r.u64("euid", u64::from(who.euid))
                    .u64("egid", u64::from(who.egid))
                    .value("groups", &groups);
            })?;
            w.flush()?;
            continue;
        }
        let mut line = Vec::new();
        if parsed.user {
            let uid = if parsed.real { who.ruid } else { who.euid };
            if !push_id(&mut line, uid, parsed.name, true) {
                status = 1;
            }
        } else if parsed.group {
            let gid = if parsed.real { who.rgid } else { who.egid };
            if !push_id(&mut line, gid, parsed.name, false) {
                status = 1;
            }
        } else if parsed.groups {
            let delim = if parsed.zero { b'\0' } else { b' ' };
            let mut ok = push_id(&mut line, who.rgid, parsed.name, false);
            if who.egid != who.rgid {
                line.push(delim);
                ok &= push_id(&mut line, who.egid, parsed.name, false);
            }
            for g in who.groups() {
                if g != who.rgid && g != who.egid {
                    line.push(delim);
                    ok &= push_id(&mut line, g, parsed.name, false);
                }
            }
            if !ok {
                status = 1;
            }
        } else {
            line.extend_from_slice(id_full_format(&who, st).as_bytes());
        }
        line.push(term);
        // With several users, -Gz ends each record with two NULs so that
        // the records can be told apart.
        if parsed.zero && parsed.groups && parsed.users.len() > 1 {
            line.push(b'\0');
        }
        out.write_all(&line)?;
    }
    out.flush()?;
    Ok(status)
}

/// id's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("id", ID_HELP, SPECS)
}
