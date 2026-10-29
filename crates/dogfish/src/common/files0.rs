//! `--files0-from=F`: file operands read as NUL-terminated names from F (or
//! standard input for `-`), with GNU's diagnostics. Shared by du, wc and
//! sort (gnulib `readtokens0` and `argv_iter`). Names are bytes underneath
//! ([`Files0From::next_os_name`]), so du can use names that are not UTF-8.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use crate::common::quote::{qa, qf};

/// Lists of at most this size are read whole, so their length is known in
/// advance (GNU: 10 MiB or half the available memory).
const READ_WHOLE_LIMIT: u64 = 10 * 1024 * 1024;

/// An open `--files0-from` list.
pub(crate) struct Files0From {
    /// The option's argument, for messages.
    pub(crate) name: OsString,
    reader: Box<dyn BufRead>,
    is_regular: bool,
    size: u64,
    /// Names read so far (`argv_iter_n_args`).
    count: u64,
}

impl Files0From {
    /// Opens the list; `Err` carries the open error.
    pub(crate) fn open(name: impl AsRef<OsStr>) -> io::Result<Self> {
        let name = name.as_ref();
        let (reader, meta): (Box<dyn Read>, _) = if name.as_bytes() == b"-" {
            let stdin = crate::common::input::stdin_file();
            let meta = stdin.metadata().ok();
            (Box::new(io::stdin()), meta)
        } else {
            let file = File::open(name)?;
            let meta = file.metadata().ok();
            (Box::new(file), meta)
        };
        Ok(Self {
            name: name.to_os_string(),
            reader: Box::new(BufReader::new(reader)),
            is_regular: meta.as_ref().is_some_and(|m| m.is_file()),
            size: meta.map_or(0, |m| m.len()),
            count: 0,
        })
    }

    /// GNU's message for a list that cannot be opened (du).
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

    /// Whether GNU's wc would read the whole list up front (a small regular
    /// file), and so know how many names there are.
    pub(crate) fn is_small_regular(&self) -> bool {
        self.is_regular && self.size <= READ_WHOLE_LIMIT
    }

    /// How many names have been read so far.
    pub(crate) fn count(&self) -> u64 {
        self.count
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

    /// The next name as text (bytes that are not UTF-8 replaced), `None`
    /// at the end of the list.
    pub(crate) fn next_name(&mut self) -> io::Result<Option<String>> {
        Ok(self
            .next_os_name()?
            .map(|n| n.to_string_lossy().into_owned()))
    }

    /// The next name, byte for byte, `None` at the end of the list.
    pub(crate) fn next_os_name(&mut self) -> io::Result<Option<OsString>> {
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
    /// the list itself is standard input (`stdin_only`), or an empty name.
    pub(crate) fn check_name(
        &self,
        name: impl AsRef<OsStr>,
        number: u64,
        stdin_only: bool,
    ) -> Option<String> {
        let name = name.as_ref();
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
pub(crate) fn extra_operand_error(tool: &str, operand: impl AsRef<OsStr>, status: i32) -> i32 {
    eprintln!(
        "{tool}: extra operand {}\nfile operands cannot be combined with --files0-from\n\
         Try '{tool} --help' for more information.",
        qa(operand)
    );
    status
}
