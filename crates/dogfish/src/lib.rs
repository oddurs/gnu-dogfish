//! Not a library: the `dogfish` binary is the product. With the `fuzzing`
//! feature, this target compiles the binary's modules once more so the fuzz
//! targets in `fuzz/` can call the parsers through [`fuzz_api`]. Without the
//! feature it is empty, and the binary never uses it.

#![cfg(feature = "fuzzing")]
// The tools' entry points are not called from here.
#![allow(dead_code, unused_imports)]

/// As in `main.rs`: diagnostics print as glibc's `error` does.
macro_rules! eprintln {
    ($($arg:tt)*) => {
        dogfish_core::gnu_error!($($arg)*)
    };
}

mod common;
mod tools;

#[doc(hidden)]
pub mod fuzz_api;
