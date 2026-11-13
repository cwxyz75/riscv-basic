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
            // touch the 4 stack pages the parent mapped at 0x20_0000_0000 (present in the page
            // table, so a thread started on them takes no first-touch fault), then block forever
            // Give the parent time to map the pages (a scheduler that runs the child first, as
            // the round-robin class does, would otherwise fault here before they exist).
            let ev = rt::sys::event_create().a1 as u32;
            rt::sys::object_wait(ev, rt::abi::signals::USER_MASK, rt::sys::clock_get() + 60);
            for page in 0..4usize {
                // SAFETY: the parent mapped these pages read-write before starting us.
                unsafe {
                    core::ptr::write_volatile((0x20_0000_0000usize + page * 4096) as *mut u8, 1)
                };
            }
            rt::sys::object_wait(rt::BOOTSTRAP, rt::abi::signals::USER_MASK, u64::MAX);
            return 9;
        }
        28 => {
            // vsetvli t0, zero, e8, m1, ta, ma: illegal while sstatus.VS is off
            // SAFETY: either traps (the process dies) or only changes vl/vtype and t0.
            unsafe { asm!(".4byte 0x0c0072d7", out("t0") _) };
            return 77;
        }
        26 => {
            // take a socket end from the bootstrap channel and drain it to the end of the
            // stream; the exit code is how many bytes arrived
            use rt::abi::status::Status;
            let mut hbuf = [0u32; 1];
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
            let port = rt::sys::port_create().a1 as u32;
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
            spawn(exit_eight, 0);
            rt::sys::thread_exit(1);
        }
        24 => {
            // create short-lived threads in a loop until killed
            let me = rt::sys::process_self().a1 as u32;
            let top = core::ptr::addr_of_mut!(STACK) as u64 + 8192;
            loop {
                let r = rt::sys::thread_create(me, exit_eight as *const () as usize as u64, top, 0);
                if r.status == 0 {
                    rt::sys::handle_close(r.a1 as u32);
                }
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
                let vmo = v.a1 as u32;
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
            if rt::sys::vmo_create_physical(77, 0x1000_1000, 4096).status == -1
                && rt::sys::vmo_create_dma(77, 1).status == -1
            {
                code |= 2;
            }
            let ev = rt::sys::event_create().a1 as u32;
            if rt::sys::vmo_create_physical(ev, 0x1000_1000, 4096).status == -2
                && rt::sys::vmo_create_dma(ev, 1).status == -2
            {
                code |= 4;
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
            let me = rt::sys::process_self().a1 as u32;
            let top1 = core::ptr::addr_of_mut!(STACK) as u64 + 8192;
            let first =
                rt::sys::thread_create(me, spin_forever as *const () as usize as u64, top1, 0);
            let second =
                rt::sys::thread_create(me, spin_forever as *const () as usize as u64, top1, 0);
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
                    let w = rt::sys::socket_write(pair.a1 as u32, block.as_ptr(), block.len());
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
        40 => return unmap_while_siblings_touch(),
        n if n & 0xff == 42 => return churn((n >> 8) as u64),
        41 => {
            // S5d: a process with two spinning sibling threads (and itself spinning): ended from
            // outside while they run on other harts.
            let me = rt::sys::process_self().a1 as u32;
            for k in 0..2 {
                // SAFETY: only the address of the static stack is taken; each new thread owns its own.
                let top = unsafe { core::ptr::addr_of_mut!(WORKER_STACKS.0[k]) } as u64 + 8192;
                rt::sys::thread_create(me, spin_forever as *const () as usize as u64, top, 0);
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

#[repr(align(16))]
struct WorkerStacks([[u8; 8192]; 2]);
static mut WORKER_STACKS: WorkerStacks = WorkerStacks([[0; 8192]; 2]);

#[repr(align(16))]
#[allow(dead_code)]
struct Stack([u8; 8192]);
static mut STACK: Stack = Stack([0; 8192]);

/// Start a second thread running `f(arg)` on a static stack; returns its thread handle.
fn spawn(f: extern "C" fn(usize) -> !, arg: usize) -> u32 {
    let me = rt::sys::process_self().a1 as u32;
    // Only the address of the static is taken; the one new thread uses it as its stack.
    let top = core::ptr::addr_of_mut!(STACK) as u64 + 8192;
    let r = rt::sys::thread_create(me, f as *const () as usize as u64, top, arg as u64);
    if r.status != 0 {
        rt::sys::exit(100 + (-(r.status as i64)) as i32);
    }
    r.a1 as u32
}

extern "C" fn spin_forever(_: usize) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

extern "C" fn exit_soon(_: usize) -> ! {
    let t = rt::sys::clock_get() + 3;
    // an event nobody signals: a timed wait
    let ev = rt::sys::event_create().a1 as u32;
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

// ---- S5d: remote TLB shootdown ----

use core::sync::atomic::{AtomicU64, Ordering};

/// Where the changing mapping lives.
const X: u64 = 0x30_0000_0000;
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

/// Two sibling threads read a page that the main thread unmaps and remaps, round after round,
/// with the harts they run on caching the translation in between. If an unmap did not reach the
/// other harts' TLBs, a worker would read the *previous* round's data through its stale entry
/// (a different frame from the new mapping). The exit code is 0, or the first failing round.
fn unmap_while_siblings_touch() -> i32 {
    let me = rt::sys::process_self().a1 as u32;
    for k in 0..2 {
        // SAFETY: only the address of the static stack is taken; each worker owns its own.
        let top = unsafe { core::ptr::addr_of_mut!(WORKER_STACKS.0[k]) } as u64 + 8192;
        let t = rt::sys::thread_create(me, reader as *const () as usize as u64, top, k as u64);
        if t.status != 0 {
            return 900 + (-(t.status as i64)) as i32;
        }
    }
    for round in 1..=ROUNDS {
        let v = rt::sys::vmo_create(1);
        if v.status != 0 {
            return 910;
        }
        let vmo = v.a1 as u32;
        let want = pattern(round).to_le_bytes();
        if rt::sys::vmo_write(vmo, 0, want.as_ptr(), 8).status != 0 {
            return 911;
        }
        // map it read-only at X (PF_R = 1)
        if rt::sys::process_map(me, vmo, X, 0, 4096, 1).status != 0 {
            return 912;
        }
        // a second VMO with other contents, to give a freed frame something to be reused for
        let junk = rt::sys::vmo_create(1).a1 as u32;
        let _ = rt::sys::vmo_write(junk, 0, [0xee; 8].as_ptr(), 8);
        PHASE.store((round << 2) | READ, Ordering::Release);
        let mut spins = 0u64;
        while READ_ACKS.load(Ordering::Acquire) < 2 * round {
            spins += 1;
            if spins > 400_000_000 {
                return 920;
            }
            core::hint::spin_loop();
        }
        PHASE.store((round << 2) | IDLE, Ordering::Release);
        spins = 0;
        while IDLE_ACKS.load(Ordering::Acquire) < 2 * round {
            spins += 1;
            if spins > 400_000_000 {
                return 921;
            }
            core::hint::spin_loop();
        }
        // nobody is reading X now; take the mapping away and free the frame
        if rt::sys::process_unmap(me, X).status != 0 {
            return 913;
        }
        rt::sys::handle_close(vmo);
        rt::sys::handle_close(junk);
        if FAIL.load(Ordering::Acquire) != 0 {
            return FAIL.load(Ordering::Acquire) as i32;
        }
    }
    FAIL.load(Ordering::Acquire) as i32
}

extern "C" fn reader(_id: usize) -> ! {
    let mut read_round = 0u64;
    let mut idle_round = 0u64;
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
        core::hint::spin_loop();
    }
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
    let me = rt::sys::process_self().a1 as u32;
    let buf = [0x42u8; 4096];
    let mut out = [0u8; 4096];
    for _ in 0..1500u32 {
        match rnd(7) {
            0 => {
                let c = rt::sys::channel_create();
                if c.status != 0 {
                    return 100;
                }
                let (a, b) = (c.a1 as u32, c.a2 as u32);
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
                let vmo = v.a1 as u32;
                if rt::sys::vmo_write(vmo, 0, buf.as_ptr(), 4096).status != 0 {
                    return 111;
                }
                let at = 0x31_0000_0000 + rnd(8) * 0x2_0000;
                if rt::sys::process_map(me, vmo, at, 0, 4 * 4096, 3).status != 0 {
                    return 112;
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
                let (a, b) = (p.a1 as u32, p.a2 as u32);
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
                if rt::sys::event_signal(e.a1 as u32, 0, rt::abi::signals::USER_MASK & 0x0100_0000)
                    .status
                    != 0
                {
                    return 131;
                }
                rt::sys::handle_close(e.a1 as u32);
            }
            4 => {
                let port = rt::sys::port_create();
                let ev = rt::sys::event_create();
                if port.status != 0 || ev.status != 0 {
                    return 140;
                }
                let (p, e) = (port.a1 as u32, ev.a1 as u32);
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
                    let t = rt::sys::thread_create(
                        me,
                        churn_worker as *const () as usize as u64,
                        top,
                        slot as u64,
                    );
                    if t.status != 0 {
                        CHURN_BUSY[slot].store(0, Ordering::Release);
                        return 150;
                    }
                    rt::sys::handle_close(t.a1 as u32);
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
