//! KT8, the guest half: the syscall fuzzer inside the machine. The parent (the boot-time suite) starts this program in a small job with a seed as its
//! start argument and a handle to that job on the bootstrap channel; the program runs the same generator as the host fuzzer (`kfuzz-gen`) against the real
//! kernel: every call goes through the system library, a window of its own memory is the buffer the generated pointers point into, and what the calls
//! hand back (handles from `channel_read`) goes into the model. No call may panic the kernel (a panic is the boot test's `##PANIC`); the parent then kills
//! the job and checks that everything the program held comes back (the job's counters, the kernel heap).
//!
//! The exit code is 0 when the sequence ran to its end. A program that closed a handle of its own runtime by chance may fault instead: the parent
//! does not mind, the kernel's survival and the baseline are the checks.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use kfuzz_gen::{Gen, Kind, Model, Window};

/// Calls per sequence when the parent does not say otherwise (the high half of the start argument).
const CALLS: usize = 300;
const WINDOW: usize = 0x10000;

#[no_mangle]
pub fn app_main() -> i32 {
    let arg = rt::start_arg();
    let (seed, calls) = (
        (arg & 0xffff_ffff) as u64,
        match arg >> 32 {
            0 => CALLS,
            n => n,
        },
    );
    // the job handle the parent sent: the job calls act on it (the generator never closes it or sets limits on it)
    let mut job = [0u64; 1];
    let mut tries = 0;
    loop {
        let mut byte = [0u8; 1];
        let r = rt::sys::channel_read(rt::BOOTSTRAP, byte.as_mut_ptr(), 1, job.as_mut_ptr(), 1);
        if r.status == 0 && r.a2 == 1 {
            break;
        }
        tries += 1;
        if tries > 500 {
            return 90;
        }
        rt::io::nap(2);
    }
    // the window of user memory the generated pointers point into (read-write), and a read-only page past it
    let mut buf: Vec<u8> = vec![0u8; WINDOW + 4096];
    let base = (buf.as_mut_ptr() as u64 + 4095) & !4095;
    let ro = {
        let v = rt::sys::vmo_create(1);
        if v.status != 0 {
            return 91;
        }
        let me = rt::sys::process_self();
        if me.status != 0 {
            return 92;
        }
        let m = rt::sys::process_map_anywhere(me.a1 as u64, v.a1 as u64, 0, 4096, 1);
        if m.status != 0 {
            return 93;
        }
        m.a1 as u64
    };
    let window = Window {
        base,
        len: WINDOW as u64,
        read_only: ro,
    };
    let mut model = Model::with(&[(job[0], Kind::Job)]);
    model.pin(job[0]);
    let mut gen = Gen::new(seed, window);
    for _ in 0..calls {
        let call = gen.next(&model);
        for (p, bytes) in &call.pokes {
            if *p >= base && *p + bytes.len() as u64 <= base + WINDOW as u64 {
                // SAFETY: the destination lies inside `buf` (checked just above) and the source is a separate allocation of `bytes.len()` bytes.
                unsafe {
                    core::ptr::copy_nonoverlapping(bytes.as_ptr(), *p as *mut u8, bytes.len());
                }
            }
        }
        let a = call.args;
        let r = rt::syscall(
            call.nr,
            [
                a[0] as usize,
                a[1] as usize,
                a[2] as usize,
                a[3] as usize,
                a[4] as usize,
                a[5] as usize,
            ],
        );
        let ok = r.status == 0;
        model.observe(&call, ok, Some(r.a1 as u64), Some(r.a2 as u64));
        if ok
            && matches!(
                call.nr,
                abi::syscall::CHANNEL_READ | abi::syscall::CHANNEL_READ_TXID
            )
        {
            let list = if call.nr == abi::syscall::CHANNEL_READ {
                a[3]
            } else {
                a[4]
            };
            let n = (r.a2).min(64);
            if n > 0 && list >= base && list + 8 * n as u64 <= base + WINDOW as u64 {
                let mut got: Vec<(u64, Kind)> = Vec::new();
                for i in 0..n {
                    // SAFETY: inside `buf`, as checked above; the kernel wrote the handle list there.
                    let h = unsafe { core::ptr::read_unaligned((list as *const u64).add(i)) };
                    got.push((h, Kind::Any));
                }
                model.received(&got);
            }
        }
    }
    0
}
