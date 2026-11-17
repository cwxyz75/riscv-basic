//! `kilo`: antirez's small terminal text editor (`third_party/fetched/kilo`, unmodified; fetched by `make fetch-third-party`),
//! compiled with a RISC-V clang against our headers and linked with the C library. It needs the terminal for programs (raw mode,
//! the window size, a read with a timeout: T124). Without the fetched source, or without a RISC-V C compiler, this is a stub that
//! exits 99.
#![no_std]
#![no_main]

#[cfg(have_kilo)]
#[no_mangle]
pub fn app_main() -> i32 {
    oslibc::run_main()
}

#[cfg(not(have_kilo))]
#[no_mangle]
pub fn app_main() -> i32 {
    // (a reference to `rt` keeps its runtime and panic handler linked into the stub)
    let _ = rt::sys::clock_get();
    99
}
