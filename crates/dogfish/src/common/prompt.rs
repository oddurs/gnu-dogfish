//! Interactive prompts (cp, ln, mv, rm).

use std::io::{self, BufRead, Write};

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
