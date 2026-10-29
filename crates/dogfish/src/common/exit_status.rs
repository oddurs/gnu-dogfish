//! Exit statuses env and timeout use for their own failures.

/// GNU `env` reports its own failures with status 125.
pub(crate) const EXIT_CANCELED: i32 = 125;
pub(crate) const EXIT_CANNOT_INVOKE: i32 = 126;
pub(crate) const EXIT_ENOENT: i32 = 127;
