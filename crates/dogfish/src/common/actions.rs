//! `-J` for the tools that change files (cp, mv, rm, mkdir, ln, chmod,
//! chown, touch): one record per action, written as it completes, the
//! structured form of `-v` (`doc/ndjson.md`, "Action records").
//!
//! With `-J` a tool decides what to report exactly as `-v` does, and writes
//! a record where `-v` would print a line; the `-v` text is not written.
//! Diagnostics become error records in the same stream (see
//! `dogfish_core::ndjson::diagnostic`).
//!
//! The writer is global so that the deep call chains that report actions
//! (the copy engine, the tree walks) need no new parameter; the flag check
//! [`on`] is one relaxed load, so the path without `-J` does not pay for it.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};

use dogfish_core::{Record, RecordWriter};

static ON: AtomicBool = AtomicBool::new(false);

thread_local! {
    static OUT: RefCell<Option<RecordWriter>> = const { RefCell::new(None) };
}

/// Starts writing action records to standard output (`-J` was given).
pub(crate) fn start() {
    OUT.with(|o| *o.borrow_mut() = Some(RecordWriter::stdout()));
    ON.store(true, Ordering::Relaxed);
}

/// Whether actions are reported as records.
#[inline]
pub(crate) fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Writes the record of one action: `action`, then the fields `fill` adds.
/// A write error is sticky on standard output and reported at exit.
pub(crate) fn record(action: &str, fill: impl FnOnce(&mut Record<'_>)) {
    OUT.with(|o| {
        if let Some(w) = o.borrow_mut().as_mut() {
            let _ = w.record(|r| {
                r.str("action", action);
                fill(r);
            });
        }
    });
}

/// Writes out the records still buffered (with the error records queued
/// behind them); called once the tool has finished.
pub(crate) fn finish() {
    ON.store(false, Ordering::Relaxed);
    if let Some(mut w) = OUT.with(|o| o.borrow_mut().take()) {
        let _ = w.flush();
    }
}
