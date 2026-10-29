//! Interactive prompts (cp, ln, mv, rm).

use std::io::{self, BufRead, Write};
use std::path::Path;

use crate::common::path::c_path;

/// Prints `tool: question` on stderr and reads one line from stdin; true when it
/// starts with `y` or `Y` (GNU's `yesno` in the C locale). EOF answers no.
pub(crate) fn ask(tool: &str, question: &str) -> bool {
    eprint!("{tool}: {question}");
    let _ = io::stderr().flush();
    let mut line = Vec::new();
    match io::stdin().lock().read_until(b'\n', &mut line) {
        Ok(n) if n > 0 => matches!(line[0], b'y' | b'Y'),
        _ => false,
    }
}

/// True when the calling user may not write `p` (GNU asks before removing or
/// replacing such files). Symbolic links are never write-protected.
pub(crate) fn write_protected(p: &Path) -> bool {
    let Ok(c) = c_path(p) else {
        return false;
    };
    // SAFETY: `c` is a NUL-terminated path that outlives the call.
    let denied = unsafe { libc::access(c.as_ptr(), libc::W_OK) } != 0;
    denied && io::Error::last_os_error().raw_os_error() == Some(libc::EACCES)
}

/// Decides whether `rm`, `mv` or `cp` should ask before acting on a file.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Interactive {
    /// Never ask (`-f`).
    Never,
    /// Ask only about write-protected files when stdin is a terminal (the default).
    Sometimes,
    /// Always ask (`-i`).
    Always,
}
