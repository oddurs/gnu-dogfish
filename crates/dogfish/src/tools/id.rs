//! id — With `--json` and colorized interactive output

use std::ffi::{CStr, CString};
use std::io::{self, Write};

use anyhow::Result;
use clap::{ArgAction, Parser};
use dogfish_core::{DogfishContext, gnu_quote, io::ignore_broken_pipe};
use serde::Serialize;

use crate::common::users::{get_groupname_by_gid, get_username_by_uid};

#[derive(Parser)]
#[command(
    name = "id",
    about = "Print user and group information for each specified USER, \
             or (when USER omitted) for the current process",
    disable_help_flag = true,
    infer_long_args = true
)]
pub(crate) struct IdArgs {
    /// Ignore, for compatibility with other versions
    #[arg(short = 'a')]
    pub(crate) ignored: bool,

    /// Print only the security context of the process
    #[arg(short = 'Z', long = "context")]
    context: bool,

    /// Print only the effective group ID
    #[arg(short = 'g', long = "group")]
    group: bool,

    /// Print all group IDs
    #[arg(short = 'G', long = "groups")]
    groups: bool,

    /// Print a name instead of a number, for -u, -g, -G
    #[arg(short = 'n', long = "name")]
    pub(crate) name: bool,

    /// Print the real ID instead of the effective ID, with -u, -g, -G
    #[arg(short = 'r', long = "real")]
    real: bool,

    /// Print only the effective user ID
    #[arg(short = 'u', long = "user")]
    user: bool,

    /// Delimit entries with NUL characters, not whitespace
    #[arg(short = 'z', long = "zero")]
    pub(crate) zero: bool,

    /// Emit structured JSON
    #[arg(short = 'J', long = "json")]
    pub(crate) json: bool,

    /// Display this help and exit
    #[arg(long, action = ArgAction::Help)]
    pub(crate) help: Option<bool>,

    users: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct IdRecord {
    pub(crate) uid: u32,
    user: String,
    pub(crate) gid: u32,
    group: String,
    groups: Vec<(u32, String)>,
}

/// Looks `spec` up as a user name, falling back to a numeric user ID.
fn lookup_user(spec: &str) -> Option<(u32, u32, CString)> {
    let cname = CString::new(spec).ok()?;
    // SAFETY: getpwnam/getpwuid return null or a pointer to static storage that
    // stays valid until the next lookup; the fields are copied out immediately.
    unsafe {
        let mut pw = libc::getpwnam(cname.as_ptr());
        if pw.is_null()
            && let Ok(uid) = spec.parse::<u32>()
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
pub(crate) struct Identity {
    ruid: u32,
    euid: u32,
    rgid: u32,
    egid: u32,
    pub(crate) name: Option<CString>,
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

pub fn run_id(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = IdArgs::try_parse_from(args)?;
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

    let mut targets: Vec<Option<Identity>> = Vec::new();
    let mut status = 0;
    if parsed.users.is_empty() {
        targets.push(Some(Identity::current()));
    } else {
        for spec in &parsed.users {
            match lookup_user(spec) {
                Some((uid, gid, name)) => targets.push(Some(Identity {
                    ruid: uid,
                    euid: uid,
                    rgid: gid,
                    egid: gid,
                    name: Some(name),
                })),
                None => {
                    eprintln!("id: {}: no such user", gnu_quote(spec));
                    status = 1;
                    targets.push(None);
                }
            }
        }
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let st = ctx.style();
    let term = if parsed.zero { b'\0' } else { b'\n' };
    for who in targets.into_iter().flatten() {
        if parsed.json {
            let group_entry =
                |g: u32| (g, get_groupname_by_gid(g).unwrap_or_else(|| g.to_string()));
            let rec = IdRecord {
                uid: who.euid,
                user: get_username_by_uid(who.euid).unwrap_or_else(|| who.euid.to_string()),
                gid: who.egid,
                group: group_entry(who.egid).1,
                groups: who.groups().into_iter().map(group_entry).collect(),
            };
            writeln!(out, "{}", serde_json::to_string_pretty(&rec)?)?;
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
        ignore_broken_pipe(out.write_all(&line))?;
    }
    ignore_broken_pipe(out.flush())?;
    Ok(status)
}
