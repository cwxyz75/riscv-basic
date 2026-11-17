//! `busybox`: BusyBox 1.36.1 (UV8), the subset in `third_party/busybox/config`, compiled with a RISC-V clang against our headers
//! and linked with the C library. The applet is chosen by the program's name or by the first argument (`busybox wc`). Without
//! the fetched sources or a RISC-V C compiler at build time this is a stub that exits 99.
#![no_std]
#![no_main]

#[cfg(have_busybox)]
#[no_mangle]
pub fn app_main() -> i32 {
    oslibc::run_main()
}

#[cfg(not(have_busybox))]
#[no_mangle]
pub fn app_main() -> i32 {
    99
}
