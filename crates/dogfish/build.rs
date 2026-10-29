//! Links GCC's unwinder statically on GNU/Linux.
//!
//! Rust's standard library refers to the `_Unwind_*` functions (backtraces,
//! and unwinding in test builds) and links them from `libgcc_s.so.1`, so
//! every run of every tool made the dynamic loader find, map and relocate
//! one more shared library: 9 system calls, about 9 page faults and 30,000
//! instructions before `main`. Naming `libgcc_eh.a` (the same unwinder, as
//! an archive) here puts it on the link line ahead of `-lgcc_s`; the linker
//! takes the functions from it, and `--as-needed` then drops `libgcc_s`.
//! This is what Rust itself does for `+crt-static` builds.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CC");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os != "linux" || target_env != "gnu" || !have_libgcc_eh() {
        return;
    }
    println!("cargo:rustc-link-lib=static:-bundle=gcc_eh");
}

/// Whether the C compiler that links the binary has `libgcc_eh.a` (GCC's
/// does; a toolchain without it keeps the shared `libgcc_s`).
fn have_libgcc_eh() -> bool {
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    Command::new(cc)
        .arg("-print-file-name=libgcc_eh.a")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        // An unknown file is echoed back by name, without a directory.
        .is_some_and(|path| path.contains('/'))
}
