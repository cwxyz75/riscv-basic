//! A user program that misbehaves on purpose (S3b): the kernel must end it, with the right
//! exit code, and keep running. The behaviour is chosen by the start argument in `a0`.
#![no_std]
#![no_main]

use core::arch::asm;

#[no_mangle]
pub fn app_main() -> i32 {
    match rt::start_arg() {
        1 => {
            // store into kernel memory: the direct map (read-write for the kernel, but with
            // no user bit, so only the U bit stands between this store and kernel RAM)
            // SAFETY: deliberately invalid; the kernel must refuse and end this process.
            unsafe { core::ptr::write_volatile(0xffff_ffc0_8800_0000usize as *mut u64, 1) };
        }
        2 => {
            // load from kernel memory
            // SAFETY: as above.
            let v = unsafe { core::ptr::read_volatile(0xffff_ffff_8020_0000usize as *const u64) };
            return v as i32;
        }
        3 => {
            // store into my own (read-only, executable) text
            // SAFETY: as above.
            unsafe { core::ptr::write_volatile(app_main as *const () as *mut u8, 0) };
        }
        4 => {
            // jump into non-executable data
            static DATA: [u32; 4] = [0; 4];
            // SAFETY: as above.
            unsafe { asm!("jr {}", in(reg) DATA.as_ptr() as usize) };
        }
        5 => {
            // an instruction user mode may not run
            // SAFETY: as above.
            unsafe { asm!("csrr t0, sstatus") };
        }
        6 => {
            // jump to an address nothing maps
            // SAFETY: as above.
            unsafe { asm!("jr {}", in(reg) 0x30_0000_0000usize) };
        }
        7 => {
            // load through the null pointer
            // SAFETY: as above.
            let v = unsafe { core::ptr::read_volatile(core::ptr::null::<u64>()) };
            return v as i32;
        }
        8 => {
            // exit normally with a chosen code
            return 42;
        }
        10 => {
            // jump into kernel text
            // SAFETY: as above.
            unsafe { asm!("jr {}", in(reg) 0xffff_ffff_8020_0000usize) };
        }
        11 => {
            // the stack pointer points into the kernel when the trap happens: the kernel must
            // not touch it (it must switch stacks), and the syscall must still work
            // SAFETY: `ecall` with sp pointing at kernel memory; the program uses no stack
            // until it exits, and the exit code (7) proves the syscall ran.
            unsafe {
                asm!("mv sp, {k}", "li a7, 1", "li a0, 7", "ecall", k = in(reg) 0xffff_ffff_8020_0000usize, options(noreturn))
            };
        }
        12 => {
            // run off the bottom of the stack into the guard page
            fn down(n: usize) -> usize {
                if n == usize::MAX {
                    return 0; // never reached; keeps the recursion from being provably endless
                }
                let pad = [n as u8; 4096];
                core::hint::black_box(&pad);
                down(n + 1) + core::hint::black_box(pad[n % 4096]) as usize
            }
            return down(0) as i32;
        }
        14 => {
            // a polite child: wait for one message on the bootstrap channel, echo it back
            // reversed, exit with 3
            let mut buf = [0u8; 64];
            rt::sys::object_wait(rt::BOOTSTRAP, rt::abi::signals::READABLE, u64::MAX);
            let r = rt::sys::channel_read(
                rt::BOOTSTRAP,
                buf.as_mut_ptr(),
                64,
                core::ptr::null_mut(),
                0,
            );
            if r.status != 0 {
                return 100;
            }
            buf[..r.a1].reverse();
            rt::sys::channel_write(rt::BOOTSTRAP, buf.as_ptr(), r.a1, core::ptr::null(), 0);
            return 3;
        }
        15 => {
            // spin forever in user mode (only a kill ends this)
            loop {
                core::hint::spin_loop();
            }
        }
        16 => {
            // block forever in a wait (only a kill ends this)
            rt::sys::object_wait(rt::BOOTSTRAP, rt::abi::signals::USER_MASK, u64::MAX);
            return 9;
        }
        n if n & 0xff == 13 => {
            // The same isolation check, switching processes through the `thread_yield`
            // syscall (the other way a process loses the CPU, so the other return path).
            let marker = (n >> 8) as u64;
            let mut cell = marker;
            let until = rt::sys::clock_get() + 25;
            while rt::sys::clock_get() < until {
                rt::sys::thread_yield();
                if core::hint::black_box(unsafe { core::ptr::read_volatile(&cell) }) != marker {
                    return 1;
                }
                // SAFETY: a plain volatile store to a local.
                unsafe { core::ptr::write_volatile(&mut cell, marker) };
            }
            return marker as i32;
        }
        n if n & 0xff == 9 => {
            // Memory isolation: every process has its own stack at the same address. Keep a
            // private marker there across timer preemption (other processes run meanwhile
            // under their own address spaces) and verify it never changes.
            let marker = (n >> 8) as u64;
            let mut cell = marker;
            let until = rt::sys::clock_get() + 25;
            let mut i = 0u64;
            // Mostly pure user-mode spinning (a syscall only every 4096 iterations), so the
            // timer interrupts land on user instructions and the process is resumed in the
            // middle of its own code, not at a syscall return.
            loop {
                i += 1;
                if core::hint::black_box(unsafe { core::ptr::read_volatile(&cell) }) != marker {
                    return 1; // someone else's memory (or a wrong address space) showed through
                }
                // SAFETY: a plain volatile store to a local.
                unsafe { core::ptr::write_volatile(&mut cell, marker) };
                if i.is_multiple_of(4096) && rt::sys::clock_get() >= until {
                    break;
                }
            }
            return marker as i32;
        }
        _ => {}
    }
    0
}
