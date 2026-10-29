//! Standard output for tools that write their own large blocks.

use std::fs::File;
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;

/// Standard output without Rust's line buffering (which looks for the last
/// newline in every write and splits it there): each block a tool hands it
/// goes out in one `write`. Nothing else may have buffered output for fd 1.
pub(crate) fn raw_stdout() -> ManuallyDrop<File> {
    // Safety: fd 1 stays open for the life of the process, and ManuallyDrop
    // keeps this handle from closing it.
    ManuallyDrop::new(unsafe { File::from_raw_fd(1) })
}
