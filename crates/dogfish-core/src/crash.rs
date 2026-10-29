//! Crash injection for the data-safety tests (debug builds only).
//!
//! Every step of `cp`, `mv`, `rm --safe` and `undo` that changes the file
//! system or the undo journal calls [`crash_point`] first. With
//! `DOGFISH_CRASH_AT=<n>` in the environment, a debug build kills itself with
//! `SIGKILL` at the n-th such step, the way a power cut or `kill -9` would:
//! no destructors run and nothing is flushed. Release builds compile the hook
//! to nothing and ignore the variable.

/// Kills the process at the n-th call when `DOGFISH_CRASH_AT=<n>` is set.
#[cfg(debug_assertions)]
pub fn crash_point() {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};

    static AT: OnceLock<Option<u64>> = OnceLock::new();
    static STEPS: AtomicU64 = AtomicU64::new(0);
    let Some(at) = *AT.get_or_init(|| std::env::var("DOGFISH_CRASH_AT").ok()?.parse().ok()) else {
        return;
    };
    if STEPS.fetch_add(1, Ordering::Relaxed) + 1 == at {
        // SAFETY: getpid and kill have no memory-safety preconditions.
        unsafe {
            libc::kill(libc::getpid(), libc::SIGKILL);
        }
    }
}

/// Release builds: no crash injection.
#[cfg(not(debug_assertions))]
#[inline(always)]
pub fn crash_point() {}
