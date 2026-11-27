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
            // jump to an address nothing maps (the last page of the user half: above the stack arena nothing is placed, T272)
            // SAFETY: as above.
            unsafe { asm!("jr {}", in(reg) rt::abi::user::TOP as usize - 4096) };
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
        84 => {
            // AB8: a system call from the program's own text, not from the system library: an illegal instruction (the process ends with
            // -1002); were it let through, the process would exit with 5
            // SAFETY: the `ecall` is meant to be refused; with the check off it is `process_exit(5)`.
            unsafe {
                asm!("ecall", in("a7") rt::abi::syscall::PROCESS_EXIT, in("a0") 5usize, options(noreturn))
            };
        }
        83 => {
            // HD4: a program started with no startup message has no debug resource, and is not privileged: it can neither write a verdict
            // line to the console nor ask for the resource (exit code 0: both refused as they should be)
            let w = rt::sys::debug_write(b"##TEST forged_by_a_confined_child PASS");
            let r = rt::sys::debug_root();
            let ok = w.status == rt::abi::status::Status::BadHandle as i32
                && r.status == rt::abi::status::Status::AccessDenied as i32;
            return i32::from(!ok);
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
                asm!("mv sp, {k}", "li a0, 7", "jr t1", k = in(reg) 0xffff_ffff_8020_0000usize, in("t1") rt::library::entry(rt::abi::syscall::PROCESS_EXIT), options(noreturn))
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
        80 => {
            // spin calling `clock_get` (L12c: a hart running this should take no big kernel lock for it)
            loop {
                core::hint::black_box(rt::sys::clock_get());
            }
        }
        82 => {
            // spin calling `futex_wake` on a word nobody sleeps on (L14)
            static WORD: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
            loop {
                let _ = rt::sys::futex_wake(WORD.as_ptr(), 1);
            }
        }
        81 => {
            // spin calling `thread_yield` (L12c)
            loop {
                let _ = rt::sys::thread_yield();
            }
        }
        16 => {
            // block forever in a wait (only a kill ends this)
            rt::sys::object_wait(rt::BOOTSTRAP, rt::abi::signals::USER_MASK, u64::MAX);
            return 9;
        }
        17 => {
            // a spinning second thread; the first thread leaves, the process lives on
            spawn(spin_forever, 0);
            rt::sys::thread_exit(5);
        }
        18 => {
            // a second thread ends the whole process with exit(9) after a short wait; the first
            // thread spins and is stopped by it
            spawn(exit_soon, 0);
            loop {
                core::hint::spin_loop();
            }
        }
        19 => {
            // a second thread faults (null load): the whole process dies with the fault code
            spawn(fault, 0);
            rt::sys::object_wait(rt::BOOTSTRAP, rt::abi::signals::USER_MASK, u64::MAX);
            return 9;
        }
        20 => {
            // a second thread exits with 4; the first waits for it, checks what it left, then
            // exits with 6, the last thread's code and so the process's
            let h = spawn(thread_exit_four, 0);
            let r = rt::sys::object_wait(h, rt::abi::signals::TERMINATED, u64::MAX);
            if r.status != 0 || rt::sys::thread_info(h).a1 != 4 {
                return 100;
            }
            rt::sys::thread_exit(6);
        }
        21 => {
            // hog: seven spinning threads (a test starts many of these)
            for _ in 0..7 {
                spawn(spin_forever, 0);
            }
            loop {
                core::hint::spin_loop();
            }
        }
        25 => {
            // block forever while the parent adds a thread on a stack it mapped ANYWHERE
            // (`child_with_a_ready_exit_thread`). This used to touch 4 pages at the fixed
            // 0x20_0000_0000 first, which since HD2 nobody maps: a child the parent was slow to
            // kill would have faulted there (T272).
            rt::sys::object_wait(rt::BOOTSTRAP, rt::abi::signals::USER_MASK, u64::MAX);
            return 9;
        }
        121 => return hostile_driver(),
        26 => {
            // take a socket end from the bootstrap channel and drain it to the end of the
            // stream; the exit code is how many bytes arrived
            use rt::abi::status::Status;
            let mut hbuf = [0u64; 1];
            let mut mbuf = [0u8; 8];
            rt::sys::object_wait(rt::BOOTSTRAP, rt::abi::signals::READABLE, u64::MAX);
            let r =
                rt::sys::channel_read(rt::BOOTSTRAP, mbuf.as_mut_ptr(), 8, hbuf.as_mut_ptr(), 1);
            if r.status != 0 || r.a2 != 1 {
                return 100;
            }
            let h = hbuf[0];
            let mut total = 0usize;
            let mut buf = [0u8; 64];
            loop {
                let r = rt::sys::socket_read(h, buf.as_mut_ptr(), 64);
                match r.status() {
                    Some(Status::Ok) => total += r.a1,
                    Some(Status::PeerClosed) => break,
                    Some(Status::ShouldWait) => {
                        rt::sys::object_wait(
                            h,
                            rt::abi::signals::READABLE | rt::abi::signals::PEER_CLOSED,
                            u64::MAX,
                        );
                    }
                    _ => return 101,
                }
            }
            return total as i32;
        }
        27 => {
            // wait on a port with nothing bound, forever (only a kill ends this)
            let port = rt::sys::port_create().a1 as u64;
            rt::sys::port_wait(port, u64::MAX);
            return 9;
        }
        22 => {
            // threads created but perhaps not yet run when the whole process exits
            for _ in 0..3 {
                spawn(spin_forever, 0);
            }
            rt::sys::exit(3);
        }
        23 => {
            // the first thread leaves at once; the process lives until the second ends
            // (the second thread waits a little first: with several harts it could otherwise end
            // before this one, and then *this* one would be the last thread and set the code)
            spawn(exit_eight_late, 0);
            rt::sys::thread_exit(1);
        }
        24 => {
            // create short-lived threads in a loop until killed
            let me = rt::sys::process_self().a1 as u64;
            let top = core::ptr::addr_of_mut!(STACK) as u64 + 8192;
            loop {
                let r =
                    rt::sys::thread_on_stack(me, exit_eight as *const () as usize as u64, top, 0);
                if r.status == 0 {
                    rt::sys::handle_close(r.a1 as u64);
                }
            }
        }
        101 => {
            // MM7: a guard page: protect the second of two pages read-only, then write it: a clean fault ends the process
            let me = rt::sys::process_self().a1 as u64;
            let v = rt::sys::vmo_create(2);
            if v.status != 0 {
                return -1;
            }
            const BASE: usize = 0x560_0000;
            if rt::sys::process_map(me, v.a1 as u64, BASE as u64, 0, 2 * 4096, 3).status != 0 {
                return -2;
            }
            // SAFETY: both pages are mapped read-write in this process.
            unsafe { core::ptr::write_volatile((BASE + 4096) as *mut u8, 1) };
            if rt::sys::process_protect(me, (BASE + 4096) as u64, 4096, 1).status != 0 {
                return -3;
            }
            // SAFETY: deliberately a write to a page that is read-only now: the kernel must refuse and end this process.
            unsafe { core::ptr::write_volatile((BASE + 4096) as *mut u8, 2) };
            return 0;
        }
        110 => {
            // MM9: a reader of a pager's VMO. The parent sends the VMO; map it and read a byte of page 3 (which waits for the pager), and
            // exit with that byte.
            rt::sys::object_wait(rt::BOOTSTRAP, rt::abi::signals::READABLE, u64::MAX);
            let mut m = [0u8; 8];
            let mut h = [0u64; 1];
            let r = rt::sys::channel_read(rt::BOOTSTRAP, m.as_mut_ptr(), 8, h.as_mut_ptr(), 1);
            if r.status != 0 || r.a2 != 1 {
                return -1;
            }
            let me = rt::sys::process_self().a1 as u64;
            const BASE: usize = 0x560_0000;
            if rt::sys::process_map(me, h[0], BASE as u64, 0, 16 * 4096, 3).status != 0 {
                return -2;
            }
            // SAFETY: the 16 pages are mapped read-write in this process; the read waits for the pager to supply page 3.
            let b = unsafe { core::ptr::read_volatile((BASE + 3 * 4096 + 16) as *const u8) };
            return i32::from(b);
        }
        100 => {
            // MM6: shrink a resizable VMO under a mapping, then touch the part that went: a clean fault, the process ends
            let me = rt::sys::process_self().a1 as u64;
            let v = rt::sys::vmo_create_flags(8, 0, 1);
            if v.status != 0 {
                return -1;
            }
            let v = v.a1 as u64;
            const BASE: usize = 0x540_0000;
            if rt::sys::process_map(me, v, BASE as u64, 0, 8 * 4096, 3).status != 0 {
                return -2;
            }
            // SAFETY: the pages are mapped read-write in this process.
            unsafe { core::ptr::write_volatile((BASE + 6 * 4096) as *mut u8, 1) };
            if rt::sys::vmo_set_size(v, 4 * 4096).status != 0 {
                return -3;
            }
            // SAFETY: deliberately past the shrunk VMO's end: the kernel must refuse and end this process.
            unsafe { core::ptr::write_volatile((BASE + 6 * 4096) as *mut u8, 2) };
            return 0;
        }
        99 => {
            // MM5: kernel memory until the job says no. Make events and duplicate handles (every one an object body and a handle-table slot
            // the kernel holds for this process) until the job's kernel-byte limit refuses, then stay alive for the parent to measure.
            let ev = rt::sys::event_create();
            if ev.status != 0 {
                return -1;
            }
            let ev = ev.a1 as u64;
            let mut made = 0i32;
            loop {
                let e = rt::sys::event_create();
                if e.status != 0 {
                    break;
                }
                made += 1;
                let d = rt::sys::handle_duplicate(ev, 1u32 << 31);
                if d.status != 0 {
                    break;
                }
            }
            let _ = made;
            // (the job's kernel bytes are used up: a handle table that grows on without being charged would show here, up to its 20,000 handles)
            while rt::sys::handle_duplicate(ev, 1u32 << 31).status == 0 {}
            loop {
                rt::sys::thread_yield();
            }
        }
        30 => {
            // S4.5: a memory hog. Write one page after another (new VMOs as needed) until the
            // job's page limit says no; the exit code is how many pages it got.
            let mut pages = 0i32;
            loop {
                let v = rt::sys::vmo_create(256);
                if v.status != 0 {
                    return if v.status == -5 { pages } else { -1 };
                }
                let vmo = v.a1 as u64;
                let buf = [1u8; 4096];
                for page in 0..256u64 {
                    let w = rt::sys::vmo_write(vmo, page * 4096, buf.as_ptr(), 4096);
                    if w.status != 0 {
                        return if w.status == -5 { pages } else { -2 };
                    }
                    pages += 1;
                }
            }
        }
        31 => {
            // S4.5: no ambient authority over jobs. Bit 0: `job_root` is refused; bit 1: a job
            // handle made up from nothing is refused; bit 2: a process cannot be made when the
            // job allows none beyond this one (the parent sets processes = 1).
            let mut code = 0;
            if rt::sys::job_root().status == -3 {
                code |= 1;
            }
            if rt::sys::job_create(
                77,
                &[u64::MAX, u64::MAX, u64::MAX, u64::MAX, u64::MAX, u64::MAX],
            )
            .status
                == -1
            {
                code |= 2;
            }
            if rt::sys::process_create().status == -5 {
                code |= 4;
            }
            return code;
        }
        50 => {
            // S6a: no ambient device authority. Bit 0: `resource_root` is refused; bit 1: made-up
            // handles mint nothing; bit 2: neither does an event (a wrong object).
            let mut code = 0;
            if rt::sys::resource_root().status == -3 {
                code |= 1;
            }
            // (the RTC's page is a device window; `vmo_create_dma_in` is the DMA call since IOMMU12)
            if rt::sys::vmo_create_physical(77, 0x10_1000, 4096).status == -1
                && rt::sys::vmo_create_dma_in(77, 1, 0).status == -1
            {
                code |= 2;
            }
            let ev = rt::sys::event_create().a1 as u64;
            if rt::sys::vmo_create_physical(ev, 0x10_1000, 4096).status == -2
                && rt::sys::vmo_create_dma_in(ev, 1, 0).status == -2
            {
                code |= 4;
            }
            // IOMMU8: nor the PCI host's configuration space, a BAR in its memory window, or a function's domain
            if [(77, -1), (ev, -2)].iter().all(|&(h, e)| {
                rt::sys::vmo_create_physical(h, 0x3000_0000, 4096).status == e
                    && rt::sys::vmo_create_physical(h, 0x4000_0000, 4096).status == e
                    && rt::sys::iodomain_create(h, 0x28, 0).status == e
            }) {
                code |= 8;
            }
            return code;
        }
        32 => {
            // S4.5: a process bomb. Create processes until the job says no; the exit code is how
            // many it made (their handles are kept: they never start, so they cost only slots).
            let mut made = 0;
            loop {
                let r = rt::sys::process_create();
                if r.status != 0 {
                    return if r.status == -5 { made } else { -1 };
                }
                made += 1;
            }
        }
        33 => {
            // S4.5: a thread bomb. Threads on two static stacks: the second must be refused when
            // the job allows only this thread plus one more.
            let me = rt::sys::process_self().a1 as u64;
            let top1 = core::ptr::addr_of_mut!(STACK) as u64 + 8192;
            let first =
                rt::sys::thread_on_stack(me, spin_forever as *const () as usize as u64, top1, 0);
            let second =
                rt::sys::thread_on_stack(me, spin_forever as *const () as usize as u64, top1, 0);
            let mut code = 0;
            if first.status == 0 {
                code |= 1;
            }
            if second.status == -5 {
                code |= 2;
            }
            return code;
        }
        34 => {
            // S4.5c: an IPC hog. Make socket pairs and fill them with data until the job's budget
            // says no; the exit code is how many KiB it managed to queue.
            let mut queued = 0usize;
            let block = [3u8; 1024];
            loop {
                let pair = rt::sys::socket_create();
                if pair.status != 0 {
                    return if pair.status == -5 {
                        (queued / 1024) as i32
                    } else {
                        -1
                    };
                }
                for _ in 0..32 {
                    let w = rt::sys::socket_write(pair.a1 as u64, block.as_ptr(), block.len());
                    if w.status != 0 {
                        return if w.status == -5 {
                            (queued / 1024) as i32
                        } else {
                            -2
                        };
                    }
                    queued += w.a1;
                }
            }
        }
        35 => {
            // S4.5c: an object bomb. Make events until the job's object quota says no; the exit
            // code is how many it made.
            let mut made = 0;
            loop {
                let e = rt::sys::event_create();
                if e.status != 0 {
                    return if e.status == -5 { made } else { -1 };
                }
                made += 1;
            }
        }
        // S9 step 1 (the stream data-path prototype, S9-SPEC 4.7): a socket hand. It obeys commands on the bootstrap channel and
        // keeps one socket end: 'C' create a pair, keep one end and send the other back (the reply carries the handle);
        // 'H' take the end that comes with the message; 'W' n: write 4*n KiB into the kept end (reply: bytes written, the last
        // status, the end's signals); 'R' drain the kept end (reply: bytes read); 'X' exit.
        70 => return socket_hand(),
        40 => return unmap_while_siblings_touch(),
        102 => return decommit_while_siblings_touch(),
        103 => return protect_while_siblings_write(),
        n if n & 0xff == 42 => return churn((n >> 8) as u64),
        // slice A (OD76): floating-point state. 60: keep a private FP pattern across preemption and
        // yields (mode in bits 8-15, seed above); 61: read every FP register at first use (all zero
        // expected, whatever other processes left in the unit); 62: three threads in one process, each
        // with its own pattern.
        n if n & 0xff == 60 => return fp_keep((n >> 16) as u64, ((n >> 8) & 0xff) as u64),
        61 => return fp_fresh(),
        // PL2: the vector twins. 92: keep a private vector state across preemption and yields (mode in bits 8-15, seed above); 93: read every
        // vector register at first use (all zero); 94: three threads, each with its own pattern; 95: use the vector unit, then run an
        // instruction user mode may not: the process dies
        n if n & 0xff == 92 => return vec_keep((n >> 16) as u64, ((n >> 8) & 0xff) as u64, 0),
        93 => return vec_fresh(),
        96 => return vec_edges(),
        // PL3: the user build is RVA23U64: a vector loop and `cbo.zero` over a page with no `.option` (the target features enable them)
        98 => return rva23_loop_and_cbo(),
        97 => return vec_load_first(),
        94 => return vec_threads(),
        95 => {
            let _ = vlenb();
            // SAFETY: as variant 5: traps and ends the process.
            unsafe { asm!("csrr t0, sstatus") };
        }
        62 => return fp_threads(),
        // 63: use FP, then run an instruction user mode may not: the process dies (the unit being
        // enabled must not turn a genuine illegal instruction into anything else)
        63 => {
            let mut got = [0u64; 32];
            fp_store(&mut got);
            // SAFETY: as variant 5: traps and ends the process.
            unsafe { asm!("csrr t0, sstatus") };
        }
        // 64: take our own process handle (every runtime program does, on its first heap growth), then
        // spin: the parent closing its handle must still kill us (review 2026-11-14: it did not)
        64 => {
            let _ = rt::sys::process_self();
            loop {
                core::hint::spin_loop();
            }
        }
        n if n & 0xff == 51 => {
            // S7b: a wake from another process on the same address number wakes nobody: the exit
            // code is 100 plus the number of threads it woke (0 expected), 1 if the call failed.
            let r = rt::sys::futex_wake((n >> 8) as *const u32, 10);
            return if r.status == 0 { 100 + r.a1 as i32 } else { 1 };
        }
        // 66: a confined program tries to raise its priority (slice SC): above its job's maximum is refused with
        // ACCESS_DENIED, within it works and reads back. Exit code 0 if all of that holds; else a code that says which.
        66 => {
            use rt::abi::status::Status as S;
            let denied = rt::sys::thread_set_sched(0, 63, 0).status;
            if denied != S::AccessDenied.code() {
                return 1;
            }
            if rt::sys::thread_set_sched(0, 12, 0).status != 0 {
                return 2;
            }
            let got = rt::sys::thread_get_sched(0);
            if got.status != 0 || got.a1 & 0xff != 12 || (got.a1 >> 16) & 0xff != 12 {
                return 3;
            }
            return 0;
        }
        // 67: raise the own priority to the job's limit, say so, wait to be told, report the priority then (the
        // parent lowers the job's maximum in between: PR1, every thread of the job is clamped at once).
        67 => {
            if rt::sys::thread_set_sched(0, 15, 0).status != 0 {
                return 1;
            }
            rt::sys::channel_write(rt::BOOTSTRAP, b"r".as_ptr(), 1, core::ptr::null(), 0);
            rt::sys::object_wait(rt::BOOTSTRAP, rt::abi::signals::READABLE, u64::MAX);
            let mut b = [0u8; 1];
            rt::sys::channel_read(rt::BOOTSTRAP, b.as_mut_ptr(), 1, core::ptr::null_mut(), 0);
            return (rt::sys::thread_get_sched(0).a1 & 0xff) as i32;
        }
        // 68: spawn three threads that sleep for ever, then (mode in the high bits) exit, fault or wait to be killed:
        // everything the process held, spawned stacks included, must come back (review 2026-11-15, finding 1).
        n if n & 0xff == 68 => {
            extern "C" fn sleeper(_a: u64, _tid: u64) -> ! {
                static W: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
                loop {
                    rt::sys::futex_wait(W.as_ptr(), 0, u64::MAX);
                }
            }
            let me = rt::sys::process_self().a1 as u64;
            for _ in 0..3 {
                let rec = rt::SpawnArgs {
                    entry: sleeper as *const () as usize as u64,
                    flags: rt::abi::spawn::DETACHED,
                    ..Default::default()
                };
                if rt::sys::thread_spawn(me, &rec).status != 0 {
                    return 1;
                }
            }
            match n >> 8 {
                0 => return 0,
                1 => {
                    // SAFETY: a deliberate fault.
                    unsafe { core::ptr::write_volatile(8 as *mut u32, 1) };
                    return 2;
                }
                _ => {
                    rt::sys::channel_write(rt::BOOTSTRAP, b"r".as_ptr(), 1, core::ptr::null(), 0);
                    loop {
                        core::hint::spin_loop();
                    }
                }
            }
        }
        71 => {
            // SIG2: count in a loop and report the count on the bootstrap channel now and then (a full channel drops the
            // report): a parent sees whether the process runs by whether reports arrive.
            let mut n: u64 = 0;
            loop {
                n = n.wrapping_add(1);
                if n & 0x1fff == 0 {
                    rt::sys::channel_write(
                        rt::BOOTSTRAP,
                        n.to_le_bytes().as_ptr(),
                        8,
                        core::ptr::null(),
                        0,
                    );
                }
                core::hint::spin_loop();
            }
        }
        72 => {
            // SIG2: stop the whole process (this thread included), and say so once resumed, then exit 4
            // (its own `process_self` handle has no KILL, so the parent sends it a handle to the process that has)
            rt::sys::object_wait(rt::BOOTSTRAP, rt::abi::signals::READABLE, u64::MAX);
            let (mut buf, mut got) = ([0u8; 4], [0u64; 1]);
            let r = rt::sys::channel_read(rt::BOOTSTRAP, buf.as_mut_ptr(), 4, got.as_mut_ptr(), 1);
            if r.status != 0 || r.a2 != 1 || rt::sys::process_suspend(got[0]).status != 0 {
                return 1;
            }
            rt::sys::channel_write(rt::BOOTSTRAP, b"r".as_ptr(), 1, core::ptr::null(), 0);
            return 4;
        }
        52 => {
            // S7b: sleep on a futex forever (until killed): nothing may be left behind
            static WORD: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
            // tell the parent we are about to sleep (it then waits a moment and kills us)
            rt::sys::channel_write(rt::BOOTSTRAP, b"s".as_ptr(), 1, core::ptr::null(), 0);
            rt::sys::futex_wait(WORD.as_ptr(), 0, u64::MAX);
            return 3; // not reached unless woken, which nobody does
        }
        41 => {
            // S5d: a process with two spinning sibling threads (and itself spinning): ended from
            // outside while they run on other harts.
            let me = rt::sys::process_self().a1 as u64;
            // (indexed on purpose: an iterator would make a reference to the `static mut`, and only its address may be taken)
            #[allow(clippy::needless_range_loop)]
            for k in 0..2 {
                // SAFETY: only the address of the static stack is taken; each new thread owns its own.
                let top = unsafe { core::ptr::addr_of_mut!(WORKER_STACKS.0[k]) } as u64 + 8192;
                rt::sys::thread_on_stack(me, spin_forever as *const () as usize as u64, top, 0);
            }
            loop {
                core::hint::spin_loop();
            }
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

/// IOMMU12, the hostile-driver matrix: a driver process that holds exactly the handles it is sent (a device claim's, in the driver's
/// job) and makes the calls the suite asks for with them, nothing of its own. The first message carries the handles; the reply is
/// their numbers here (8 bytes each). Then each request is eight words: an operation (0 a system call by number, 1 the library's
/// `sys_vmo_phys_addr`, 2 its two-argument `sys_vmo_create_dma`, 3 map one of its handles into itself anywhere (handle, offset,
/// bytes, permissions; `a1` the address), 4 a store to its own memory or device mapping (address, width 1, 2, 4 or 8, value), 5 a load
/// (address, width; `a1` the value), anything else exit), the number and six arguments; the reply is three words, the status
/// (sign-extended), `a1` and `a2`. A closed channel ends it too. With 3 to 5 it drives its device itself: it writes the descriptors
/// in its rings and rings the doorbell in its own mapping of the register BAR it was given.
fn hostile_driver() -> i32 {
    let rd = |m: &mut [u8], h: &mut [u64]| {
        if rt::sys::object_wait(
            rt::BOOTSTRAP,
            rt::abi::signals::READABLE | rt::abi::signals::PEER_CLOSED,
            u64::MAX,
        )
        .status
            != 0
        {
            return None;
        }
        let r = rt::sys::channel_read(
            rt::BOOTSTRAP,
            m.as_mut_ptr(),
            m.len(),
            h.as_mut_ptr(),
            h.len(),
        );
        (r.status == 0).then_some((r.a1, r.a2))
    };
    let mut m = [0u8; 64];
    let mut hs = [0u64; 16];
    let Some((_, n)) = rd(&mut m, &mut hs) else {
        return 100;
    };
    let mut out = [0u8; 128];
    for (i, h) in hs[..n].iter().enumerate() {
        out[8 * i..8 * i + 8].copy_from_slice(&h.to_le_bytes());
    }
    rt::sys::channel_write(rt::BOOTSTRAP, out.as_ptr(), 8 * n, core::ptr::null(), 0);
    loop {
        let Some((len, _)) = rd(&mut m, &mut []) else {
            return 0;
        };
        if len != 64 {
            return 101;
        }
        let w = |i: usize| u64::from_le_bytes(m[8 * i..8 * i + 8].try_into().unwrap_or([0; 8]));
        let args = [w(2), w(3), w(4), w(5), w(6), w(7)].map(|a| a as usize);
        let r = match w(0) {
            0 => rt::syscall(w(1) as usize, args),
            1 => rt::sys::vmo_phys_addr(w(2)),
            2 => rt::sys::vmo_create_dma(w(2), w(3)),
            3 => {
                let me = rt::sys::process_self();
                if me.status != 0 {
                    me
                } else {
                    let r = rt::sys::process_map(
                        me.a1 as u64,
                        w(2),
                        0,
                        w(3),
                        w(4),
                        w(5) | rt::abi::map_flags::ANYWHERE,
                    );
                    rt::sys::handle_close(me.a1 as u64);
                    r
                }
            }
            4 => {
                let a = w(2) as usize;
                // SAFETY: none in the usual sense: this process is a deliberately hostile driver that stores wherever the suite tells it
                // to (its own mappings, by the protocol); a wrong address faults the process, which the suite sees as a dead driver.
                unsafe {
                    match w(3) {
                        1 => core::ptr::write_volatile(a as *mut u8, w(4) as u8),
                        2 => core::ptr::write_volatile(a as *mut u16, w(4) as u16),
                        4 => core::ptr::write_volatile(a as *mut u32, w(4) as u32),
                        _ => core::ptr::write_volatile(a as *mut u64, w(4)),
                    }
                }
                rt::Ret {
                    status: 0,
                    a1: 0,
                    a2: 0,
                }
            }
            5 => {
                let a = w(2) as usize;
                // SAFETY: as for 4, a load.
                let v = unsafe {
                    match w(3) {
                        1 => u64::from(core::ptr::read_volatile(a as *const u8)),
                        2 => u64::from(core::ptr::read_volatile(a as *const u16)),
                        4 => u64::from(core::ptr::read_volatile(a as *const u32)),
                        _ => core::ptr::read_volatile(a as *const u64),
                    }
                };
                rt::Ret {
                    status: 0,
                    a1: v as usize,
                    a2: 0,
                }
            }
            _ => return 0,
        };
        let mut rep = [0u8; 24];
        rep[..8].copy_from_slice(&(i64::from(r.status) as u64).to_le_bytes());
        rep[8..16].copy_from_slice(&(r.a1 as u64).to_le_bytes());
        rep[16..].copy_from_slice(&(r.a2 as u64).to_le_bytes());
        rt::sys::channel_write(rt::BOOTSTRAP, rep.as_ptr(), 24, core::ptr::null(), 0);
    }
}

#[repr(align(16))]
struct WorkerStacks([[u8; 8192]; 2]);
static mut WORKER_STACKS: WorkerStacks = WorkerStacks([[0; 8192]; 2]);

#[repr(align(16))]
#[allow(dead_code)]
struct Stack([u8; 8192]);
static mut STACK: Stack = Stack([0; 8192]);

/// Start a second thread running `f(arg)` on a static stack; returns its thread handle.
fn spawn(f: extern "C" fn(usize) -> !, arg: usize) -> u64 {
    let me = rt::sys::process_self().a1 as u64;
    // Only the address of the static is taken; the one new thread uses it as its stack.
    let top = core::ptr::addr_of_mut!(STACK) as u64 + 8192;
    let r = rt::sys::thread_on_stack(me, f as *const () as usize as u64, top, arg as u64);
    if r.status != 0 {
        rt::sys::exit(100 + (-(r.status as i64)) as i32);
    }
    r.a1 as u64
}

extern "C" fn spin_forever(_: usize) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

extern "C" fn exit_soon(_: usize) -> ! {
    let t = rt::sys::clock_get() + 3;
    // an event nobody signals: a timed wait
    let ev = rt::sys::event_create().a1 as u64;
    rt::sys::object_wait(ev, 1 << 24, t);
    rt::sys::exit(9)
}

extern "C" fn fault(_: usize) -> ! {
    // SAFETY: deliberately invalid; the kernel must end the whole process.
    unsafe { core::ptr::read_volatile(core::ptr::null::<u64>()) };
    loop {
        core::hint::spin_loop();
    }
}

extern "C" fn thread_exit_four(_: usize) -> ! {
    rt::sys::thread_exit(4)
}

extern "C" fn exit_eight(_: usize) -> ! {
    rt::sys::thread_exit(8)
}

extern "C" fn exit_eight_late(_: usize) -> ! {
    let gate = rt::sys::event_create().a1 as u64;
    rt::sys::object_wait(gate, 1 << 24, rt::sys::clock_get() + 20);
    rt::sys::thread_exit(8)
}

// ---- S5d: remote TLB shootdown ----

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Where the changing mapping lives: above the stack arena, where nothing the kernel places lands (in the stack arena a thread stack may be, T272).
const X: u64 = rt::abi::user::STACK_ARENA_END;
const ROUNDS: u64 = 300;
const READ: u64 = 1;
const IDLE: u64 = 2;

/// (round << 2) | state: what the workers should be doing.
static PHASE: AtomicU64 = AtomicU64::new(0);
static READ_ACKS: AtomicU64 = AtomicU64::new(0);
static IDLE_ACKS: AtomicU64 = AtomicU64::new(0);
/// The first round in which a worker read a value that was not the current mapping's.
static FAIL: AtomicU64 = AtomicU64::new(0);

fn pattern(round: u64) -> u64 {
    0x5eed_0000_0000_0000 | (round * 0x0101_0101)
}

/// How many turns a waiting thread of these tests spins before it yields its hart.
const SPINS_PER_YIELD: u64 = 4096;

/// One turn of a wait in the sibling tests (`spins` counts the turns so far): mostly a spin, so that a thread on a hart of its own keeps
/// running there with its translations cached (the point of the tests), and every [`SPINS_PER_YIELD`] turns a yield. With fewer harts
/// than threads (one hart: the kernel-only boots and KT5's instruction counts) the thread being waited for gets the hart at once instead
/// of at the end of the waiter's time slice: a round there took a dozen time slices, and the 300 rounds of each test most of a minute.
///
/// The tests' power at several harts rests on this: a yield with nothing else queued on the hart (`yield_fast`) leaves the caller where it
/// is, with no `satp` write, no `sfence` and no migration, so its cached translations survive it. A change to what a yield does must rerun
/// the three negative controls of `os-boot-test-smp-neg` at four harts (`fault-skip-shootdown`, `fault-decommit-no-shootdown`,
/// `fault-protect-no-shootdown`): they are in the soak tier, so no gate would notice the tests going blind. (What does not depend on the
/// emulator is on the host: `ksys/tests/shootdown_model.rs` checks in the feature gate that every unmap, protect and decommit asks for the
/// remote flush, against harts that keep every translation until flushed, T193; these tests check that the machine then honours it.)
fn relax(spins: u64) {
    if spins.is_multiple_of(SPINS_PER_YIELD) {
        rt::sys::thread_yield();
    } else {
        core::hint::spin_loop();
    }
}

/// Two sibling threads read a page that the main thread unmaps and remaps, round after round,
/// with the harts they run on caching the translation in between. If an unmap did not reach the
/// other harts' TLBs, a worker would read the *previous* round's data through its stale entry
/// (a different frame from the new mapping). The exit code is 0, or the first failing round.
fn unmap_while_siblings_touch() -> i32 {
    let me = rt::sys::process_self().a1 as u64;
    // (indexed on purpose: an iterator would make a reference to the `static mut`, and only its address may be taken)
    #[allow(clippy::needless_range_loop)]
    for k in 0..2 {
        // SAFETY: only the address of the static stack is taken; each worker owns its own.
        let top = unsafe { core::ptr::addr_of_mut!(WORKER_STACKS.0[k]) } as u64 + 8192;
        let t = rt::sys::thread_on_stack(me, reader as *const () as usize as u64, top, k as u64);
        if t.status != 0 {
            return 900 + (-(t.status as i64)) as i32;
        }
    }
    // (the previous round's VMOs stay open until this round's is mapped: a stale translation of the old page then shows the old bytes, whatever order the
    // allocator hands frames back in; closing them first let a freed frame come straight back, and a missing shootdown went unseen: soak, 2026-11-23)
    let mut previous: [u64; 2] = [0, 0];
    for round in 1..=ROUNDS {
        let v = rt::sys::vmo_create(1);
        if v.status != 0 {
            return 910;
        }
        let vmo = v.a1 as u64;
        let want = pattern(round).to_le_bytes();
        if rt::sys::vmo_write(vmo, 0, want.as_ptr(), 8).status != 0 {
            return 911;
        }
        // map it read-only at X (PF_R = 1)
        if rt::sys::process_map(me, vmo, X, 0, 4096, 1).status != 0 {
            return 912;
        }
        for h in previous {
            if h != 0 {
                rt::sys::handle_close(h);
            }
        }
        // a second VMO with other contents, to give a freed frame something to be reused for
        let junk = rt::sys::vmo_create(1).a1 as u64;
        let _ = rt::sys::vmo_write(junk, 0, [0xee; 8].as_ptr(), 8);
        PHASE.store((round << 2) | READ, Ordering::Release);
        let mut spins = 0u64;
        while READ_ACKS.load(Ordering::Acquire) < 2 * round {
            spins += 1;
            if spins > 400_000_000 {
                return 920;
            }
            relax(spins);
        }
        PHASE.store((round << 2) | IDLE, Ordering::Release);
        spins = 0;
        while IDLE_ACKS.load(Ordering::Acquire) < 2 * round {
            spins += 1;
            if spins > 400_000_000 {
                return 921;
            }
            relax(spins);
        }
        // nobody is reading X now; take the mapping away and free the frame
        if rt::sys::process_unmap(me, X).status != 0 {
            return 913;
        }
        previous = [vmo, junk];
        if FAIL.load(Ordering::Acquire) != 0 {
            return FAIL.load(Ordering::Acquire) as i32;
        }
    }
    for h in previous {
        if h != 0 {
            rt::sys::handle_close(h);
        }
    }
    FAIL.load(Ordering::Acquire) as i32
}

extern "C" fn reader(_id: usize) -> ! {
    let mut read_round = 0u64;
    let mut idle_round = 0u64;
    let mut spins = 0u64;
    loop {
        let ph = PHASE.load(Ordering::Acquire);
        let (round, state) = (ph >> 2, ph & 3);
        if state == READ && round > read_round {
            let want = pattern(round);
            for _ in 0..40 {
                // SAFETY: X is mapped (read-only) by the main thread for this whole phase.
                let got = unsafe { core::ptr::read_volatile(X as *const u64) };
                if got != want {
                    let _ = FAIL.compare_exchange(0, round, Ordering::AcqRel, Ordering::Acquire);
                }
            }
            read_round = round;
            READ_ACKS.fetch_add(1, Ordering::AcqRel);
        } else if state == IDLE && round > idle_round {
            idle_round = round;
            IDLE_ACKS.fetch_add(1, Ordering::AcqRel);
        }
        spins = spins.wrapping_add(1);
        relax(spins);
    }
}

// ---- MM6/MM7: decommit and protect against sibling threads ----

const D_READ: u64 = 1;
const D_ZERO: u64 = 3;
static D_PHASE: AtomicU64 = AtomicU64::new(0);
static D_READ_ACKS: AtomicU64 = AtomicU64::new(0);
static D_ZERO_ACKS: AtomicU64 = AtomicU64::new(0);
static D_FAIL: AtomicU64 = AtomicU64::new(0);

/// A worker: for each round, reads the page the main thread maps and expects the round's pattern; then, after the main thread has
/// decommitted it and given the frame to another VMO, expects zeros. A stale translation shows the frame's new owner's bytes.
extern "C" fn decommit_reader(_id: usize) -> ! {
    let (mut read_round, mut zero_round) = (0u64, 0u64);
    let mut spins = 0u64;
    loop {
        let ph = D_PHASE.load(Ordering::Acquire);
        let (round, state) = (ph >> 2, ph & 3);
        if state == D_READ && round > read_round {
            for _ in 0..40 {
                // SAFETY: X is mapped (read-only) by the main thread for the round.
                let got = unsafe { core::ptr::read_volatile(X as *const u64) };
                if got != pattern(round) {
                    let _ = D_FAIL.compare_exchange(0, round, Ordering::AcqRel, Ordering::Acquire);
                }
            }
            read_round = round;
            D_READ_ACKS.fetch_add(1, Ordering::AcqRel);
        } else if state == D_ZERO && round > zero_round {
            for _ in 0..40 {
                // SAFETY: as above (a decommitted page of a mapping faults in again as zeros).
                let got = unsafe { core::ptr::read_volatile(X as *const u64) };
                if got != 0 {
                    let _ = D_FAIL.compare_exchange(0, round, Ordering::AcqRel, Ordering::Acquire);
                }
            }
            zero_round = round;
            D_ZERO_ACKS.fetch_add(1, Ordering::AcqRel);
        }
        spins = spins.wrapping_add(1);
        relax(spins);
    }
}

fn spin_until(acks: &AtomicU64, want: u64) -> bool {
    let mut spins = 0u64;
    while acks.load(Ordering::Acquire) < want {
        spins += 1;
        if spins > 400_000_000 {
            return false;
        }
        relax(spins);
    }
    true
}

/// Two sibling threads read a page that the main thread decommits and whose frame it hands to another VMO, round after round. Exit code
/// 0, or the first failing round.
fn decommit_while_siblings_touch() -> i32 {
    let me = rt::sys::process_self().a1 as u64;
    #[allow(clippy::needless_range_loop)]
    for k in 0..2 {
        // SAFETY: only the address of the static stack is taken; each worker owns its own.
        let top = unsafe { core::ptr::addr_of_mut!(WORKER_STACKS.0[k]) } as u64 + 8192;
        let t = rt::sys::thread_on_stack(
            me,
            decommit_reader as *const () as usize as u64,
            top,
            k as u64,
        );
        if t.status != 0 {
            return 900 + (-(t.status as i64)) as i32;
        }
    }
    for round in 1..=ROUNDS {
        let v = rt::sys::vmo_create(1);
        if v.status != 0 {
            return 910;
        }
        let vmo = v.a1 as u64;
        if rt::sys::vmo_write(vmo, 0, pattern(round).to_le_bytes().as_ptr(), 8).status != 0 {
            return 911;
        }
        if rt::sys::process_map(me, vmo, X, 0, 4096, 1).status != 0 {
            return 912;
        }
        D_PHASE.store((round << 2) | D_READ, Ordering::Release);
        if !spin_until(&D_READ_ACKS, 2 * round) {
            return 920;
        }
        // the frame goes back, and another VMO is handed it (the freed frame is the next one this hart's cache gives)
        if rt::sys::vmo_decommit(vmo, 0, 4096).status != 0 {
            return 913;
        }
        let junk = rt::sys::vmo_create(1).a1 as u64;
        let _ = rt::sys::vmo_write(junk, 0, [0xee; 8].as_ptr(), 8);
        D_PHASE.store((round << 2) | D_ZERO, Ordering::Release);
        if !spin_until(&D_ZERO_ACKS, 2 * round) {
            return 921;
        }
        if rt::sys::process_unmap(me, X).status != 0 {
            return 914;
        }
        rt::sys::handle_close(vmo);
        rt::sys::handle_close(junk);
        if D_FAIL.load(Ordering::Acquire) != 0 {
            return D_FAIL.load(Ordering::Acquire) as i32;
        }
    }
    D_FAIL.load(Ordering::Acquire) as i32
}

const P_WRITE1: u64 = 1;
const P_WRITE2: u64 = 2;
static P_PHASE: AtomicU64 = AtomicU64::new(0);
static P_ACKS1: AtomicU64 = AtomicU64::new(0);
static P_ACKS2: AtomicU64 = AtomicU64::new(0);

/// A worker: writes its own word of the page when told (so its hart caches a writable translation), and again after the main thread
/// has taken write permission away. Once the protect has reached its hart the second write faults and ends the process.
extern "C" fn protect_writer(id: usize) -> ! {
    let mut done1 = false;
    let mut done2 = false;
    loop {
        let ph = P_PHASE.load(Ordering::Acquire);
        let word = (X + 8 * id as u64) as *mut u64;
        if ph == P_WRITE1 && !done1 {
            // SAFETY: X is mapped read-write by the main thread for this phase; each worker has its own word.
            unsafe { core::ptr::write_volatile(word, 0x11 + id as u64) };
            done1 = true;
            P_ACKS1.fetch_add(1, Ordering::AcqRel);
        } else if ph == P_WRITE2 && !done2 {
            // SAFETY: the page is read-only now: with the translation dropped this write faults, which is the test.
            unsafe { core::ptr::write_volatile(word, 0x22 + id as u64) };
            done2 = true;
            P_ACKS2.fetch_add(1, Ordering::AcqRel);
        }
        // (a bare spin on purpose, not `relax`: a writer switched out between its two writes would lose the writable translation the test
        // needs; at one hart each of the twenty processes costs a time slice or two, which is cheap)
        core::hint::spin_loop();
    }
}

/// Exit code: -1001 from the kernel when a worker's write faulted (as it must), 0 if the writes went through, 9xx if the protocol broke.
fn protect_while_siblings_write() -> i32 {
    let me = rt::sys::process_self().a1 as u64;
    #[allow(clippy::needless_range_loop)]
    for k in 0..2 {
        // SAFETY: only the address of the static stack is taken; each worker owns its own.
        let top = unsafe { core::ptr::addr_of_mut!(WORKER_STACKS.0[k]) } as u64 + 8192;
        let t = rt::sys::thread_on_stack(
            me,
            protect_writer as *const () as usize as u64,
            top,
            k as u64,
        );
        if t.status != 0 {
            return 900 + (-(t.status as i64)) as i32;
        }
    }
    let vmo = rt::sys::vmo_create(1).a1 as u64;
    if rt::sys::process_map(me, vmo, X, 0, 4096, 3).status != 0 {
        return 912;
    }
    P_PHASE.store(P_WRITE1, Ordering::Release);
    if !spin_until(&P_ACKS1, 2) {
        return 920;
    }
    if rt::sys::process_protect(me, X, 4096, 1).status != 0 {
        return 913;
    }
    P_PHASE.store(P_WRITE2, Ordering::Release);
    // (with the shootdown done, a worker faults here and the kernel ends the process)
    if !spin_until(&P_ACKS2, 2) {
        return 921;
    }
    0
}

// ---- S5e: churn ----

#[repr(align(16))]
struct ChurnStacks([[u8; 4096]; 4]);
static mut CHURN_STACKS: ChurnStacks = ChurnStacks([[0; 4096]; 4]);
static CHURN_BUSY: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
static CHURN_DONE: AtomicU64 = AtomicU64::new(0);

extern "C" fn churn_worker(slot: usize) -> ! {
    CHURN_DONE.fetch_add(1, Ordering::AcqRel);
    CHURN_BUSY[slot].store(0, Ordering::Release);
    rt::sys::thread_exit(0)
}

/// Random object, memory and thread churn from one process (several of them run at once on
/// different harts); every call must behave. Exit code 0, or 100 + the operation that failed.
fn churn(seed: u64) -> i32 {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut rnd = move |n: u64| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % n
    };
    let me = rt::sys::process_self().a1 as u64;
    let buf = [0x42u8; 4096];
    let mut out = [0u8; 4096];
    for _ in 0..1500u32 {
        match rnd(7) {
            0 => {
                let c = rt::sys::channel_create();
                if c.status != 0 {
                    return 100;
                }
                let (a, b) = (c.a1 as u64, c.a2 as u64);
                if rt::sys::channel_write(a, buf.as_ptr(), 16, core::ptr::null(), 0).status != 0 {
                    return 101;
                }
                if rt::sys::channel_read(b, out.as_mut_ptr(), 4096, core::ptr::null_mut(), 0).status
                    != 0
                {
                    return 102;
                }
                rt::sys::handle_close(a);
                rt::sys::handle_close(b);
            }
            1 => {
                let v = rt::sys::vmo_create(4);
                if v.status != 0 {
                    return 110;
                }
                let vmo = v.a1 as u64;
                if rt::sys::vmo_write(vmo, 0, buf.as_ptr(), 4096).status != 0 {
                    return 111;
                }
                let at = rt::abi::user::STACK_ARENA_END + 0x1_0000_0000 + rnd(8) * 0x2_0000;
                let m = rt::sys::process_map(me, vmo, at, 0, 4 * 4096, 3);
                if m.status != 0 {
                    // (the status is in the code: a failure seen once in CI, T251)
                    return 1100 + (-(m.status as i64)) as i32;
                }
                // SAFETY: the page at `at` was just mapped read-write.
                unsafe { core::ptr::write_volatile(at as *mut u64, 7) };
                if rt::sys::process_unmap(me, at).status != 0 {
                    return 113;
                }
                rt::sys::handle_close(vmo);
            }
            2 => {
                let p = rt::sys::socket_create();
                if p.status != 0 {
                    return 120;
                }
                let (a, b) = (p.a1 as u64, p.a2 as u64);
                if rt::sys::socket_write(a, buf.as_ptr(), 100).status != 0 {
                    return 121;
                }
                if rt::sys::socket_read(b, out.as_mut_ptr(), 4096).status != 0 {
                    return 122;
                }
                rt::sys::handle_close(a);
                rt::sys::handle_close(b);
            }
            3 => {
                let e = rt::sys::event_create();
                if e.status != 0 {
                    return 130;
                }
                if rt::sys::event_signal(e.a1 as u64, 0, rt::abi::signals::USER_MASK & 0x0100_0000)
                    .status
                    != 0
                {
                    return 131;
                }
                rt::sys::handle_close(e.a1 as u64);
            }
            4 => {
                let port = rt::sys::port_create();
                let ev = rt::sys::event_create();
                if port.status != 0 || ev.status != 0 {
                    return 140;
                }
                let (p, e) = (port.a1 as u64, ev.a1 as u64);
                rt::sys::event_signal(e, 0, rt::abi::signals::USER_MASK & 0x0100_0000);
                if rt::sys::port_bind(p, e, rt::abi::signals::USER_MASK, 9).status != 0 {
                    return 141;
                }
                let w = rt::sys::port_wait(p, 0);
                if w.status != 0 {
                    return 142;
                }
                rt::sys::handle_close(p);
                rt::sys::handle_close(e);
            }
            5 => {
                // a short-lived sibling thread
                let slot = rnd(4) as usize;
                if CHURN_BUSY[slot]
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    // SAFETY: the slot's stack is free (the flag was clear).
                    let top =
                        unsafe { core::ptr::addr_of_mut!(CHURN_STACKS.0[slot]) } as u64 + 4096;
                    let t = rt::sys::thread_on_stack(
                        me,
                        churn_worker as *const () as usize as u64,
                        top,
                        slot as u64,
                    );
                    if t.status != 0 {
                        CHURN_BUSY[slot].store(0, Ordering::Release);
                        return 150;
                    }
                    rt::sys::handle_close(t.a1 as u64);
                }
            }
            _ => {
                rt::sys::thread_yield();
            }
        }
    }
    // let the workers finish before the process ends
    for _ in 0..1_000_000u32 {
        if CHURN_BUSY.iter().all(|b| b.load(Ordering::Acquire) == 0) {
            return 0;
        }
        rt::sys::thread_yield();
    }
    160
}

