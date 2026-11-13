//! `<stdio.h>`: the `printf` family over `cfmt` (the formatting half; `file.rs` has `FILE` and the
//! stream functions).
//!
//! The variadic entry points (`printf`, `fprintf`, `snprintf`, `sprintf`) are assembly stubs: stable
//! Rust cannot define a variadic function. On lp64 the variadic arguments arrive in `a0`-`a7` and
//! then on the caller's stack; a stub stores `a0`-`a7` in a save area directly below its entry stack
//! pointer, so that the save area and the caller's stack arguments form one run of 8-byte slots, and
//! tail-calls the `v*` function with a pointer to the first variadic slot as the `va_list` (RISC-V's
//! `va_list` is exactly such a pointer).

use crate::errno;
use cfmt::{Args, Len};
use core::arch::global_asm;
use core::ffi::{c_char, c_int};

macro_rules! variadic {
    ($name:literal, $target:literal, $nfixed:literal, $offset:literal) => {
        global_asm!(concat!(
            ".section .text.",
            $name,
            ",\"ax\",@progbits\n",
            ".globl ",
            $name,
            "\n",
            $name,
            ":\n",
            "    addi sp, sp, -80\n",
            "    sd ra, 0(sp)\n",
            "    sd a0, 16(sp)\n",
            "    sd a1, 24(sp)\n",
            "    sd a2, 32(sp)\n",
            "    sd a3, 40(sp)\n",
            "    sd a4, 48(sp)\n",
            "    sd a5, 56(sp)\n",
            "    sd a6, 64(sp)\n",
            "    sd a7, 72(sp)\n",
            "    addi a",
            $nfixed,
            ", sp, ",
            $offset,
            "\n",
            "    call ",
            $target,
            "\n",
            "    ld ra, 0(sp)\n",
            "    addi sp, sp, 80\n",
            "    ret\n",
        ));
    };
}

variadic!("printf", "vprintf", "1", "24");
variadic!("fprintf", "vfprintf", "2", "32");
variadic!("sprintf", "vsprintf", "2", "32");
variadic!("snprintf", "vsnprintf", "3", "40");

/// The arguments of a `va_list`: 8-byte slots.
struct Va(*const u64);

impl Va {
    fn next(&mut self) -> u64 {
        // SAFETY: the C caller guarantees the argument exists (a `printf` contract).
        unsafe {
            let v = *self.0;
            self.0 = self.0.add(1);
            v
        }
    }
}

impl Args for Va {
    fn int(&mut self, _: Len) -> i64 {
        self.next() as i64
    }
    fn uint(&mut self, _: Len) -> u64 {
        self.next()
    }
    fn double(&mut self) -> f64 {
        f64::from_bits(self.next())
    }
    fn ptr(&mut self) -> u64 {
        self.next()
    }
    fn string(&mut self, max: Option<usize>) -> Option<alloc::vec::Vec<u8>> {
        let p = self.next() as *const u8;
        if p.is_null() {
            return None;
        }
        let mut v = alloc::vec::Vec::new();
        let mut i = 0;
        // SAFETY: `%s` names a C string: valid up to its NUL, or for `max` bytes.
        unsafe {
            while max.is_none_or(|m| i < m) && *p.add(i) != 0 {
                v.push(*p.add(i));
                i += 1;
            }
        }
        Some(v)
    }
}

pub(crate) fn render(fmt: *const c_char, ap: *const u64) -> Option<alloc::vec::Vec<u8>> {
    // SAFETY: `fmt` is a C string.
    let f = unsafe { core::ffi::CStr::from_ptr(fmt) }.to_bytes();
    let mut out = alloc::vec::Vec::new();
    match cfmt::format(f, &mut Va(ap), &mut out) {
        Ok(()) => Some(out),
        Err(_) => {
            errno::set(errno::EINVAL);
            None
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn vsnprintf(
    buf: *mut c_char,
    n: usize,
    fmt: *const c_char,
    ap: *const u64,
) -> c_int {
    let Some(out) = render(fmt, ap) else {
        return -1;
    };
    if n > 0 {
        let k = out.len().min(n - 1);
        core::ptr::copy_nonoverlapping(out.as_ptr(), buf as *mut u8, k);
        *buf.add(k) = 0;
    }
    out.len() as c_int
}

#[no_mangle]
pub unsafe extern "C" fn vsprintf(buf: *mut c_char, fmt: *const c_char, ap: *const u64) -> c_int {
    vsnprintf(buf, usize::MAX >> 1, fmt, ap)
}
