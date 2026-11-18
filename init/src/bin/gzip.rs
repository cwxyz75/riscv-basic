//! `gzip`: GNU gzip 1.13 (`third_party/fetched/gzip`, unmodified; fetched by `make fetch-third-party`), compiled with a RISC-V
//! clang against our headers and the small shims in `third_party/compat`, and linked with the C library. Without the fetched
//! source, or without a RISC-V C compiler, this is a stub that exits 99.
#![no_std]
#![no_main]

#[cfg(have_gzip)]
#[no_mangle]
pub fn app_main() -> i32 {
    oslibc::run_main()
}

#[cfg(not(have_gzip))]
#[no_mangle]
pub fn app_main() -> i32 {
    // (a reference to `rt` keeps its runtime and panic handler linked into the stub)
    let _ = rt::sys::clock_get();
    99
}