// --- floating point (slice A) ----------------------------------------------------------------------
// This program is built for the no-FPU user target, so the FP instructions are assembled with the
// `d` extension switched on for just these blocks.

fn fp_pattern(seed: u64) -> [u64; 32] {
    let mut p = [0u64; 32];
    for (i, v) in p.iter_mut().enumerate() {
        *v = (seed.wrapping_add(i as u64 + 1)).wrapping_mul(0x9e37_79b9_7f4a_7c15)
            ^ ((i as u64) << 52);
    }
    p
}

/// Load `f0`-`f31` from `p` and set `fcsr`.
fn fp_load(p: &[u64; 32], fcsr: u32) {
    // SAFETY: only reads `p` and writes FP registers and `fcsr`; the kernel enables the unit on the
    // first instruction.
    unsafe {
        asm!(
            ".option push", ".option arch, +d",
            "fld f0, 0({p})", "fld f1, 8({p})", "fld f2, 16({p})", "fld f3, 24({p})",
            "fld f4, 32({p})", "fld f5, 40({p})", "fld f6, 48({p})", "fld f7, 56({p})",
            "fld f8, 64({p})", "fld f9, 72({p})", "fld f10, 80({p})", "fld f11, 88({p})",
            "fld f12, 96({p})", "fld f13, 104({p})", "fld f14, 112({p})", "fld f15, 120({p})",
            "fld f16, 128({p})", "fld f17, 136({p})", "fld f18, 144({p})", "fld f19, 152({p})",
            "fld f20, 160({p})", "fld f21, 168({p})", "fld f22, 176({p})", "fld f23, 184({p})",
            "fld f24, 192({p})", "fld f25, 200({p})", "fld f26, 208({p})", "fld f27, 216({p})",
            "fld f28, 224({p})", "fld f29, 232({p})", "fld f30, 240({p})", "fld f31, 248({p})",
            "fscsr {c}",
            ".option pop",
            p = in(reg) p.as_ptr(),
            c = in(reg) fcsr,
        );
    }
}

