//! `ctest`: the C test program (`libc/tests/c/transcript.c`) compiled with a RISC-V clang against our
//! headers and linked with the C library (S7c). Its output must equal the same program built and run
//! on the host. Without a RISC-V C compiler at build time this is a stub that exits 99.
#![no_std]
#![no_main]

#[cfg(have_c)]
#[no_mangle]
pub fn app_main() -> i32 {
    oslibc::run_main()
}

#[cfg(not(have_c))]
#[no_mangle]
pub fn app_main() -> i32 {
    99
}
