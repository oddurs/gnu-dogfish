//! `--files0-from=F`: file operands read as NUL-terminated names from F (or
//! standard input for `-`), with GNU's diagnostics. Shared by wc and sort
//! (gnulib `readtokens0` and `argv_iter`).

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};

use crate::common::quote::{qa, qf};

/// Lists of at most this size are read whole, so their length is known in
/// advance (GNU: 10 MiB or half the available memory).
const READ_WHOLE_LIMIT: u64 = 10 * 1024 * 1024;

/// An open `--files0-from` list.
pub(crate) struct Files0From {
    /// The option's argument, for messages.
    pub(crate) name: String,
    reader: Box<dyn BufRead>,
    is_regular: bool,
    size: u64,
}

impl Files0From {
    /// Opens the list; `Err` carries the open error.
    pub(crate) fn open(name: &str) -> io::Result<Self> {
        let (reader, meta): (Box<dyn Read>, _) = if name == "-" {
            let stdin = crate::common::input::stdin_file();
            let meta = stdin.metadata().ok();
            (Box::new(io::stdin()), meta)
        } else {
            let file = File::open(name)?;
            let meta = file.metadata().ok();
            (Box::new(file), meta)
        };
        Ok(Self {
            name: name.to_string(),
            reader: Box::new(BufReader::new(reader)),
            is_regular: meta.as_ref().is_some_and(|m| m.is_file()),
            size: meta.map_or(0, |m| m.len()),
        })
    }

    /// Whether GNU's wc would read the whole list up front (a small regular
    /// file), and so know how many names there are.
    pub(crate) fn is_small_regular(&self) -> bool {
        self.is_regular && self.size <= READ_WHOLE_LIMIT
    }

    /// Reads every remaining name (`readtokens0`); a final name without a
    /// NUL counts.
    pub(crate) fn read_all(&mut self) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        while let Some(name) = self.next_name()? {
            names.push(name);
        }
        Ok(names)
    }

    /// The next name, `None` at the end of the list.
    pub(crate) fn next_name(&mut self) -> io::Result<Option<String>> {
        let mut buf = Vec::new();
        if self.reader.read_until(0, &mut buf)? == 0 {
            return Ok(None);
        }
        if buf.last() == Some(&0) {
            buf.pop();
        }
        Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
    }

    /// GNU's complaint about the `number`th name (1-based), if any: `-` when
    /// the list itself is standard input (`stdin_only`), or an empty name.
    pub(crate) fn check_name(&self, name: &str, number: u64, stdin_only: bool) -> Option<String> {
        if name == "-" && (!stdin_only || self.name == "-") {
            return Some(format!(
                "when reading file names from stdin, no file name of {} allowed",
                qa(name)
            ));
        }
        if name.is_empty() {
            return Some(format!(
                "{}:{number}: invalid zero-length file name",
                qf(&self.name)
            ));
        }
        None
    }
}

/// GNU's diagnostic for file operands given with `--files0-from` (followed
/// by the `--help` hint); returns `status`.
pub(crate) fn extra_operand_error(tool: &str, operand: &str, status: i32) -> i32 {
    eprintln!(
        "{tool}: extra operand {}\nfile operands cannot be combined with --files0-from\n\
         Try '{tool} --help' for more information.",
        qa(operand)
    );
    status
}