/// Read `f0`-`f31` into `out`; returns `fcsr`.
fn fp_store(out: &mut [u64; 32]) -> u32 {
    let c: u64;
    // SAFETY: only writes `out` and reads FP registers and `fcsr`.
    unsafe {
        asm!(
            ".option push", ".option arch, +d",
            "fsd f0, 0({p})", "fsd f1, 8({p})", "fsd f2, 16({p})", "fsd f3, 24({p})",
            "fsd f4, 32({p})", "fsd f5, 40({p})", "fsd f6, 48({p})", "fsd f7, 56({p})",
            "fsd f8, 64({p})", "fsd f9, 72({p})", "fsd f10, 80({p})", "fsd f11, 88({p})",
            "fsd f12, 96({p})", "fsd f13, 104({p})", "fsd f14, 112({p})", "fsd f15, 120({p})",
            "fsd f16, 128({p})", "fsd f17, 136({p})", "fsd f18, 144({p})", "fsd f19, 152({p})",
            "fsd f20, 160({p})", "fsd f21, 168({p})", "fsd f22, 176({p})", "fsd f23, 184({p})",
            "fsd f24, 192({p})", "fsd f25, 200({p})", "fsd f26, 208({p})", "fsd f27, 216({p})",
            "fsd f28, 224({p})", "fsd f29, 232({p})", "fsd f30, 240({p})", "fsd f31, 248({p})",
            "frcsr {c}",
            ".option pop",
            p = in(reg) out.as_mut_ptr(),
            c = out(reg) c,
        );
    }
    c as u32
}

