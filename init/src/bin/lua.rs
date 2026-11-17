//! `lua`: the upstream Lua 5.4 interpreter (`third_party/lua/src`, unmodified) compiled with a RISC-V
//! clang against our headers and linked with the C library (S7d). Without a RISC-V C compiler at
//! build time this is a stub that exits 99.
#![no_std]
#![no_main]

#[cfg(have_lua)]
#[no_mangle]
pub fn app_main() -> i32 {
    oslibc::run_main()
}

#[cfg(not(have_lua))]
#[no_mangle]
pub fn app_main() -> i32 {
    // (a reference to `rt` keeps its runtime and panic handler linked into the stub)
    let _ = rt::sys::clock_get();
    99
}
