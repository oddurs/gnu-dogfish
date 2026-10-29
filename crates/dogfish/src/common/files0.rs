//! `--files0-from=F`: file operands read as NUL-terminated names from F (or
//! standard input for `-`), with GNU's diagnostics (gnulib `argv_iter` and
//! `readtokens0`). Names are bytes, so they need not be UTF-8.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use crate::common::quote::{qa, qf, quote_locale};

/// An open `--files0-from` list.
pub(crate) struct Files0From {
    /// The option's argument, for messages.
    pub(crate) name: OsString,
    reader: Box<dyn BufRead>,
    /// Names read so far (`argv_iter_n_args`).
    count: u64,
}

impl Files0From {
    /// Opens the list; `Err` carries the open error (GNU: "cannot open %s
    /// for reading", see [`Files0From::open_error`]).
    pub(crate) fn open(name: &OsStr) -> io::Result<Self> {
        let reader: Box<dyn Read> = if name.as_bytes() == b"-" {
            Box::new(io::stdin())
        } else {
            Box::new(File::open(name)?)
        };
        Ok(Self {
            name: name.to_os_string(),
            reader: Box::new(BufReader::new(reader)),
            count: 0,
        })
    }

    /// GNU's message for a list that cannot be opened.
    pub(crate) fn open_error(name: &OsStr, e: &io::Error) -> String {
        format!(
            "cannot open {} for reading: {}",
            qa(name),
            dogfish_core::gnu_io_error(e)
        )
    }

    /// GNU's message for a failure to read the list.
    pub(crate) fn read_error(&self, e: &io::Error) -> String {
        format!(
            "{}: read error: {}",
            qf(&self.name),
            dogfish_core::gnu_io_error(e)
        )
    }

    /// How many names have been read so far.
    pub(crate) fn count(&self) -> u64 {
        self.count
    }

    /// The next name, `None` at the end of the list.
    pub(crate) fn next_name(&mut self) -> io::Result<Option<OsString>> {
        let mut buf = Vec::new();
        if self.reader.read_until(0, &mut buf)? == 0 {
            return Ok(None);
        }
        if buf.last() == Some(&0) {
            buf.pop();
        }
        self.count += 1;
        Ok(Some(OsString::from_vec(buf)))
    }

    /// GNU's complaint about the `number`th name (1-based), if any: `-` when
    /// the list itself is standard input (or, unless `stdin_only`, always),
    /// or an empty name.
    pub(crate) fn check_name(&self, name: &OsStr, number: u64, stdin_only: bool) -> Option<String> {
        if name.as_bytes() == b"-" && (!stdin_only || self.name.as_bytes() == b"-") {
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
pub(crate) fn extra_operand_error(tool: &str, operand: &OsStr, status: i32) -> i32 {
    eprintln!(
        "{tool}: extra operand {}\nfile operands cannot be combined with --files0-from\n\
         Try '{tool} --help' for more information.",
        quote_locale(operand)
    );
    status
}