/// Keep a private pattern in the FP registers for a while. Mode 0 mostly spins in user mode (timer
/// interrupts land on user instructions); mode 1 yields every round (the other way to lose the CPU).
/// Returns 0 if every register and `fcsr` always held this program's own values, else 1.
fn fp_keep(seed: u64, mode: u64) -> i32 {
    let mut want = fp_pattern(seed);
    let mut fcsr = (seed & 7) as u32; // a different rounding mode per program (the low bits stay clear)
    fp_load(&want, fcsr << 5);
    let until = rt::sys::clock_get() + 30;
    let mut round = 0u64;
    let mut got = [0u64; 32];
    while rt::sys::clock_get() < until {
        for _ in 0..2000 {
            core::hint::spin_loop();
        }
        if mode == 1 {
            rt::sys::thread_yield();
        }
        let c = fp_store(&mut got);
        if got != want || c != fcsr << 5 {
            return 1;
        }
        round += 1;
        if round.is_multiple_of(5) {
            // change the state (dirty the unit again) and keep going
            want = fp_pattern(seed.wrapping_add(round));
            fcsr = ((seed + round) & 7) as u32;
            fp_load(&want, fcsr << 5);
        }
    }
    0
}

/// First FP use: every register must read as zero.
fn fp_fresh() -> i32 {
    let mut got = [0u64; 32];
    let c = fp_store(&mut got);
    let nonzero = got.iter().filter(|&&v| v != 0).count() as i32;
    nonzero + if c != 0 { 100 } else { 0 }
}

