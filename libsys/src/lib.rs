//! The system library (AB7, KERNEL-V1-SPEC 3.1).
//!
//! `libsys.so` is a small shared object the kernel embeds and maps, text and data page, into every process at a random place. It holds one
//! `sys_<name>` wrapper per call in `abi::syscall::TABLE` (`li a7, N; ecall; ret`), a `sys_raw` that takes the number in `a6`, and a read-only
//! data page ([`page`]) the kernel fills. It has no relocations at all: its code is PC-relative. A program finds the wrappers by walking the
//! object's `.dynsym` ([`Image`]), starting from the base the kernel passes in `a2` of the first thread.
//!
//! The crate has three parts: the layout and the symbol walker (`no_std`, used by `rt` and the kernel), the builder (feature `build`: it
//! generates the object's source and runs `rustc`), and the checker.
#![cfg_attr(not(any(feature = "build", test)), no_std)]
#![forbid(unsafe_code)]

#[cfg(any(feature = "build", test))]
extern crate std;

pub mod image;
pub mod page;

pub use image::Image;

/// Is `symbol` the wrapper of the call `constant` names (`abi::syscall::TABLE`'s name): `sys_` and the constant in lower case. No allocation.
pub fn is_wrapper_of(symbol: &str, constant: &str) -> bool {
    symbol.strip_prefix("sys_").is_some_and(|rest| {
        rest.len() == constant.len()
            && rest
                .bytes()
                .zip(constant.bytes())
                .all(|(r, c)| r == c.to_ascii_lowercase())
    })
}

/// The name of the wrapper for a call: the constant's name in lower case, after `sys_`.
#[cfg(any(feature = "build", test))]
pub fn symbol_name(constant: &str) -> std::string::String {
    let mut s = std::string::String::from("sys_");
    s.extend(constant.chars().map(|c| c.to_ascii_lowercase()));
    s
}

#[cfg(feature = "build")]
pub mod build;
