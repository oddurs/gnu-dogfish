//! System identification shared by uname and hostname.

use std::io;

use anyhow::Result;
use serde::Serialize;

#[derive(Serialize)]
pub(crate) struct UnameInfo {
    pub(crate) sysname: String,
    pub(crate) nodename: String,
    pub(crate) release: String,
    pub(crate) version: String,
    pub(crate) machine: String,
    pub(crate) processor: String,
    pub(crate) hardware_platform: String,
    pub(crate) operating_system: String,
}

pub(crate) const UNKNOWN: &str = "unknown";

pub(crate) fn get_uname_info() -> Result<UnameInfo> {
    // SAFETY: `uts` is a valid out-pointer; on success every field holds a
    // NUL-terminated string.
    let uts = unsafe {
        let mut uts: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut uts) != 0 {
            return Err(io::Error::last_os_error().into());
        }
        uts
    };
    let field = |chars: &[libc::c_char]| -> String {
        let bytes: Vec<u8> = chars
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let machine = field(&uts.machine);
    let sysname = field(&uts.sysname);
    // GNU/Linux distributions report the machine for -p and -i; elsewhere
    // GNU has no portable answer.
    let (processor, hardware_platform) = if cfg!(target_os = "linux") {
        (machine.clone(), machine.clone())
    } else {
        (UNKNOWN.to_string(), UNKNOWN.to_string())
    };
    let operating_system = if cfg!(all(target_os = "linux", target_env = "gnu")) {
        "GNU/Linux".to_string()
    } else {
        sysname.clone()
    };
    Ok(UnameInfo {
        sysname,
        nodename: field(&uts.nodename),
        release: field(&uts.release),
        version: field(&uts.version),
        machine,
        processor,
        hardware_platform,
        operating_system,
    })
}