static FP_THREAD_RESULT: [AtomicU32; 2] = [AtomicU32::new(99), AtomicU32::new(99)];

extern "C" fn fp_worker(k: usize) -> ! {
    let r = fp_keep(1000 + k as u64 * 77, 1) as u32;
    FP_THREAD_RESULT[k].store(r, Ordering::Release);
    rt::sys::thread_exit(0)
}

fn fp_threads() -> i32 {
    let me = rt::sys::process_self().a1 as u64;
    // (indexed on purpose: an iterator would make a reference to the `static mut`, and only its address may be taken)
    #[allow(clippy::needless_range_loop)]
    for k in 0..2usize {
        // SAFETY: only the address of the static stack is taken; each worker owns its own.
        let top = unsafe { core::ptr::addr_of_mut!(WORKER_STACKS.0[k]) } as u64 + 8192;
        let t = rt::sys::thread_on_stack(me, fp_worker as *const () as usize as u64, top, k as u64);
        if t.status != 0 {
            return 900;
        }
    }
    let mine = fp_keep(5, 0);
    for _ in 0..1_000_000u32 {
        if FP_THREAD_RESULT
            .iter()
            .all(|r| r.load(Ordering::Acquire) != 99)
        {
            let worst = FP_THREAD_RESULT
                .iter()
                .map(|r| r.load(Ordering::Acquire) as i32)
                .max()
                .unwrap_or(0);
            return mine.max(worst);
        }
        rt::sys::thread_yield();
    }
    901
}

