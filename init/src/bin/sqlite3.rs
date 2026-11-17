//! `sqlite3`: the SQLite amalgamation and its shell (`third_party/fetched/sqlite`, unmodified; fetched by `make fetch-third-party`),
//! compiled with a RISC-V clang against our headers and linked with the C library: the database file is the C library's files, so it
//! is an independent test of the file system (journal files, `fsync`, advisory locks, `pread`/`pwrite`). Without the fetched source,
//! or without a RISC-V C compiler, this is a stub that exits 99.
#![no_std]
#![no_main]

#[cfg(have_sqlite)]
#[no_mangle]
pub fn app_main() -> i32 {
    oslibc::run_main()
}

#[cfg(not(have_sqlite))]
#[no_mangle]
pub fn app_main() -> i32 {
    // (a reference to `rt` keeps its runtime and panic handler linked into the stub)
    let _ = rt::sys::clock_get();
    99
}