// PL2: vector state. The user programs are built for rv64gc, so the vector instructions are assembled with the extension on for these blocks;
// the kernel enables the unit at the first one (an illegal instruction until then, `csrr vlenb` included).

/// The longest vector register this test handles (bytes), and the buffers: a pattern and a readback for each of three harts of threads.
const VMAX: usize = 256;
#[repr(align(16))]
struct VecBufs([[u8; 32 * VMAX]; 6]);
static mut VBUFS: VecBufs = VecBufs([[0; 32 * VMAX]; 6]);

fn vlenb() -> usize {
    let v: usize;
    // SAFETY: reads a vector CSR (the kernel enables the unit at the first use).
    unsafe { asm!("csrr {v}, 0xc22", v = out(reg) v) };
    v
}

fn vec_pattern(buf: &mut [u8], seed: u64) {
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (seed
            .wrapping_add(i as u64 + 1)
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            >> 29) as u8;
    }
}

/// One round of the privacy test, in ONE assembly block (the user build is RVA23U64, so compiled code uses vector registers too and would
/// overwrite a pattern held across it): set `vl`/`vtype`, load `v0`-`v31` from `want`, set `vcsr`, spin without touching a vector register,
/// optionally yield (a system call: the thread is switched out and back, possibly on another hart), store the registers to `got`, and read
/// `vcsr`, `vl` and `vtype` back. Returns (`vl` as set, `vcsr`, `vl`, `vtype` as read back).
#[allow(clippy::too_many_arguments)] // one assembly block takes everything it needs: the buffers, the new state and the number of spins
fn vec_round(
    want: *const u8,
    got: *mut u8,
    vlenb: usize,
    avl: usize,
    vtype: u64,
    csr: u64,
    spins: u64,
    yield_after: bool,
) -> (u64, u64, u64, u64) {
    let (vl_set, c, vl, vt): (u64, u64, u64, u64);
    // SAFETY: reads and writes `32 * vlenb` bytes at `want` and `got` (the caller's buffers); every vector register is declared clobbered; the
    // system call preserves all registers but a0-a2 (declared clobbered).
    unsafe {
        asm!(
            "vsetvl {vl_set}, {avl}, {vtype}",
            "mv {p}, {want}",
            "vl8re8.v v0, ({p})", "add {p}, {p}, {s}",
            "vl8re8.v v8, ({p})", "add {p}, {p}, {s}",
            "vl8re8.v v16, ({p})", "add {p}, {p}, {s}",
            "vl8re8.v v24, ({p})",
            "csrw vcsr, {csr}",
            "2:",
            "addi {spins}, {spins}, -1",
            "bnez {spins}, 2b",
            "beqz {y}, 3f",
            "jalr {lib}",
            "3:",
            "mv {p}, {got}",
            "vs8r.v v0, ({p})", "add {p}, {p}, {s}",
            "vs8r.v v8, ({p})", "add {p}, {p}, {s}",
            "vs8r.v v16, ({p})", "add {p}, {p}, {s}",
            "vs8r.v v24, ({p})",
            "csrr {c}, vcsr",
            "csrr {vl}, vl",
            "csrr {vt}, vtype",
            want = in(reg) want,
            got = in(reg) got,
            s = in(reg) vlenb * 8,
            avl = in(reg) avl,
            vtype = in(reg) vtype,
            csr = in(reg) csr,
            spins = inout(reg) spins => _,
            y = in(reg) yield_after as usize,
            p = out(reg) _,
            vl_set = out(reg) vl_set,
            c = out(reg) c,
            vl = out(reg) vl,
            vt = out(reg) vt,
            lib = in(reg) rt::library::entry(rt::abi::syscall::THREAD_YIELD),
            out("ra") _, out("a7") _,
            out("a0") _, out("a1") _, out("a2") _,
            out("v0") _, out("v1") _, out("v2") _, out("v3") _, out("v4") _, out("v5") _, out("v6") _, out("v7") _, out("v8") _, out("v9") _, out("v10") _, out("v11") _, out("v12") _, out("v13") _, out("v14") _, out("v15") _, out("v16") _, out("v17") _, out("v18") _, out("v19") _, out("v20") _, out("v21") _, out("v22") _, out("v23") _, out("v24") _, out("v25") _, out("v26") _, out("v27") _, out("v28") _, out("v29") _, out("v30") _, out("v31") _
        );
    }
    (vl_set, c, vl, vt)
}

/// Keep a private pattern in the vector registers, `vcsr`, `vl` and `vtype` for a while (`slot` picks the buffers: one per thread). Returns 0 if
/// everything always held this program's own values, else 1.
fn vec_keep(seed: u64, mode: u64, slot: usize) -> i32 {
    let n = vlenb();
    if n == 0 || n > VMAX || !n.is_power_of_two() {
        return 800;
    }
    // SAFETY: only the addresses of this thread's two buffers are taken.
    let (want, got) = unsafe {
        let b = core::ptr::addr_of_mut!(VBUFS.0);
        (
            core::slice::from_raw_parts_mut((b as *mut u8).add(2 * slot * 32 * VMAX), 32 * n),
            core::slice::from_raw_parts_mut((b as *mut u8).add((2 * slot + 1) * 32 * VMAX), 32 * n),
        )
    };
    let until = rt::sys::clock_get() + 30;
    let mut round = 0u64;
    while rt::sys::clock_get() < until {
        // a new pattern, element width, length and `vcsr` every round, so a stale restore cannot pass
        let cur = seed.wrapping_add(round);
        vec_pattern(want, cur);
        let (avl, vtype, csr) = (
            1 + (cur % 7) as usize,
            ((cur >> 3) & 3) << 3 | 0xc0,
            cur & 7,
        );
        let (vl_set, c, vl, vt) = vec_round(
            want.as_ptr(),
            got.as_mut_ptr(),
            n,
            avl,
            vtype,
            csr,
            2000,
            mode == 1,
        );
        if got[..] != want[..] || c != csr || vl != vl_set || vt != vtype {
            return 1;
        }
        round += 1;
    }
    0
}

/// First vector use: every register, `vcsr`, `vl` and `vtype` must read as zero. Run on a thread whose entry is assembly that executes a vector
/// instruction first: compiled code (the user build is RVA23U64) may already have used the vector unit in this process, and a thread of its
/// own starts with the unit off. The thread stores what it saw in `VFRESH` and signals through its last word.
fn vec_fresh() -> i32 {
    let me = rt::sys::process_self().a1 as u64;
    // SAFETY: only addresses of statics are taken; the thread writes `VFRESH` and its buffer, and this thread reads them after the done flag.
    let (res, buf) = unsafe {
        (
            core::ptr::addr_of_mut!(VFRESH) as *mut u64,
            (core::ptr::addr_of_mut!(VBUFS.0) as *mut u8).add(5 * 32 * VMAX),
        )
    };
    // SAFETY: as above.
    unsafe {
        res.write_volatile(buf as u64);
        for k in 1..6 {
            res.add(k).write_volatile(0);
        }
    }
    // SAFETY: written before the thread that reads it exists.
    unsafe { VFRESH_EXIT = rt::library::entry(rt::abi::syscall::THREAD_EXIT) };
    // SAFETY: only the address of the static stack is taken (the thread's entry uses no stack).
    let top = unsafe { core::ptr::addr_of_mut!(WORKER_STACKS.0[0]) } as u64 + 8192;
    let t = rt::sys::thread_on_stack(
        me,
        vec_fresh_thread as *const () as usize as u64,
        top,
        res as u64,
    );
    if t.status != 0 {
        return 900;
    }
    for _ in 0..1_000_000u32 {
        // SAFETY: reads the done flag the thread writes last.
        if unsafe { res.add(5).read_volatile() } == 1 {
            break;
        }
        rt::sys::thread_yield();
    }
    // SAFETY: as above.
    let r: [u64; 6] = unsafe { core::array::from_fn(|k| res.add(k).read_volatile()) };
    let n = r[1] as usize;
    if r[5] != 1 || n == 0 || n > VMAX {
        return 800;
    }
    // SAFETY: the buffer the thread stored into, `32 * n` bytes of it.
    let got = unsafe { core::slice::from_raw_parts(buf, 32 * n) };
    let nonzero = got.iter().filter(|&&b| b != 0).count() as i32;
    nonzero + if r[2] | r[3] | r[4] != 0 { 10_000 } else { 0 }
}

/// [buffer address, vlenb, vcsr, vl, vtype, done] as the fresh thread leaves them.
static mut VFRESH: [u64; 6] = [0; 6];

extern "C" {
    fn vec_fresh_thread(arg: usize) -> !;
}

core::arch::global_asm!(
    ".section .text.vec_fresh_thread,\"ax\",@progbits",
    ".globl vec_fresh_thread",
    "vec_fresh_thread:",
    "csrr t0, vlenb", // the first vector instruction this thread executes: the kernel enables the unit here, from zeros
    "ld t1, 0(a0)",
    "slli t2, t0, 3",
    "mv t3, t1",
    "vs8r.v v0, (t3)",
    "add t3, t3, t2",
    "vs8r.v v8, (t3)",
    "add t3, t3, t2",
    "vs8r.v v16, (t3)",
    "add t3, t3, t2",
    "vs8r.v v24, (t3)",
    "csrr t4, vcsr",
    "sd t4, 16(a0)",
    "csrr t4, vl",
    "sd t4, 24(a0)",
    "csrr t4, vtype",
    "sd t4, 32(a0)",
    "sd t0, 8(a0)",
    "fence w, w",
    "li t4, 1",
    "sd t4, 40(a0)",
    "li a0, 0",
    "la t0, VFRESH_EXIT",
    "ld t0, 0(t0)",
    "jr t0",
);

/// The library's `thread_exit` entry, for the fresh thread above (which has no registers to spare for it).
#[no_mangle]
static mut VFRESH_EXIT: usize = 0;

/// Edge states: the first vector instruction is a `vsetvli` (not a CSR read); an illegal `vtype` (`vill`) and a non-zero `vstart` survive a trap
/// (a yield). Each is one assembly block, with the system call inside it: compiled code in between could use the vector unit and change the
/// state (the user build is RVA23U64). Returns 0 or a code for the first thing that was wrong.
fn vec_edges() -> i32 {
    let (vl, vt): (u64, u64);
    // SAFETY: only changes `vl` and `vtype` (the first vector instruction is `vsetvli`); the system call preserves all registers but a0-a2.
    unsafe {
        asm!(
            "vsetvl {t}, {avl}, {vtype}",
            "csrr {vl}, vl",
            "csrr {vt}, vtype",
            t = out(reg) _,
            avl = in(reg) 3usize,
            vtype = in(reg) (2u64 << 3) | 0xc0,
            vl = out(reg) vl,
            vt = out(reg) vt,
        );
    }
    if vl != 3 || vt >> 63 != 0 {
        return 1;
    }
    let (vl, vt, vl2, vt2): (u64, u64, u64, u64);
    // SAFETY: an illegal `vtype` (a reserved bit) sets `vill` and clears `vl`; then a yield (the thread may be switched out, and back on another
    // hart) and the state is read again.
    unsafe {
        asm!(
            "vsetvl zero, zero, {bad}",
            "csrr {vl}, vl",
            "csrr {vt}, vtype",
            "jalr {lib}",
            "csrr {vl2}, vl",
            "csrr {vt2}, vtype",
            bad = in(reg) 1u64 << 8,
            vl = out(reg) vl,
            vt = out(reg) vt,
            vl2 = out(reg) vl2,
            vt2 = out(reg) vt2,
            lib = in(reg) rt::library::entry(rt::abi::syscall::THREAD_YIELD),
            out("ra") _, out("a7") _,
            out("a0") _, out("a1") _, out("a2") _,
        );
    }
    if vt >> 63 != 1 || vl != 0 {
        return 2;
    }
    if vt2 >> 63 != 1 || vl2 != 0 {
        return 3;
    }
    let (v1, v2): (u64, u64);
    // SAFETY: writes `vstart` (the element a trapped vector instruction resumes at), yields, and reads it back; clears it again.
    unsafe {
        asm!(
            "csrw vstart, {two}",
            "jalr {lib}",
            "csrr {v1}, vstart",
            "jalr {lib}",
            "csrr {v2}, vstart",
            "csrw vstart, zero",
            two = in(reg) 2u64,
            v1 = out(reg) v1,
            v2 = out(reg) v2,
            lib = in(reg) rt::library::entry(rt::abi::syscall::THREAD_YIELD),
            out("ra") _, out("a7") _,
            out("a0") _, out("a1") _, out("a2") _,
        );
    }
    if v1 != 2 || v2 != 2 {
        return 4;
    }
    0
}

/// First vector instruction is a vector load (a whole-register load from a zeroed buffer): the unit must be enabled by it too.
fn vec_load_first() -> i32 {
    // SAFETY: only the address of the last buffer is taken; the load reads 16 bytes at most (one register, at least 16 bytes long).
    let p = unsafe { core::ptr::addr_of_mut!(VBUFS.0) as *mut u8 }.wrapping_add(5 * 32 * VMAX);
    // SAFETY: reads one vector register's worth of the buffer and writes `v0`.
    unsafe {
        asm!(".option push", ".option arch, +v", "vl1re8.v v0, ({p})", ".option pop", p = in(reg) p)
    };
    0
}

#[repr(align(4096))]
struct Page(#[allow(dead_code)] [u8; 4096]);
static mut CBO_PAGE: Page = Page([0; 4096]);

/// PL3: `c[i] = a[i] + b[i]` over 100 words, strip-mined with `vsetvli`, then `cbo.zero` on the first block of a page. Returns 0 if both worked.
fn rva23_loop_and_cbo() -> i32 {
    let a: [u32; 100] = core::array::from_fn(|i| i as u32);
    let b: [u32; 100] = core::array::from_fn(|i| 3 * i as u32);
    let mut c = [0u32; 100];
    let (mut pa, mut pb, mut pc, mut n) = (a.as_ptr(), b.as_ptr(), c.as_mut_ptr(), 100usize);
    // SAFETY: reads `a` and `b` and writes `c`, 100 words each, in strips of `vl` words; the vector unit is enabled by the first instruction.
    unsafe {
        asm!(
            "1:",
            "vsetvli {vl}, {n}, e32, m1, ta, ma",
            "vle32.v v0, ({a})",
            "vle32.v v1, ({b})",
            "vadd.vv v2, v0, v1",
            "vse32.v v2, ({c})",
            "slli {t}, {vl}, 2",
            "add {a}, {a}, {t}",
            "add {b}, {b}, {t}",
            "add {c}, {c}, {t}",
            "sub {n}, {n}, {vl}",
            "bnez {n}, 1b",
            a = inout(reg) pa, b = inout(reg) pb, c = inout(reg) pc, n = inout(reg) n,
            vl = out(reg) _, t = out(reg) _,
            out("v0") _, out("v1") _, out("v2") _,
        );
    }
    let _ = (pa, pb, pc, n);
    if (0..100).any(|i| c[i] != 4 * i as u32) {
        return 1;
    }
    // SAFETY: only the address of the static page is taken; it is written here by this thread alone.
    let page = unsafe {
        core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(CBO_PAGE) as *mut u8, 4096)
    };
    page.fill(0xff);
    // SAFETY: `cbo.zero` zeroes the cache block that holds the address (it needs Zicboz and the kernel's `senvcfg.CBZE`).
    unsafe { asm!("cbo.zero 0({p})", p = in(reg) page.as_mut_ptr()) };
    if page[..64].iter().any(|&x| x != 0) || page[256..].iter().any(|&x| x != 0xff) {
        return 2;
    }
    0
}

static VEC_THREAD_RESULT: [AtomicU32; 2] = [AtomicU32::new(99), AtomicU32::new(99)];

extern "C" fn vec_worker(k: usize) -> ! {
    let r = vec_keep(1000 + k as u64 * 77, 1, 1 + k) as u32;
    VEC_THREAD_RESULT[k].store(r, Ordering::Release);
    rt::sys::thread_exit(0)
}

fn vec_threads() -> i32 {
    let me = rt::sys::process_self().a1 as u64;
    #[allow(clippy::needless_range_loop)]
    for k in 0..2usize {
        // SAFETY: only the address of the static stack is taken; each worker owns its own.
        let top = unsafe { core::ptr::addr_of_mut!(WORKER_STACKS.0[k]) } as u64 + 8192;
        let t =
            rt::sys::thread_on_stack(me, vec_worker as *const () as usize as u64, top, k as u64);
        if t.status != 0 {
            return 900;
        }
    }
    let mine = vec_keep(5, 0, 0);
    for _ in 0..1_000_000u32 {
        if VEC_THREAD_RESULT
            .iter()
            .all(|r| r.load(Ordering::Acquire) != 99)
        {
            let worst = VEC_THREAD_RESULT
                .iter()
                .map(|r| r.load(Ordering::Acquire) as i32)
                .max()
                .unwrap_or(0);
            return mine.max(worst);
        }
        rt::sys::thread_yield();
    }
    901
}

#[inline(never)] // (T266: its buffers stay out of the frame of `main`, where child threads start on 16 KiB stacks)
fn socket_hand() -> i32 {
    use rt::abi::signals as sig;
    let reply = |tag: u8, a: u32, b: i32, c: u32, h: Option<u64>| {
        let mut m = [0u8; 13];
        m[0] = tag;
        m[1..5].copy_from_slice(&a.to_le_bytes());
        m[5..9].copy_from_slice(&b.to_le_bytes());
        m[9..13].copy_from_slice(&c.to_le_bytes());
        let hs = [h.unwrap_or(0)];
        rt::sys::channel_write(
            rt::BOOTSTRAP,
            m.as_ptr(),
            m.len(),
            hs.as_ptr(),
            usize::from(h.is_some()),
        );
    };
    let mut kept = 0u64;
    let mut sent = 0u64; // (the stream offset of the next 'P' byte)
    loop {
        let w = rt::sys::object_wait(rt::BOOTSTRAP, sig::READABLE | sig::PEER_CLOSED, u64::MAX);
        let mut m = [0u8; 8];
        let mut h = [0u64; 1];
        let r = rt::sys::channel_read(rt::BOOTSTRAP, m.as_mut_ptr(), 8, h.as_mut_ptr(), 1);
        if r.status != 0 {
            if w.a1 as u32 & sig::PEER_CLOSED != 0 {
                return 0;
            }
            continue;
        }
        match m[0] {
            b'C' => {
                let p = rt::sys::socket_create();
                if p.status != 0 {
                    reply(b'C', 0, p.status as i32, 0, None);
                    continue;
                }
                kept = p.a1 as u64;
                reply(b'C', 0, 0, 0, Some(p.a2 as u64));
            }
            b'H' => {
                if r.a2 == 1 {
                    kept = h[0];
                }
                reply(b'H', 0, 0, 0, None);
            }
            b'W' => {
                let block = [7u8; 1024];
                let (mut done, mut last) = (0u32, 0i32);
                for _ in 0..usize::from(m[1]) * 4 {
                    // (m[1] counts 4 KiB units; writes go in 1 KiB pieces so a short write shows)
                    let mut left = 1024;
                    while left > 0 {
                        let w = rt::sys::socket_write(kept, block.as_ptr(), left);
                        last = w.status as i32;
                        if w.status != 0 {
                            break;
                        }
                        done += w.a1 as u32;
                        left -= w.a1;
                    }
                    if last != 0 {
                        break;
                    }
                }
                reply(
                    b'W',
                    done,
                    last,
                    rt::sys::object_signals(kept).a1 as u32,
                    None,
                );
            }
            b'R' => {
                let mut buf = [0u8; 1024];
                let mut total = 0u32;
                loop {
                    let r = rt::sys::socket_read(kept, buf.as_mut_ptr(), 1024);
                    if r.status != 0 || r.a1 == 0 {
                        break;
                    }
                    total += r.a1 as u32;
                }
                reply(b'R', total, 0, 0, None);
            }
            b'P' => {
                // (T266) like 'W', but the bytes are the stream pattern at their offset (`stream_byte`), so the reader can check every byte
                let mut block = [0u8; 1024];
                let (mut done, mut last) = (0u32, 0i32);
                let want = u64::from(m[1]) * 4096;
                while u64::from(done) < want {
                    let n = 1024.min((want - u64::from(done)) as usize);
                    for (i, b) in block[..n].iter_mut().enumerate() {
                        *b = stream_byte(sent + i as u64);
                    }
                    let w = rt::sys::socket_write(kept, block.as_ptr(), n);
                    last = w.status as i32;
                    if w.status != 0 {
                        break;
                    }
                    done += w.a1 as u32;
                    sent += w.a1 as u64;
                }
                reply(b'P', done, last, 0, None);
            }
            b'K' => {
                // (T266) a channel pair: this hand keeps one end, the other goes back
                let p = rt::sys::channel_create();
                if p.status != 0 {
                    reply(b'K', 0, p.status as i32, 0, None);
                    continue;
                }
                kept = p.a1 as u64;
                reply(b'K', 0, 0, 0, Some(p.a2 as u64));
            }
            b'M' => hand_send_messages(kept, m[1]),
            b'S' => hand_serve_calls(kept),
            b'X' => return 0,
            _ => {}
        }
    }
}

/// T266, the socket hand's 'M': write `n` messages on `ch`: message i is `msg_len(i)` bytes of `msg_byte(i, offset)` with two fresh events; a
/// full queue or a job at its IPC limit is waited out, and a closed peer ends it. (Not inlined: its 64 KiB buffer must not be in the frame of
/// `main`, which every child thread of this image starts in, on a stack of 16 KiB.)
#[inline(never)]
fn hand_send_messages(ch: u64, n: u8) {
    let mut buf = [0u8; 64 * 1024];
    for i in 0..u32::from(n) {
        let len = msg_len(i);
        for (o, b) in buf[..len].iter_mut().enumerate() {
            *b = msg_byte(i, o);
        }
        if !send_with_two_events(ch, &buf[..len]) {
            break;
        }
    }
}

/// T266, the socket hand's 'S': serve `channel_call`s on `ch` until the peer closes: a request is (txid, i); the reply is message i with the
/// txid in its first four bytes, and two fresh events. (Not inlined, as `hand_send_messages`.)
#[inline(never)]
fn hand_serve_calls(ch: u64) {
    use rt::abi::signals as sig;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let w = rt::sys::object_wait(ch, sig::READABLE | sig::PEER_CLOSED, u64::MAX);
        let mut req = [0u8; 8];
        let r = rt::sys::channel_read(ch, req.as_mut_ptr(), 8, core::ptr::null_mut(), 0);
        if r.status != 0 {
            if w.a1 as u32 & sig::PEER_CLOSED != 0 {
                return;
            }
            continue;
        }
        let i = u32::from_le_bytes([req[4], req[5], req[6], req[7]]);
        let len = msg_len(i);
        for (o, b) in buf[..len].iter_mut().enumerate() {
            *b = msg_byte(i, o);
        }
        buf[..4].copy_from_slice(&req[..4]);
        if !send_with_two_events(ch, &buf[..len]) {
            return;
        }
    }
}

/// T266: the byte at `offset` of a pattern-checked stream (the reader computes it from the offset, it is not echoed): not periodic at
/// 256 bytes, 1 KiB or 4 KiB, so a chunk copied to the wrong place or not at all shows.
fn stream_byte(offset: u64) -> u8 {
    (offset ^ (offset >> 8) ^ (offset >> 16)) as u8
}

/// T266: the length of message `i` of a channel kill storm: 1 to 64 KiB, the big copies of T215 (1 KiB and more go outside the lock).
fn msg_len(i: u32) -> usize {
    1024 * (1 + (i as usize * 37 + 11) % 64)
}

/// T266: the byte at `offset` of message `i` (the reader computes it).
fn msg_byte(i: u32, offset: usize) -> u8 {
    (offset ^ (offset >> 8) ^ (i as usize).wrapping_mul(31)) as u8
}

/// Write `bytes` with two fresh events on `ch`, waiting out a full queue or a job at its limit. False when the peer has closed (or
/// the events cannot be made).
fn send_with_two_events(ch: u64, bytes: &[u8]) -> bool {
    use rt::abi::signals as sig;
    let (a, b) = (rt::sys::event_create(), rt::sys::event_create());
    if a.status != 0 || b.status != 0 {
        return false;
    }
    let hs = [a.a1 as u64, b.a1 as u64];
    loop {
        let w = rt::sys::channel_write(ch, bytes.as_ptr(), bytes.len(), hs.as_ptr(), 2);
        if w.status == 0 {
            return true;
        }
        use rt::abi::status::Status as St;
        let waitable = w.status == St::ShouldWait as i32 || w.status == St::NoMemory as i32;
        if !waitable || rt::sys::object_signals(ch).a1 as u32 & sig::PEER_CLOSED != 0 {
            rt::sys::handle_close(hs[0]);
            rt::sys::handle_close(hs[1]);
            return false;
        }
        rt::io::nap(1);
    }
}
