//! User mode (S3b): processes, the program loader, the syscall dispatcher and the
//! handling of traps from U-mode. The logic that can be pure (ELF validation, user memory
//! copies, handle tables, channels) lives in `kcore` and is tested on the host; this file
//! wires it to the machine.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::panic,
    clippy::unreachable
)] // KT3 (KERNEL-V1-SPEC 7.4): no panic a program can cause; remaining sites carry `#[allow] // INVARIANT:` (scripts/kernel_allows.py)

use crate::console::Console;
use crate::{irq, kmem, thread};
use abi::limits::{MAX_HANDLES, MAX_MSG_BYTES, MAX_MSG_HANDLES};
use abi::rights::Rights;
use abi::status::Status;
use abi::syscall as nr;
use abi::user::{MAX_DEBUG_WRITE, STACK_PAGES, STACK_TOP};
use abi::Handle;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use kcore::aspace::{Access, AddressSpace};
use kcore::elf;
use kcore::handle::HandleTable;
use kcore::ipc::{self, Kobjs, ReadError};
use kcore::job::{JobError, JobId, Resource};
use kcore::pagetable::{PageTable, Perms};
use kcore::sched::Tid;
use kcore::usercopy::{copy_from_user, copy_to_user_verified, verify_writable};
use ksys::{ProcState, Process, SpawnStack};

/// The embedded user programs (see build.rs); empty if they were not built.
pub static SUITE_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/suite.elf"));
pub static CRASH_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/crash.elf"));
/// The interactive supervisor (`init`, S4a): the first program of the interactive boot.
#[cfg_attr(not(feature = "interactive"), allow(dead_code))]
pub static INIT_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/init.elf"));
/// The boot image (S4-SPEC section 3): every user program and the fixture files, as one
/// archive that becomes a read-only VMO for `init`.
pub static BOOT_IMAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bootimg.bin"));
pub static HELLO_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/hello.elf"));

struct ThreadStart {
    pid: u32,
    entry: u64,
    sp: u64,
    arg: u64,
    obj: Option<kcore::object::ObjId>,
    /// `thread_spawn` extras: the `tp` register, the stack the kernel provisioned, round robin, and whether `a1` is the tid.
    tp: u64,
    stack: Option<SpawnStack>,
    rr: bool,
    spawned: bool,
}

/// A stack `thread_spawn` mapped for a thread: its region (start of the mapped stack) and the VMO behind it.
struct UserState {
    objs: Kobjs,
    procs: BTreeMap<u32, Process>,
    thread_pid: BTreeMap<Tid, u32>,
    /// Scheduling flags (bit 0: round robin) set by `thread_set_sched`, by thread.
    sched_flags: BTreeMap<Tid, u8>,
    next_pid: u32,
    /// VMOs whose last handle is gone but that some address space still maps: freed when the
    /// last mapping goes (SYSCALLS.md: a VMO lives while a handle or a mapping does).
    pending_vmos: Vec<u64>,
    /// Threads that are inside `cleanup_dead` (a per-thread recursion guard, T63: a global flag made every other
    /// hart skip the cleanup while one cleaner was preempted).
    cleaning: alloc::collections::BTreeSet<Tid>,
    /// Threads with a pending `thread_interrupt`: the next blocking wait that would sleep returns INTERRUPTED and takes the flag.
    interrupts: alloc::collections::BTreeSet<Tid>,
    /// Thread starts waiting for their thread to run, by token.
    starts: BTreeMap<usize, ThreadStart>,
    next_token: usize,
    /// Handles the kernel itself holds (the creator's ends of bootstrap channels).
    ktable: HandleTable,
    /// The futex wait table (S7b).
    futexes: kcore::futex::Futexes,
    /// Threads waiting on an object's signals (slice W step 3): woken by id when a touched object's
    /// signals match, instead of by the global queue.
    waits: kcore::wait::WaitTable,
    /// The interrupt object that owns each PLIC source (S6a-2).
    irq_objs: [Option<kcore::object::ObjId>; abi::layout::MAX_IRQ as usize + 1],
    /// Output of user programs, assembled into lines.
    line: String,
    pass: u32,
    fail: u32,
}

impl UserState {
    /// Every object something outside the registry can still reach: all processes' handles
    /// **and the kernel's own** (the creator ends of bootstrap channels). Any sweep must use
    /// all of these (a root left out is destroyed, whatever still points at it).
    fn roots(&self) -> Vec<kcore::object::ObjId> {
        self.procs
            .values()
            .flat_map(|q| q.handles.objects())
            .chain(self.ktable.objects())
            .collect()
    }
}

/// The quota to charge new objects to: `job` (the creating process's, or the root's for the
/// kernel's own objects).
fn quota(job: JobId) -> ipc::Quota<'static> {
    ipc::Quota {
        jobs: &mut kmem::get().vmos.jobs,
        job: Some(job),
    }
}

static mut USER: Option<UserState> = None;

#[track_caller]
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
fn state() -> &'static mut UserState {
    debug_assert!(
        !irq::enabled() && crate::bkl::held(),
        "user state touched without the big kernel lock"
    );
    // SAFETY: the big kernel lock is held (debug-checked); every caller drops the reference
    // before any context switch, so no two live `&mut` alias.
    unsafe {
        let slot = &mut *addr_of_mut!(USER);
        slot.get_or_insert_with(|| UserState {
            objs: Kobjs::new(),
            procs: BTreeMap::new(),
            thread_pid: BTreeMap::new(),
            sched_flags: BTreeMap::new(),
            next_pid: 1,
            pending_vmos: Vec::new(),
            cleaning: alloc::collections::BTreeSet::new(),
            interrupts: alloc::collections::BTreeSet::new(),
            starts: BTreeMap::new(),
            next_token: 1,
            ktable: HandleTable::new(256),
            futexes: kcore::futex::Futexes::new(),
            waits: kcore::wait::WaitTable::new(),
            irq_objs: [None; abi::layout::MAX_IRQ as usize + 1],
            line: String::new(),
            pass: 0,
            fail: 0,
        })
    }
}

/// The `##TEST` results user programs have reported so far (pass, fail).
pub fn reported() -> (u32, u32) {
    irq::without(|| {
        let s = state();
        (s.pass, s.fail)
    })
}

/// How many processes exist right now (a leak counter for tests).
/// (jobs alive including the root, and the root job's live processes, threads, objects and queued
/// IPC bytes): `(1, 0, 0, 0, 0)` when nothing is left (a leak oracle for the ledger, INVARIANTS
/// J2). Applies the credits of destroyed objects first.
pub fn job_ledger() -> (usize, u64, u64, u64, u64) {
    irq::without(|| {
        ipc::settle(&mut state().objs, &mut kmem::get().vmos.jobs);
        let jobs = &kmem::get().vmos.jobs;
        let u = jobs.usage(JobId::ROOT).unwrap_or_default();
        (jobs.live(), u.processes, u.threads, u.objects, u.ipc_bytes)
    })
}

pub fn process_count() -> usize {
    irq::without(|| state().procs.len())
}

/// Threads sleeping on a futex (for leak checks).
pub fn futex_sleepers() -> usize {
    irq::without(|| state().futexes.len())
}

/// The number of live kernel objects (for leak checks).
pub fn live_objects() -> usize {
    irq::without(|| state().objs.live())
}

/// Why a program could not be started (the detail is shown through `Debug` in test failures).
#[derive(Debug)]
#[allow(dead_code)]
pub enum LoadError {
    Elf(elf::ElfError),
    NoMemory,
    NoProgram,
    /// The kernel's top-level page-table slots changed since boot (new processes would not see them).
    KernelHalfChanged,
}

/// Load `image` into a new process and start it with start argument `arg`. Returns the
/// process id and the id of its thread.
/// Build a process from `image` and start its first thread. The whole construction runs under the
/// big kernel lock (it touches the frame database, the VMO store and the process table).
pub fn start(image: &[u8], arg: u64) -> Result<(u32, Tid), LoadError> {
    irq::without(|| start_locked(image, arg))
}

#[allow(clippy::large_stack_frames)]
// KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing
)]
// INVARIANT: boot-time process start: the segments were checked by `elf::parse` (offsets and sizes inside the file, addresses in the user half); pids are a counter far below u32::MAX
fn start_locked(image: &[u8], arg: u64) -> Result<(u32, Tid), LoadError> {
    if image.is_empty() {
        return Err(LoadError::NoProgram);
    }
    let parsed = elf::parse(image).map_err(LoadError::Elf)?;
    if !kmem::kernel_half_unchanged() {
        return Err(LoadError::KernelHalfChanged);
    }
    let k = kmem::get();
    let table =
        PageTable::new_with_kernel_half(&mut k.mm, k.aspace.table()).ok_or(LoadError::NoMemory)?;
    let mut aspace = AddressSpace::new(table);
    let mut vmo_ids: Vec<u64> = Vec::new();
    let built = (|| -> Result<(), LoadError> {
        for seg in &parsed.segments {
            let (first, end) = seg.pages();
            let id = k.vmos.create(end - first);
            vmo_ids.push(id);
            let bytes = &image[seg.offset as usize..(seg.offset + seg.filesz) as usize];
            k.vmos
                .write(&mut k.mm, id, seg.vaddr % 4096, bytes)
                .map_err(|_| LoadError::NoMemory)?;
            aspace
                .map_region(&k.vmos, first * 4096, end - first, id, 0, seg.perms)
                .map_err(|_| LoadError::NoMemory)?;
        }
        let stack = k.vmos.create(STACK_PAGES);
        vmo_ids.push(stack);
        aspace
            .map_region(
                &k.vmos,
                STACK_TOP - STACK_PAGES * 4096,
                STACK_PAGES,
                stack,
                0,
                Perms::RW.user(),
            )
            .map_err(|_| LoadError::NoMemory)?;
        Ok(())
    })();
    let (pid, asid) = irq::without(|| {
        let s = state();
        let pid = s.next_pid;
        s.next_pid = pid.wrapping_add(1).max(1);
        (pid, (pid & 0xffff) as u16)
    });
    if let Err(e) = built {
        aspace.destroy_user(&mut k.mm);
        for id in vmo_ids {
            let _ = k.vmos.destroy(&mut k.mm, id);
        }
        return Err(e);
    }
    let process = Process {
        aspace,
        handles: HandleTable::new(MAX_HANDLES),
        asid,
        vmos: vmo_ids,
        entry: parsed.entry,
        arg,
        exit_code: None,
        kernel_end: None,
        start_sp: 0,
        obj: None,
        state: ProcState::Running,
        detached: false,
        killed: None,
        stopped: false,
        threads_alive: 1,
        thread_objs: BTreeMap::new(),
        job: JobId::ROOT,
        table_charge: BTreeMap::new(),
        stacks: BTreeMap::new(),
        stack_hint: abi::user::STACK_ARENA_BASE,
        privileged: true,
    };
    // Every process starts with one handle, value 1: the bootstrap channel (SYSCALLS.md).
    let mut process = process;
    let made = irq::without(|| -> bool {
        let s = state();
        let Ok((kernel_end, process_end)) =
            ipc::channel_create_in(&mut s.objs, &mut s.ktable, &mut quota(JobId::ROOT))
        else {
            return false;
        };
        let entry = s
            .ktable
            .remove_for_transfer(&mut s.objs, process_end)
            .expect("a fresh handle can be transferred");
        let first = process
            .handles
            .insert(entry.obj, entry.rights)
            .expect("an empty table has room");
        assert_eq!(
            first,
            abi::BOOTSTRAP_HANDLE,
            "the bootstrap channel must be handle 1"
        );
        process.kernel_end = Some(kernel_end);
        true
    });
    if !made {
        process.aspace.destroy_user(&mut k.mm);
        for id in process.vmos {
            let _ = k.vmos.destroy(&mut k.mm, id);
        }
        return Err(LoadError::NoMemory);
    }
    let token = irq::without(|| {
        let s = state();
        let (entry, arg) = (process.entry, process.arg);
        // (the root job has no limits: these charges only keep its usage honest)
        let jobs = &mut kmem::get().vmos.jobs;
        let _ = jobs.charge(JobId::ROOT, Resource::Processes, 1);
        let _ = charge_thread(jobs, JobId::ROOT);
        s.procs.insert(pid, process);
        new_start(s, pid, entry, STACK_TOP, arg, None)
    });
    // user threads may run on any hart (kernel threads stay on the boot hart by default)
    let tid = thread::spawn_free("user", 10, user_thread_main, token);
    Ok((pid, tid))
}

/// Register a thread start and return its token.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: a u64 counter that gains one per thread start
fn new_start(
    s: &mut UserState,
    pid: u32,
    entry: u64,
    sp: u64,
    arg: u64,
    obj: Option<kcore::object::ObjId>,
) -> usize {
    let token = s.next_token;
    s.next_token += 1;
    s.starts.insert(
        token,
        ThreadStart {
            pid,
            entry,
            sp,
            arg,
            obj,
            tp: 0,
            stack: None,
            rr: false,
            spawned: false,
        },
    );
    token
}

#[allow(clippy::expect_used)] // INVARIANT: the start record was registered by the spawner for this token, and the process of a running thread exists (both under the big lock)
fn user_thread_main(token: usize) {
    let top = thread::current_stack_top();
    // Interrupts stay off from here until `enter_user` is in U-mode: a kill that arrives in
    // between would otherwise be seen only at the next trap, after user code had run. The big
    // kernel lock is held while the thread's start is looked up and dropped just before the
    // jump to user mode (nothing user-visible runs under it).
    let _ = irq::save_and_disable();
    crate::bkl::lock();
    let (ts, satp, killed) = {
        let s = state();
        let ts = s
            .starts
            .remove(&token)
            .expect("a thread start was registered");
        // Registered here, by the thread itself, before it can make a syscall: the spawner
        // cannot be preempted into leaving a window where the thread is unknown.
        s.thread_pid.insert(thread::current(), ts.pid);
        let p = s
            .procs
            .get_mut(&ts.pid)
            .expect("a process lives while one of its threads does");
        if let Some(o) = ts.obj {
            p.thread_objs.insert(thread::current(), o);
        }
        if let Some(st) = ts.stack {
            p.stacks.insert(thread::current(), st);
        }
        if ts.rr {
            s.sched_flags.insert(thread::current(), 1);
        }
        let satp = satp_of(p);
        // A thread that was created before its job's maximum was lowered has not been seen by the clamp (it was in
        // no table yet): clamp it now (PR1; review of task 8).
        let max = kmem::get()
            .vmos
            .jobs
            .sched(p.job)
            .map_or(63, |j| j.max_prio)
            .max(1);
        let late_clamp = thread::priority_of(thread::current()).is_some_and(|now| now > max);
        if late_clamp {
            thread::set_priority(thread::current(), max);
        }
        (ts, satp, p.killed)
    };
    // Killed before it ever ran: it never runs user code.
    if let Some(code) = killed {
        crate::bkl::unlock();
        irq::enable();
        leave(code, true);
    }
    // A thread created in a stopped process waits here: it runs no user code before `process_resume` (or dies with the
    // process).
    park_while_stopped();
    // (the return to user mode is a defined point for a switch a wake asked for: X7)
    thread::preempt_point();
    // The marks above were read before `preempt_point`, which can switch and let a kill or stop land (and another thread on this hart
    // record the new epoch) before this thread resumes: its first trap takes the full path, which looks again (PR 267 review)
    crate::percpu::this()
        .attention_seen
        .store(0, Ordering::SeqCst);
    // The lock is released here, with interrupts still off: only `enter_user`'s own instructions
    // (loads from this thread's stack and CSR writes) run before `sret`.
    crate::bkl::unlock();
    user_mode_entered();
    // SAFETY: `enter_user` switches to the process's address space and drops to U-mode at
    // its entry point with the stack and argument chosen above; it never returns.
    unsafe {
        enter_user(
            ts.entry,
            ts.sp,
            ts.arg,
            satp,
            top,
            ts.tp,
            if ts.spawned {
                u64::from(thread::current())
            } else {
                0
            },
        )
    }
}

core::arch::global_asm!(
    r#"
    .section .text
    .globl enter_user
    .balign 4
enter_user:                      # a0 entry, a1 user sp, a2 arg, a3 satp, a4 kernel stack top, a5 tp, a6 a1 for the thread
    csrci sstatus, 2             # interrupts off until we are in U-mode
    csrw  satp, a3
    sfence.vma zero, zero
    csrw  sepc, a0
    sd    a4, 0(tp)              # PerHart.kstack_top: where the next trap from user mode lands
    csrw  sscratch, tp           # from now on a trap is a trap from user mode
    li    t0, 256
    csrc  sstatus, t0            # SPP = 0: sret goes to U-mode
    li    t0, 32
    csrs  sstatus, t0            # SPIE = 1: interrupts on in U-mode
    li    t0, 0x6600
    csrc  sstatus, t0            # FP and vector units off: a new thread has no FP state
    mv    sp, a1
    mv    a0, a2
    li x1, 0
    li x3, 0
    mv x4, a5
    li x5, 0
    li x6, 0
    li x7, 0
    li x8, 0
    li x9, 0
    mv x11, a6
    li x12, 0
    li x13, 0
    li x14, 0
    li x15, 0
    li x16, 0
    li x17, 0
    li x18, 0
    li x19, 0
    li x20, 0
    li x21, 0
    li x22, 0
    li x23, 0
    li x24, 0
    li x25, 0
    li x26, 0
    li x27, 0
    li x28, 0
    li x29, 0
    li x30, 0
    li x31, 0
    sret
"#
);

extern "C" {
    fn enter_user(entry: u64, sp: u64, arg: u64, satp: u64, kernel_top: u64, tp: u64, a1: u64)
        -> !;
}

/// Wait for a process's thread to finish, tear the process down, and return its exit code.
pub fn finish(pid: u32, tid: Tid) -> i32 {
    thread::join(tid);
    // Other threads of the process may outlive the first one.
    while irq::without(|| state().procs.get(&pid).map_or(0, |p| p.threads_alive)) > 0 {
        thread::sleep_ticks(1);
    }
    teardown(pid)
}

/// Take a process apart: close its handle table, sweep what nothing can reach any more,
/// mark its process object TERMINATED, free its address space and memory, and wake
/// everything that was waiting on any of that. Returns its exit code. Runs with interrupts
/// off, never holds a borrow across `notify`, and is safe to call from the dying thread
/// itself (it first switches to the kernel page table, whose upper half every process
/// shares).
fn teardown(pid: u32) -> i32 {
    // Back to the kernel's own page table before the process's root is freed.
    kmem::use_kernel_table();
    let Some(mut process) = irq::without(|| {
        let s = state();
        s.thread_pid.retain(|_, p| *p != pid);
        // nobody of this process sleeps on a futex any more (a killed sleeper has normally left
        // already; this is the backstop)
        s.futexes.forget_process(pid);
        s.procs.remove(&pid)
    }) else {
        // Another killer got here first (two kills of a never-started process can race): it
        // does the teardown.
        return i32::MIN;
    };
    // A process that vanished without a recorded code is not mistaken for `exit(-1)`.
    let code = process.exit_code.or(process.killed).unwrap_or(i32::MIN);
    irq::without(|| {
        let s = state();
        process.handles.drain(&mut s.objs);
        if let Some(end) = process.kernel_end.take() {
            let _ = s.ktable.close(&mut s.objs, end);
        }
        // Anything still alive that nothing can reach (reference cycles). The roots are
        // computed here, in the same interrupts-off step as the sweep, and include the
        // kernel's own handles.
        let roots = s.roots();
        s.objs.sweep(&roots);
        if let Some(obj) = process.obj {
            ipc::process_mark_terminated(&mut s.objs, obj, code);
        }
        if !s.line.is_empty() {
            let line = core::mem::take(&mut s.line);
            let _ = writeln!(Console, "{line}");
        }
    });
    irq::without(|| {
        let k = kmem::get();
        process.aspace.destroy_user(&mut k.mm);
        for id in process.vmos {
            let _ = k.vmos.destroy(&mut k.mm, id);
        }
        // the stacks `thread_spawn` mapped for threads that were still alive (their mappings went with the address space)
        for st in process.stacks.values() {
            let _ = k.vmos.destroy(&mut k.mm, st.vmo);
        }
        let tables: u64 = process.table_charge.values().sum();
        // the address space and its page tables are gone: no hart may keep translations through them
        crate::smp::shootdown();
        k.vmos.jobs.credit(process.job, Resource::Pages, tables);
        k.vmos.jobs.credit(process.job, Resource::Processes, 1);
        k.vmos.jobs.release(process.job);
    });
    kmem::flush_tlb();
    REAP_DIRTY.store(true, Ordering::SeqCst); // its mappings are gone: pending VMOs may go
    notify(); // peers of what this process held now see PEER_CLOSED; waiters see TERMINATED
    code
}

/// Unmap and free the stack `thread_spawn` gave thread `tid`, if it has one. The thread is in the kernel, on its
/// kernel stack, so the stack it stood on in user mode is nobody's; the shootdown comes before the frames can be reused.
fn free_spawn_stack(p: &mut Process, tid: Tid) {
    let Some(st) = p.stacks.remove(&tid) else {
        return;
    };
    let k = kmem::get();
    let _ = p.aspace.unmap_region(&mut k.mm, st.region);
    if let Some(tables) = p.table_charge.remove(&st.region) {
        k.vmos.jobs.credit(p.job, Resource::Pages, tables);
    }
    // other harts may cache translations of the stack: they go before the frames can be reused
    crate::smp::shootdown();
    kmem::flush_tlb();
    let _ = k.vmos.destroy(&mut k.mm, st.vmo);
}

fn exit_process(code: i32) -> ! {
    leave(code, true)
}

/// The calling thread ends. With `whole`, its process ends too (`process_exit`, a fault, a
/// kill): every other thread is told to die. Without it (`thread_exit`) only this thread goes,
/// and the process ends when the last thread does. The process is torn down by its last thread
/// (a process started by `process_start`), or by `finish` (one the kernel started).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: threads_alive >= 1 for a thread that is leaving
fn leave(code: i32, whole: bool) -> ! {
    if whole {
        LAST_EXIT_CYCLES.store(crate::timer::now(), Ordering::SeqCst);
    }
    let me = irq::without(|| {
        let s = state();
        let tid = thread::current();
        crate::fp::thread_gone(tid);
        s.sched_flags.remove(&tid);
        s.interrupts.remove(&tid);
        let pid = s.thread_pid.remove(&tid)?;
        let UserState { objs, procs, .. } = s;
        let p = procs.get_mut(&pid)?;
        // A code the process was already given by a kill wins over this thread's own.
        let code = p.killed.unwrap_or(code);
        if let Some(o) = p.thread_objs.remove(&tid) {
            ipc::thread_mark_terminated(objs, o, code);
        }
        p.threads_alive -= 1;
        credit_thread(&mut kmem::get().vmos.jobs, p.job);
        if !whole {
            // (a whole-process exit leaves the stacks to the teardown)
            free_spawn_stack(p, tid);
        }
        if whole {
            p.exit_code.get_or_insert(code);
            p.killed.get_or_insert(code);
            bump_attention();
            // (siblings spinning in user mode on other harts must be told: with no periodic tick (L13) nothing else would stop them)
            if !cfg!(feature = "fault-skip-kill-ipi") {
                crate::smp::kick_others();
            }
        } else if p.threads_alive == 0 {
            p.exit_code.get_or_insert(code);
        }
        Some((pid, p.detached, p.threads_alive))
    });
    match me {
        Some((pid, _, alive)) if alive > 0 => {
            notify(); // waiters on this thread
            wake_threads_of(pid); // siblings (a futex sleeper is on no queue)
        }
        Some((pid, true, _)) => {
            teardown(pid);
        }
        // the last thread of a kernel-started process: `finish` tears it down, but waiters
        // on this thread must not have to wait for that poll
        _ => notify(),
    }
    thread::exit()
}

/// Whether the calling thread has a pending `thread_interrupt` (for wait conditions; interrupts are off or the lock is held).
fn interrupted_now() -> bool {
    state().interrupts.contains(&thread::current())
}

/// Take the calling thread's pending interrupt, if any: the blocking call that reports INTERRUPTED does this.
fn take_interrupt() -> bool {
    irq::without(|| state().interrupts.remove(&thread::current()))
}

/// The exit code the calling thread's process was told to die with (`process_kill`), if any.
fn killed_now() -> Option<i32> {
    let s = state();
    let pid = s.thread_pid.get(&thread::current())?;
    s.procs.get(pid)?.killed
}

/// Where a stopped process stops and a killed one dies: the calling thread's return to user mode. A thread of a process
/// that `process_suspend` stopped sleeps here (woken by id, like a futex sleeper) until `process_resume` or a kill; a
/// killed process ends here too. Nothing is held across the sleep but the big lock, which follows the thread as it does
/// in every blocking call.
fn park_while_stopped() {
    loop {
        if let Some(code) = irq::without(killed_now) {
            exit_process(code);
        }
        if !irq::without(stopped_now) {
            return;
        }
        commit_wake(); // (wakes this trap made are committed before this thread sleeps, as at every blocking site)
        thread::wait_deadline_private(|| killed_now().is_some() || !stopped_now(), u64::MAX);
    }
}

/// Is the calling thread's process stopped (`process_suspend`)?
fn stopped_now() -> bool {
    let s = state();
    s.thread_pid
        .get(&thread::current())
        .and_then(|pid| s.procs.get(pid))
        .is_some_and(|p| p.stopped)
}

fn satp_of(p: &Process) -> u64 {
    (8u64 << 60) | (u64::from(p.asid) << 44) | p.aspace.table().root()
}

/// Make the calling thread's process the current address space, just before returning to
/// user mode. Needed because the hart's satp belongs to whichever user thread last ran:
/// threads of different processes are switched between, and the kernel half is shared so
/// kernel code runs under any of them, but returning to user mode must use *this* one.
pub fn activate_current() {
    // (the epoch first, then the marks: a kill that lands after this load moves the epoch and is seen at the next trap)
    crate::percpu::this()
        .attention_seen
        .store(ATTENTION.load(Ordering::SeqCst), Ordering::SeqCst);
    // A process that was killed while its thread was not running dies here, at the point
    // where it would otherwise return to user mode.
    park_while_stopped();
    let satp = irq::without(|| {
        let s = state();
        s.thread_pid
            .get(&thread::current())
            .and_then(|pid| s.procs.get(pid))
            .map(satp_of)
    });
    if let Some(satp) = satp {
        if crate::percpu::this()
            .active_satp
            .swap(satp, Ordering::SeqCst)
            != satp
            || kmem::satp() != satp
        {
            // SAFETY: `satp` names a live process root whose upper half is the kernel's; the
            // flush drops the translations of whatever table was active.
            unsafe { core::arch::asm!("csrw satp, {}", "sfence.vma zero, zero", in(reg) satp) };
        }
    }
}

/// The attention epoch (L12b): bumped, under the big lock, whenever a process is marked killed or stopped, **after** the mark is set and
/// before any IPI that tells a hart to look. A hart that returns to user mode records the epoch it saw (`activate_current`, before it
/// reads the marks); a trap from user mode that finds the epoch unchanged has nothing to die or stop for, and needs no big lock.
static ATTENTION: AtomicU64 = AtomicU64::new(1);

fn bump_attention() {
    ATTENTION.fetch_add(1, Ordering::SeqCst);
}

/// Has nothing happened since this hart last returned to user mode that could end or stop the thread it is about to resume? (No
/// lock needed: both sides are atomics, and the marks are set before the epoch moves.)
pub fn user_return_is_quiet() -> bool {
    crate::percpu::this().attention_seen.load(Ordering::SeqCst) == ATTENTION.load(Ordering::SeqCst)
}

/// A trap from user mode (called from the trap handler with interrupts off).
pub fn user_trap(frame: &mut crate::trap::TrapFrame) {
    user_trap_inner(frame);
    crate::chaos::point(crate::chaos::Point::ReturnToUser);
}

#[allow(clippy::large_stack_frames)]
// KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)] // INVARIANT: sepc + 4 steps past an ecall; the process of a trapping thread exists; -1000 - cause is a small negative number (cause < 64)
fn user_trap_inner(frame: &mut crate::trap::TrapFrame) {
    let cause = frame.scause;
    match cause {
        8 => {
            frame.sepc += 4; // past the ecall
            syscall(frame);
        }
        12 | 13 | 15 => {
            let access = match cause {
                12 => Access::Exec,
                13 => Access::Read,
                _ => Access::Write,
            };
            let va = frame.stval;
            let resolved = {
                let pid = state().thread_pid.get(&thread::current()).copied();
                match pid {
                    Some(pid) if va < abi::user::TOP => {
                        let k = kmem::get();
                        let p = state().procs.get_mut(&pid).expect("process exists");
                        let r = p
                            .aspace
                            .handle_fault_mapped(&mut k.mm, &mut k.vmos, va, access);
                        if let Ok(m) = &r {
                            kmem::flush_pages(m.first, m.last, p.asid);
                        }
                        if cfg!(feature = "fault-log") {
                            if let Err(e) = &r {
                                let _ = core::fmt::Write::write_fmt(
                                    &mut crate::console::Console,
                                    format_args!("kernel: handle_fault failed: {e:?} (pid {pid}, va {va:#x})\n"),
                                );
                            }
                        }
                        r.is_ok()
                    }
                    Some(pid) => {
                        if cfg!(feature = "fault-log") {
                            let _ = core::fmt::Write::write_fmt(
                                &mut crate::console::Console,
                                format_args!("kernel: fault above TOP (pid {pid}, va {va:#x})\n"),
                            );
                        }
                        false
                    }
                    None => {
                        if cfg!(feature = "fault-log") {
                            let _ = core::fmt::Write::write_fmt(
                                &mut crate::console::Console,
                                format_args!(
                                    "kernel: fault in thread {} with no process\n",
                                    thread::current()
                                ),
                            );
                        }
                        false
                    }
                }
            };
            if resolved {
                // (the pages the fault mapped were flushed one by one above)
            } else {
                // a diagnostic for headless debugging (off by default: the tests crash programs
                // on purpose and the shell tests read the console line by line)
                if cfg!(feature = "fault-log") {
                    let _ = core::fmt::Write::write_fmt(
                        &mut crate::console::Console,
                        format_args!(
                            "kernel: user fault, cause {cause}, pc {:#x}, address {va:#x}, ra {:#x}, sp {:#x}\n",
                            frame.sepc,
                            frame.arg(1),
                            frame.arg(2)
                        ),
                    );
                }
                exit_process(-1000 - cause as i32);
            }
        }
        2 if crate::fp::first_use(frame) => {
            // The first floating-point instruction of this thread (slice A): the unit is now enabled
            // for it, from zeros; the instruction is retried. (A real illegal instruction traps again
            // with the unit on and ends the process below.)
        }
        _ => exit_process(-1000 - cause as i32),
    }
}

fn status_ret(frame: &mut crate::trap::TrapFrame, s: Status) {
    frame.set_return(s.code() as i64 as u64, None, None);
}

fn handle_list(args: &[u8]) -> Vec<Handle> {
    args.as_chunks::<4>()
        .0
        .iter()
        .map(|c| Handle(u32::from_le_bytes(*c)))
        .collect()
}

#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
fn syscall(frame: &mut crate::trap::TrapFrame) {
    crate::latency::set_syscall(frame.arg(17) as usize);
    let (n, a) = (
        frame.arg(17) as usize,
        [
            frame.arg(10),
            frame.arg(11),
            frame.arg(12),
            frame.arg(13),
            frame.arg(14),
            frame.arg(15),
        ],
    );
    match n {
        nr::THREAD_YIELD => {
            status_ret(frame, Status::Ok);
            thread::yield_now();
        }
        nr::PROCESS_EXIT => exit_process(a[0] as i32),
        nr::THREAD_EXIT => leave(a[0] as i32, false),
        nr::CLOCK_GET => frame.set_return(0, Some(thread::now_ns()), None),
        nr::DEBUG_STAT => frame.set_return(0, Some(thread::wake_stat(a[0])), None),
        nr::OBJECT_WAIT => reply(frame, object_wait(a)),
        nr::PORT_WAIT => {
            // (a poll moves the port's cursor, which counts as a touch)
            let r = port_wait(a);
            commit_wake();
            reply(frame, r);
        }
        nr::FUTEX_WAIT => reply(frame, futex_wait(a)),
        nr::FUTEX_WAKE => {
            let (r, woken) = futex_wake(a);
            // wake exactly the sleepers that were removed from the table, by id (not everyone)
            let tids: Vec<Tid> = woken.into_iter().map(|t| t as Tid).collect();
            thread::wake_many(&tids);
            reply(frame, r);
        }
        nr::PROCESS_CREATE
        | nr::JOB_SET_SCHED
        | nr::THREAD_SET_SCHED
        | nr::THREAD_GET_SCHED
        | nr::JOB_KILL
        | nr::JOB_ROOT
        | nr::RESOURCE_ROOT
        | nr::PROCESS_SELF
        | nr::PROCESS_MAP
        | nr::PROCESS_UNMAP
        | nr::PROCESS_START
        | nr::PROCESS_KILL
        | nr::PROCESS_SUSPEND
        | nr::PROCESS_RESUME
        | nr::PROCESS_INFO
        | nr::THREAD_CREATE
        | nr::THREAD_SPAWN
        | nr::THREAD_INFO
        | nr::THREAD_INTERRUPT => {
            let r = process_call(n, a);
            notify();
            reply(frame, r);
        }
        nr::CHANNEL_CALL => {
            let r = channel_call(a);
            notify();
            reply(frame, r);
        }
        _ => {
            let result = dispatch(n, a);
            // A syscall that can change a signal wakes every waiter (after the borrows of
            // `dispatch` are gone, since waking may switch threads).
            if matches!(
                n,
                nr::CHANNEL_WRITE
                    | nr::CHANNEL_READ
                    | nr::CHANNEL_READ_TXID
                    | nr::HANDLE_CLOSE
                    // a replaced handle's old value names nothing: waits on it end
                    | nr::HANDLE_REPLACE
                    | nr::EVENT_SIGNAL
                    // creating objects can sweep garbage, which closes peers
                    | nr::CHANNEL_CREATE
                    | nr::EVENT_CREATE
                    | nr::SOCKET_CREATE
                    | nr::PORT_CREATE
                    // data moved, space freed, or a binding that is ready at once
                    | nr::SOCKET_WRITE
                    | nr::SOCKET_READ
                    | nr::SOCKET_SHUTDOWN
                    | nr::PORT_BIND
                    | nr::PORT_UNBIND
                    | nr::PORT_REARM
            ) {
                notify();
            }
            // Slice W step 3: wake the threads whose wait this syscall ended, by id (a handle removed
            // by `replace` or sent away is a forced touch, made where it happens)
            commit_wake();
            reply(frame, result);
        }
    }
    if n == nr::THREAD_YIELD {
        // status already set above
    }
}

fn reply(frame: &mut crate::trap::TrapFrame, r: Reply) {
    match r {
        Ok((a1, a2)) => frame.set_return(0, a1, a2),
        Err((s, a1, a2)) => frame.set_return(s.code() as i64 as u64, a1, a2),
    }
}

type ReplyErr = (Status, Option<u64>, Option<u64>);
type Reply = Result<(Option<u64>, Option<u64>), ReplyErr>;

fn fail(s: Status) -> Reply {
    Err((s, None, None))
}

#[allow(clippy::large_stack_frames)]
// KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing
)]
// INVARIANT: a is [u64; 6] and every `h(i)`, `over(i)` is called with a constant below 6; the process of a calling thread exists; sizes were bounded by the limits checked first (nbytes <= MAX_MSG_BYTES, nhandles <= MAX_MSG_HANDLES, so nhandles * 4 and pages * 4096 are small); IRQ slots are below the table size (checked by the source number)
fn dispatch(n: usize, a: [u64; 6]) -> Reply {
    let tid = thread::current();
    let s = state();
    let Some(&pid) = s.thread_pid.get(&tid) else {
        return fail(Status::BadState);
    };
    let k = kmem::get();
    // Sweep before creating kernel objects if garbage cycles have piled up (OD39).
    if matches!(
        n,
        nr::CHANNEL_CREATE
            | nr::EVENT_CREATE
            | nr::SOCKET_CREATE
            | nr::PORT_CREATE
            | nr::INTERRUPT_CREATE
    ) && s.objs.needs_sweep()
    {
        let roots = s.roots();
        s.objs.sweep(&roots);
    }
    let UserState {
        objs,
        procs,
        line,
        pass,
        fail: failed,
        irq_objs,
        ..
    } = s;
    let p = procs.get_mut(&pid).expect("process exists");
    let h = |i: usize| Handle(a[i] as u32);
    // The syscall layer that needs no machine is in the `ksys` crate (TS3): the width check of the 32-bit arguments first, then the calls
    // on handles, events, channels, sockets and ports, over this machine's memory.
    let bad_args = ksys::wide_argument_error(n, &a);
    if let Some(status) = bad_args {
        return fail(status);
    }
    {
        let mut ctx = ksys::Ctx {
            objs: &mut *objs,
            handles: &mut p.handles,
            job: p.job,
            aspace: &mut p.aspace,
            mm: &mut k.mm,
            vmos: &mut k.vmos,
            faults: ksys::Faults {
                skip_forced_touch: cfg!(feature = "fault-skip-forced-touch"),
            },
            harts_online: crate::smp::online() as u64,
        };
        if let Some(r) = ksys::ipc_call(n, a, &mut ctx) {
            return r;
        }
        if let Some(r) = ksys::vmo_call(n, a, &mut ctx) {
            return r;
        }
        if let Some(r) = ksys::job_call(n, a, &mut ctx) {
            return r;
        }
    }
    match n {
        nr::DEBUG_WRITE => {
            let (ptr, len) = (a[0], a[1] as usize);
            if len > MAX_DEBUG_WRITE {
                return fail(Status::TooBig);
            }
            let bytes = copy_from_user(&mut p.aspace, &mut k.mm, &mut k.vmos, ptr, len)
                .map_err(|e| (e, None, None))?;
            emit(line, pass, failed, &bytes);
            Ok((None, None))
        }
        nr::RESOURCE_DTB => {
            ipc::resource_of(objs, &p.handles, h(0)).map_err(|e| (e, None, None))?;
            let Some(blob) =
                crate::dtb::dtb_blob().filter(|b| b.len() <= abi::limits::MAX_DTB_BYTES)
            else {
                return fail(Status::NotSupported);
            };
            let pages = (blob.len() as u64).div_ceil(4096);
            let id = k.vmos.create_in(pages, p.job);
            if k.vmos.write(&mut k.mm, id, 0, blob).is_err() {
                let _ = k.vmos.destroy(&mut k.mm, id);
                return fail(Status::NoMemory);
            }
            let rights =
                Rights(Rights::READ.0 | Rights::MAP.0 | Rights::DUPLICATE.0 | Rights::TRANSFER.0);
            match ipc::vmo_object_create_with(objs, &mut p.handles, id, rights, &mut quota(p.job)) {
                Ok(hd) => Ok((Some(u64::from(hd.0)), Some(blob.len() as u64))),
                Err(e) => {
                    let _ = k.vmos.destroy(&mut k.mm, id);
                    fail(e)
                }
            }
        }
        nr::INTERRUPT_CREATE => {
            ipc::resource_of(objs, &p.handles, h(0)).map_err(|e| (e, None, None))?;
            let irq = a[1];
            if irq == 0 || irq > u64::from(abi::layout::MAX_IRQ) {
                return fail(Status::InvalidArgs);
            }
            if !crate::aia::present() {
                return fail(Status::NotSupported);
            }
            let slot = irq as usize;
            let owned = matches!(*irq_objs.get(slot).unwrap_or(&None), Some(o)
                if matches!(objs.get(o), Some(ipc::KObj::Interrupt(i)) if i.irq == irq as u32));
            if owned {
                return fail(Status::BadState);
            }
            match ipc::interrupt_object_create_in(
                objs,
                &mut p.handles,
                irq as u32,
                &mut quota(p.job),
            ) {
                Ok((hd, obj)) => {
                    irq_objs[slot] = Some(obj);
                    crate::aia::enable(irq as u32);
                    Ok((Some(u64::from(hd.0)), None))
                }
                Err(e) => fail(e),
            }
        }
        nr::INTERRUPT_ACK => {
            let (obj, _) = ipc::interrupt_of(objs, &p.handles, h(0), Rights::MANAGE)
                .map_err(|e| (e, None, None))?;
            let (irq, claims) = ipc::interrupt_ack(objs, obj).map_err(|e| (e, None, None))?;
            crate::aia::reenable(irq);
            Ok((Some(claims), None))
        }
        _ => fail(Status::BadSyscall),
    }
}

/// `##BENCH <name> <decimal value> <unit>`: names and units are single words of letters, digits, `_` and `/`.
fn is_bench_line(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("##BENCH ") else {
        return false;
    };
    let mut w = rest.split(' ');
    let word = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'/')
    };
    match (w.next(), w.next(), w.next(), w.next()) {
        (Some(n), Some(v), Some(u), None) => {
            word(n) && !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) && word(u)
        }
        _ => false,
    }
}

/// Write user output to the console as whole lines. Each `debug_write` call is flushed at
/// its end (a call that does not finish with a newline gets one), and a line longer than
/// 512 bytes is cut, so a program can never leave a half line behind to swallow its next
/// message. A well-formed `##TEST name PASS|FAIL ...` line is a result and is counted;
/// any other line starting `##` is prefixed so user code cannot forge the kernel's
/// verdict lines (`##DONE`, `##PANIC`, `##BOOT`).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: a pair of test counters
fn emit(line: &mut String, pass: &mut u32, fail: &mut u32, bytes: &[u8]) {
    let mut flush = |text: String| {
        // A well-formed `##BENCH name value unit` line (SCHED-SMP-SPEC 6.1, amends OD28) passes through like a result.
        if is_bench_line(&text) {
            let _ = writeln!(Console, "{text}");
            return;
        }
        let verdict = text.strip_prefix("##TEST ").and_then(|rest| {
            let mut words = rest.split_whitespace();
            words.next()?;
            match words.next()? {
                "PASS" => Some(true),
                "FAIL" => Some(false),
                _ => None,
            }
        });
        match verdict {
            Some(true) => {
                *pass = pass.saturating_add(1);
                let _ = writeln!(Console, "{text}");
            }
            Some(false) => {
                *fail = fail.saturating_add(1);
                let _ = writeln!(Console, "{text}");
            }
            None if text.starts_with("##") => {
                // One piece, so the console's line guard sees a line that starts with "user:".
                let _ = Console.write_str(&alloc::format!("user: {text}\n"));
            }
            None => {
                let _ = writeln!(Console, "{text}");
            }
        }
    };
    for &b in bytes {
        if b == b'\n' {
            flush(core::mem::take(line));
        } else {
            // Printable ASCII only: control bytes and anything else become '.'.
            line.push(if (0x20..0x7f).contains(&b) {
                b as char
            } else {
                '.'
            });
            if line.len() >= 512 {
                flush(core::mem::take(line));
            }
        }
    }
    if !line.is_empty() {
        flush(core::mem::take(line));
    }
}

/// A PLIC source fired (trap context, BKL held): hand it to the interrupt object that owns it.
pub fn irq_fired(irq: u32) -> bool {
    let s = state();
    let Some(&Some(obj)) = s.irq_objs.get(irq as usize) else {
        return false;
    };
    ipc::interrupt_fire(&mut s.objs, obj, irq)
}

/// Wake the waiters whose condition the interrupt path changed, without the cleanup `notify` does.
pub fn wake_signal_waiters() {
    commit_wake();
}

/// Wake exactly the threads whose wait the objects touched since the last call can have ended (slice W,
/// W-SPEC section 3): drain the registry's dirty log (and the handles removed without it), let the wait
/// table decide, and wake those threads by id. Called after every syscall and wherever a signal can have
/// changed outside one.
pub fn commit_wake() {
    // (the wakes are made in the same critical section as the commit, and as one batch: every woken thread
    // is runnable before any of them runs, T62)
    irq::without(|| {
        let s = state();
        let UserState { objs, waits, .. } = s;
        let dirty = objs.take_dirty();
        if dirty.items.is_empty() && !dirty.overflow {
            return;
        }
        let woken = waits.commit(
            |id| ipc::signals_of_object(objs, id),
            |id| matches!(objs.get(id), Some(ipc::KObj::Port(_))),
            dirty,
        );
        thread::wake_many(&woken);
    });
}

/// Clean up what the syscall destroyed and wake exactly the threads whose wait it ended (slice W: the
/// global wait queue that used to be woken here, OD43, is gone).
pub fn notify() {
    cleanup_dead();
    reap_vmos();
    commit_wake();
    // the safe point where the kernel heap may grow (heap.rs)
    irq::without(crate::heap::service);
}

/// Do the kernel work that destroying objects calls for: free the pages of a VMO that has
/// no handle left, and kill a process whose last handle was closed. (The registry cannot
/// reach the frame database or the process table, so it hands the dead objects back.)
///
/// Every thread may clean (each drain takes its objects out of the registry atomically and every step
/// below is its own locked section); what a thread must not do is recurse into itself, so a thread
/// already cleaning skips, and its loop sees the work the nested call would have done (T63: the guard used
/// to be one global flag, so while a cleaner was preempted every other hart skipped the cleanup).
#[allow(clippy::indexing_slicing)] // INVARIANT: the irq slot comes from an interrupt object the kernel made, below the table size; the time counter is far below u64::MAX
fn cleanup_dead() {
    let me = thread::current();
    if !irq::without(|| state().cleaning.insert(me)) {
        return; // a cleanup is already running further up this thread's stack; it will see our work
    }
    loop {
        let dead = irq::without(|| state().objs.drain_dead());
        if dead.is_empty() {
            irq::without(|| state().cleaning.remove(&me));
            return;
        }
        for obj in dead {
            match obj {
                ipc::KObj::Vmo(id) => {
                    irq::without(|| state().pending_vmos.push(id));
                    REAP_DIRTY.store(true, Ordering::SeqCst);
                }
                ipc::KObj::Process(p) if p.kill_on_close && !p.terminated => {
                    request_kill(p.pid, -2000)
                }
                ipc::KObj::Job(id) => irq::without(|| kmem::get().vmos.jobs.release(id)),
                ipc::KObj::Interrupt(i) => irq::without(|| {
                    // mask the source unless a newer object already owns it
                    let s = state();
                    let owned = matches!(s.irq_objs[i.irq as usize], Some(o)
                        if matches!(s.objs.get(o), Some(ipc::KObj::Interrupt(n)) if n.irq == i.irq));
                    if !owned {
                        s.irq_objs[i.irq as usize] = None;
                        crate::aia::disable(i.irq);
                    }
                }),
                _ => {}
            }
        }
        // One scan for everything that died in this round (not one per VMO).
        reap_vmos();
    }
}

/// Set whenever something happened that could let a pending VMO go: one was queued, a mapping
/// was removed, or a process was torn down. `reap_vmos` runs only then: it scans every region of
/// every process, and every VMO a process closes after mapping it stays pending until the
/// process dies, so scanning on every wake-up cost about a millisecond per syscall once a
/// dozen service processes were alive.
static REAP_DIRTY: AtomicBool = AtomicBool::new(false);

/// Free the VMOs whose last handle is gone and that no address space maps any more.
fn reap_vmos() {
    if !REAP_DIRTY.swap(false, Ordering::SeqCst) {
        return;
    }
    irq::without(|| {
        let s = state();
        let k = kmem::get();
        if s.pending_vmos.is_empty() {
            return;
        }
        // One pass over all regions, not one per pending VMO.
        let mut mapped: alloc::collections::BTreeSet<u64> = alloc::collections::BTreeSet::new();
        for p in s.procs.values() {
            p.aspace.collect_vmos(&mut mapped);
        }
        let pending = core::mem::take(&mut s.pending_vmos);
        for id in pending {
            if mapped.contains(&id) {
                s.pending_vmos.push(id);
            } else {
                let _ = k.vmos.destroy(&mut k.mm, id);
            }
        }
    });
}

/// End process `pid` with `code`, from any other thread: a process with no thread yet is
/// torn down at once; a running one is told to die and woken (its thread exits at its next
/// return to user mode).
/// How many harts are in (or just entering) user mode now, and the most there have been at once.
static USER_NOW: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static USER_MAX: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// A hart is about to return to user mode.
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: USER_NOW counts events, far below u64::MAX
pub fn user_mode_entered() {
    crate::latency::interrupts_on(); // (the return to user mode turns interrupts on: the span ends)
    crate::percpu::this().in_user.store(true, Ordering::SeqCst);
    let now = USER_NOW.fetch_add(1, Ordering::SeqCst) + 1;
    USER_MAX.fetch_max(now, Ordering::SeqCst);
}

/// A hart took a trap from user mode.
pub fn user_mode_left() {
    crate::percpu::this().in_user.store(false, Ordering::SeqCst);
    USER_NOW.fetch_sub(1, Ordering::SeqCst);
}

/// The most harts that were in user mode at the same moment since the last reset.
pub fn max_harts_in_user_mode() -> usize {
    USER_MAX.load(Ordering::SeqCst)
}

pub fn reset_user_mode_high_water() {
    USER_MAX.store(USER_NOW.load(Ordering::SeqCst), Ordering::SeqCst);
}

/// The `time` counter when a whole process last began to end (the kill-latency test).
pub static LAST_EXIT_CYCLES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// End process `pid` with `code` from kernel code (the tests).
pub fn end_process(pid: u32, code: i32) {
    request_kill(pid, code);
}

/// Wake every blocked thread of process `pid` by id (a kill must reach sleepers that are on no
/// queue: futex sleepers).
fn wake_threads_of(pid: u32) {
    let tids: Vec<Tid> = irq::without(|| {
        state()
            .thread_pid
            .iter()
            .filter(|(_, p)| **p == pid)
            .map(|(t, _)| *t)
            .collect()
    });
    thread::wake_many(&tids);
}

fn request_kill(pid: u32, code: i32) {
    let direct = irq::without(|| {
        let s = state();
        let r = match s.procs.get_mut(&pid) {
            Some(p) if p.state == ProcState::Created => {
                p.exit_code = Some(code);
                true
            }
            Some(p) => {
                p.killed.get_or_insert(code);
                bump_attention();
                false
            }
            None => false,
        };
        // A victim running user code on another hart sees the kill at its next trap: the flag is
        // set first and the kick comes in the same locked region, so a hart that handles the IPI
        // always finds the flag (the next timer tick would do it within a millisecond; this makes
        // it prompt).
        if !cfg!(feature = "fault-skip-kill-ipi") {
            crate::smp::kick_others();
        }
        r
    });
    if direct {
        teardown(pid);
    } else {
        wake_threads_of(pid);
    }
}

/// Run `f` on the calling thread's process, with interrupts off. `Err(BadState)` if the
/// caller is not a user thread.
fn with_proc<R>(
    f: impl FnOnce(&mut Kobjs, &mut Process, &mut kmem::Kmem) -> R,
) -> Result<R, Status> {
    irq::without(|| {
        let s = state();
        let pid = *s
            .thread_pid
            .get(&thread::current())
            .ok_or(Status::BadState)?;
        let k = kmem::get();
        let UserState { objs, procs, .. } = s;
        let p = procs.get_mut(&pid).ok_or(Status::BadState)?;
        Ok(f(objs, p, k))
    })
}

/// The signal word of `h` in the calling process (0 if it names nothing). Interrupts off.
fn signals_now(h: Handle) -> Result<u32, Status> {
    let s = state();
    let pid = s
        .thread_pid
        .get(&thread::current())
        .ok_or(Status::BadState)?;
    let p = s.procs.get(pid).ok_or(Status::BadState)?;
    ipc::signals_of(&s.objs, &p.handles, h)
}

const WAITABLE_SIGNALS: u32 = abi::signals::WAITABLE;

/// Register `thread` as waiting for `mask` on the object the calling process's handle `h` names (just
/// before it sleeps; interrupts off, lock held). A handle that names nothing has no registration (its
/// wait ended already: the predicate was true).
fn register_wait(thread: Tid, h: Handle, mask: u32) {
    let s = state();
    let Some(pid) = s.thread_pid.get(&thread).copied() else {
        return;
    };
    let Some(p) = s.procs.get(&pid) else {
        return;
    };
    if let Ok(entry) = p.handles.get(h) {
        s.waits.register(thread, entry.obj, mask);
    }
}

#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
fn object_wait(a: [u64; 6]) -> Reply {
    if a[0] > u64::from(u32::MAX) {
        return fail(Status::BadHandle);
    }
    let (h, deadline) = (Handle(a[0] as u32), a[2]);
    let rights = with_proc(|_, p, _| p.handles.get(h))
        .map_err(|e| (e, None, None))?
        .map_err(|e| (e, None, None))?
        .rights;
    if !rights.contains(Rights::WAIT) {
        return fail(Status::AccessDenied);
    }
    if a[1] > u64::from(u32::MAX) || a[1] == 0 || (a[1] as u32) & !WAITABLE_SIGNALS != 0 {
        return fail(Status::InvalidArgs);
    }
    let mask = a[1] as u32;
    let observed = || irq::without(|| signals_now(h));
    if deadline != 0 {
        // A handle that stops naming anything ends the wait (the check below then reports it).
        let me = thread::current();
        // (anything a kernel thread touched without committing goes first: a thread must not sleep
        // while a change it should have seen is still in the log)
        commit_wake();
        thread::wait_deadline_registered(
            || {
                killed_now().is_some()
                    || interrupted_now()
                    || signals_now(h).map_or(true, |s| s & mask != 0)
            },
            || register_wait(me, h, mask),
            deadline,
        );
        irq::without(|| state().waits.forget(me));
    }
    // The handle may have gone while we slept: BAD_HANDLE, not a wait that quietly times out.
    let seen = observed().map_err(|e| (e, None, None))?;
    if seen & mask != 0 {
        Ok((Some(u64::from(seen)), None))
    } else if deadline != 0 && take_interrupt() {
        Err((Status::Interrupted, Some(u64::from(seen)), None))
    } else {
        Err((Status::TimedOut, Some(u64::from(seen)), None))
    }
}

/// `futex_wait(addr, expected, deadline)` (S7b, SYSCALLS.md): sleep on the 32-bit word at `addr`
/// if it still equals `expected`. The check and the enqueue are one step under the big kernel lock,
/// so a wake that follows a user-mode store is never lost.
fn futex_wait(a: [u64; 6]) -> Reply {
    let (addr, expected, deadline) = (a[0], a[1], a[2]);
    if expected > u64::from(u32::MAX) {
        return fail(Status::InvalidArgs);
    }
    let tid = thread::current();
    let who = tid as u64;
    // check and enqueue, in one step
    let first = irq::without(|| -> Result<Result<(), Status>, Status> {
        let s = state();
        let Some(&pid) = s.thread_pid.get(&tid) else {
            return Err(Status::BadState);
        };
        let k = kmem::get();
        let UserState { procs, futexes, .. } = s;
        let p = procs.get_mut(&pid).ok_or(Status::BadState)?;
        let word = kcore::usercopy::read_user_u32(&mut p.aspace, &mut k.mm, &mut k.vmos, addr)?;
        if u64::from(word) != expected {
            return Ok(Err(Status::ShouldWait)); // the word changed: do not sleep
        }
        if deadline == 0 {
            return Ok(Err(Status::TimedOut)); // a poll: equal, and no time to wait
        }
        let fresh = futexes.wait((pid, addr), who);
        debug_assert!(fresh, "a thread sleeps on one futex at a time");
        Ok(Ok(()))
    })
    .map_err(|e| (e, None, None))?;
    if let Err(status) = first {
        return fail(status);
    }
    // sleep until woken (removed from the table), killed, or the deadline: not on the global queue,
    // only `futex_wake` and a kill wake this thread (by id)
    thread::wait_deadline_private(
        || killed_now().is_some() || interrupted_now() || !state().futexes.contains(who),
        deadline,
    );
    // still in the table: the deadline (or a kill) ended the wait: leave it
    let timed_out = irq::without(|| state().futexes.forget(who));
    if timed_out && take_interrupt() {
        fail(Status::Interrupted)
    } else if timed_out {
        fail(Status::TimedOut)
    } else {
        Ok((None, None))
    }
}

/// `futex_wake(addr, count)`: wake up to `count` sleepers on `addr` in this process, oldest first.
fn futex_wake(a: [u64; 6]) -> (Reply, Vec<u64>) {
    let (addr, count) = (a[0], a[1]);
    if addr & 3 != 0 || addr < abi::user::BASE || addr.saturating_add(4) > abi::user::TOP {
        return (fail(Status::InvalidArgs), Vec::new());
    }
    let tid = thread::current();
    let woken = irq::without(|| {
        let s = state();
        let pid = *s.thread_pid.get(&tid)?;
        Some(s.futexes.wake((pid, addr), count))
    });
    match woken {
        Some(ids) => (Ok((Some(ids.len() as u64), None)), ids),
        None => (Err((Status::BadState, None, None)), Vec::new()),
    }
}

/// `port_wait(port, deadline)`: wait until a binding is ready (the port's READABLE signal),
/// then report one in turn.
fn port_wait(a: [u64; 6]) -> Reply {
    if a[0] > u64::from(u32::MAX) {
        return fail(Status::BadHandle);
    }
    let (h, deadline) = (Handle(a[0] as u32), a[1]);
    let poll = || {
        with_proc(|objs, p, k| ipc::port_poll_in(objs, &p.handles, h, &mut k.vmos.jobs))
            .map_err(|e| (e, None, None))?
            .map_err(|e| (e, None, None))
    };
    // Poll, and while nothing is ready and the deadline has not passed, sleep until the port
    // is READABLE and poll again (the readiness seen by the sleep check can be gone by the time
    // we poll, so one wait is not enough).
    let mut got = poll()?;
    while got.is_none()
        && deadline != 0
        && thread::now_ns() < deadline
        && irq::without(killed_now).is_none()
        && !irq::without(interrupted_now)
    {
        // (the poll above may have touched the port; tell the wait table before sleeping, so that no
        // thread goes to sleep, and the lock is released, with signal changes still in the log)
        commit_wake();
        // Ready, killed, or the handle stopped naming a port (the poll then says so). The thread is
        // registered on the port: the wait table re-evaluates a port's signal (which follows its
        // bindings) whenever anything was touched.
        let me = thread::current();
        thread::wait_deadline_registered(
            || {
                killed_now().is_some()
                    || interrupted_now()
                    || signals_now(h).map_or(true, |s| s & abi::signals::READABLE != 0)
            },
            || register_wait(me, h, abi::signals::READABLE),
            deadline,
        );
        irq::without(|| state().waits.forget(me));
        got = poll()?;
    }
    match got {
        Some((key, observed)) => Ok((Some(key), Some(u64::from(observed)))),
        None if deadline != 0 && take_interrupt() => fail(Status::Interrupted),
        None => fail(Status::TimedOut),
    }
}

#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: every caller passes a constant offset whose four or eight bytes lie inside the fixed-size record it just copied in (the longest, offset 48, is in a record of 56 bytes)
fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: every caller passes a constant offset whose four or eight bytes lie inside the fixed-size record it just copied in (the longest, offset 48, is in a record of 56 bytes)
fn le64(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

#[allow(clippy::large_stack_frames)]
// KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
#[allow(clippy::arithmetic_side_effects)]
// INVARIANT: nhandles <= MAX_MSG_HANDLES was checked first, so nhandles * 4 is small; the record read is of fixed size
fn channel_call(a: [u64; 6]) -> Reply {
    if a[0] > u64::from(u32::MAX) {
        return fail(Status::BadHandle);
    }
    let ch = Handle(a[0] as u32);
    let rec = with_proc(|_, p, k| {
        copy_from_user(
            &mut p.aspace,
            &mut k.mm,
            &mut k.vmos,
            a[1],
            abi::CHANNEL_CALL_ARGS_BYTES,
        )
    })
    .map_err(|e| (e, None, None))?
    .map_err(|e| (e, None, None))?;
    let (req_bytes, req_handles) = (le64(&rec, 0), le64(&rec, 8));
    let (nbytes, nhandles) = (le32(&rec, 16) as usize, le32(&rec, 20) as usize);
    let (rd_bytes, rd_handles) = (le64(&rec, 24), le64(&rec, 32));
    let (rd_max_bytes, rd_max_handles) = (
        (le32(&rec, 40) as usize).min(MAX_MSG_BYTES),
        (le32(&rec, 44) as usize).min(MAX_MSG_HANDLES),
    );
    let deadline = le64(&rec, 48);
    if nbytes > MAX_MSG_BYTES || nhandles > MAX_MSG_HANDLES {
        return fail(Status::TooBig);
    }
    if nbytes < 4 {
        return fail(Status::InvalidArgs);
    }
    // An interrupt already pending ends the call before anything is sent (a retry then sends one request, not two).
    if deadline != 0 && take_interrupt() {
        return fail(Status::Interrupted);
    }
    // Send the request.
    let txid = with_proc(|objs, p, k| -> Result<u32, Status> {
        let entry = p.handles.get(ch)?;
        if !(entry.rights.contains(Rights::READ) && entry.rights.contains(Rights::WRITE)) {
            // (a non-channel handle still reports WRONG_TYPE first, below, via channel_write)
            if !matches!(objs.get(entry.obj), Some(ipc::KObj::Channel(_))) {
                return Err(Status::WrongType);
            }
            return Err(Status::AccessDenied);
        }
        let data = copy_from_user(&mut p.aspace, &mut k.mm, &mut k.vmos, req_bytes, nbytes)?;
        let raw = copy_from_user(
            &mut p.aspace,
            &mut k.mm,
            &mut k.vmos,
            req_handles,
            nhandles * 4,
        )?;
        ipc::channel_write_in(
            objs,
            &mut p.handles,
            ch,
            &data,
            &handle_list(&raw),
            &mut kmem::get().vmos.jobs,
        )?;
        Ok(le32(&data, 0))
    })
    .map_err(|e| (e, None, None))?
    .map_err(|e| (e, None, None))?;
    notify();
    // Wait for the reply with that transaction id (or the peer closing, or the deadline).
    let me = thread::current();
    let ready = || {
        let s = state();
        let Some(&pid) = s.thread_pid.get(&thread::current()) else {
            return true;
        };
        let Some(p) = s.procs.get(&pid) else {
            return true;
        };
        if p.killed.is_some() || s.interrupts.contains(&thread::current()) {
            return true;
        }
        !matches!(
            ipc::channel_peek(
                &s.objs,
                &p.handles,
                ch,
                Some(txid),
                rd_max_bytes,
                rd_max_handles
            ),
            Err(ReadError::Status(Status::ShouldWait))
        )
    };
    commit_wake();
    thread::wait_deadline_registered(
        ready,
        || register_wait(me, ch, abi::signals::READABLE | abi::signals::PEER_CLOSED),
        deadline,
    );
    irq::without(|| state().waits.forget(me));
    let out = with_proc(|objs, p, k| -> Reply {
        match ipc::channel_peek(
            objs,
            &p.handles,
            ch,
            Some(txid),
            rd_max_bytes,
            rd_max_handles,
        ) {
            Ok((nb, nh)) => {
                verify_writable(&mut p.aspace, &mut k.mm, &mut k.vmos, rd_bytes, nb)
                    .map_err(|e| (e, None, None))?;
                verify_writable(&mut p.aspace, &mut k.mm, &mut k.vmos, rd_handles, nh * 4)
                    .map_err(|e| (e, None, None))?;
                let got = ipc::channel_read_txid_in(
                    objs,
                    &mut p.handles,
                    ch,
                    txid,
                    rd_max_bytes,
                    rd_max_handles,
                    &mut k.vmos.jobs,
                )
                .map_err(|_| (Status::BadState, None, None))?;
                let raw: Vec<u8> = got.handles.iter().flat_map(|h| h.0.to_le_bytes()).collect();
                copy_to_user_verified(&mut p.aspace, &mut k.mm, rd_bytes, &got.data)
                    .map_err(|e| (e, None, None))?;
                copy_to_user_verified(&mut p.aspace, &mut k.mm, rd_handles, &raw)
                    .map_err(|e| (e, None, None))?;
                Ok((Some(got.data.len() as u64), Some(got.handles.len() as u64)))
            }
            Err(ReadError::TooSmall { bytes, handles }) => Err((
                Status::BufferTooSmall,
                Some(bytes as u64),
                Some(handles as u64),
            )),
            Err(ReadError::Status(Status::ShouldWait)) => Err((Status::TimedOut, None, None)),
            Err(ReadError::Status(st)) => Err((st, None, None)),
        }
    })
    .map_err(|e| (e, None, None))?;
    // (a reply that came wins; an interrupt that ended the wait reports INTERRUPTED, the request stays queued)
    match out {
        Err((Status::TimedOut, ..)) if deadline != 0 && take_interrupt() => {
            fail(Status::Interrupted)
        }
        other => other,
    }
}

/// The creator's end of a process's bootstrap channel, held by the kernel (S3c-1). Kernel
/// code (a test helper today, the launcher later) talks to the new process through it.
pub struct KernelEnd {
    handle: Handle,
}

impl Drop for KernelEnd {
    /// Dropping an end closes it, so a forgotten end cannot leak a kernel-table slot.
    fn drop(&mut self) {
        irq::without(|| {
            let s = state();
            let _ = s.ktable.close(&mut s.objs, self.handle);
        });
        notify();
    }
}

/// Take the kernel's end of `pid`'s bootstrap channel (once). Whoever takes it must `close` it.
pub fn take_bootstrap(pid: u32) -> Option<KernelEnd> {
    irq::without(|| {
        state()
            .procs
            .get_mut(&pid)?
            .kernel_end
            .take()
            .map(|handle| KernelEnd { handle })
    })
}

impl KernelEnd {
    pub fn write(&self, data: &[u8]) -> Result<(), Status> {
        let r = irq::without(|| {
            let s = state();
            ipc::channel_write_in(
                &mut s.objs,
                &mut s.ktable,
                self.handle,
                data,
                &[],
                &mut kmem::get().vmos.jobs,
            )
        });
        notify();
        r
    }

    /// Take the oldest message (handles in it are closed: the kernel does not keep them).
    pub fn read(&self) -> Result<Vec<u8>, Status> {
        let r = irq::without(|| {
            let s = state();
            let got = ipc::channel_read_in(
                &mut s.objs,
                &mut s.ktable,
                self.handle,
                MAX_MSG_BYTES,
                MAX_MSG_HANDLES,
                &mut kmem::get().vmos.jobs,
            );
            match got {
                Ok(msg) => {
                    for h in msg.handles {
                        let _ = s.ktable.close(&mut s.objs, h);
                    }
                    Ok(msg.data)
                }
                Err(ReadError::Status(st)) => Err(st),
                Err(ReadError::TooSmall { .. }) => Err(Status::BadState),
            }
        });
        notify();
        r
    }

    /// Send `data` together with a new VMO holding `image` (the receiver gets a VMO handle
    /// with the default VMO rights). Used to hand a program image to a launcher.
    pub fn write_with_vmo(&self, data: &[u8], image: &[u8]) -> Result<(), Status> {
        let r = irq::without(|| {
            let s = state();
            let k = kmem::get();
            let pages = (image.len() as u64).div_ceil(4096).max(1);
            let id = k.vmos.create(pages);
            if k.vmos.write(&mut k.mm, id, 0, image).is_err() {
                let _ = k.vmos.destroy(&mut k.mm, id);
                return Err(Status::NoMemory);
            }
            let h = match ipc::vmo_object_create_in(
                &mut s.objs,
                &mut s.ktable,
                id,
                &mut quota(JobId::ROOT),
            ) {
                Ok(h) => h,
                Err(e) => {
                    let _ = k.vmos.destroy(&mut k.mm, id);
                    return Err(e);
                }
            };
            let r = ipc::channel_write_in(
                &mut s.objs,
                &mut s.ktable,
                self.handle,
                data,
                &[h],
                &mut kmem::get().vmos.jobs,
            );
            if r.is_err() {
                let _ = s.ktable.close(&mut s.objs, h);
            }
            r
        });
        notify();
        r
    }

    /// Send `data` with handles that are in the kernel's table (`ktable_*` helpers make them);
    /// they move into the message. Nothing moves if the write fails.
    pub fn write_handles(&self, data: &[u8], handles: &[Handle]) -> Result<(), Status> {
        let r = irq::without(|| {
            let s = state();
            ipc::channel_write_in(
                &mut s.objs,
                &mut s.ktable,
                self.handle,
                data,
                handles,
                &mut kmem::get().vmos.jobs,
            )
        });
        notify();
        r
    }

    /// Wait until a message is readable or the peer has closed; false on timeout.
    pub fn wait_readable(&self, deadline: u64) -> bool {
        let h = self.handle;
        let me = thread::current();
        commit_wake();
        let r = thread::wait_deadline_registered(
            || {
                let s = state();
                ipc::signals_of(&s.objs, &s.ktable, h)
                    .map(|sig| sig & (abi::signals::READABLE | abi::signals::PEER_CLOSED) != 0)
                    .unwrap_or(true)
            },
            || {
                let s = state();
                if let Ok(entry) = s.ktable.get(h) {
                    s.waits.register(
                        me,
                        entry.obj,
                        abi::signals::READABLE | abi::signals::PEER_CLOSED,
                    );
                }
            },
            deadline,
        );
        irq::without(|| state().waits.forget(me));
        r
    }

    /// Close the end (the same as dropping it).
    pub fn close(self) {
        drop(self);
    }
}

/// The kernel's end of a socket (in the kernel's own handle table), for the serial bridge.
#[cfg_attr(not(feature = "interactive"), allow(dead_code))]
pub struct KernelSocket {
    handle: Handle,
}

impl Drop for KernelSocket {
    fn drop(&mut self) {
        irq::without(|| {
            let s = state();
            let _ = s.ktable.close(&mut s.objs, self.handle);
        });
        notify();
    }
}

/// A new socket pair: the kernel keeps one end; the other end's handle is in the kernel's
/// table, ready to be sent to a process with [`KernelEnd::write_handles`].
#[cfg_attr(not(feature = "interactive"), allow(dead_code))]
pub fn kernel_socket_pair() -> Result<(KernelSocket, Handle), Status> {
    irq::without(|| {
        let s = state();
        let (kernel, user) =
            ipc::socket_create_in(&mut s.objs, &mut s.ktable, &mut quota(JobId::ROOT))?;
        Ok((KernelSocket { handle: kernel }, user))
    })
}

/// A read-only VMO (READ, MAP, DUPLICATE, TRANSFER) holding `bytes`, as a handle in the
/// kernel's table.
/// Close a handle in the kernel's own table (cleanup when a send did not happen).
pub fn ktable_close(h: Handle) {
    irq::without(|| {
        let s = state();
        let _ = s.ktable.close(&mut s.objs, h);
    });
    notify();
}

pub fn ktable_readonly_vmo(bytes: &[u8]) -> Result<Handle, Status> {
    irq::without(|| {
        let s = state();
        let k = kmem::get();
        let pages = (bytes.len() as u64).div_ceil(4096).max(1);
        let id = k.vmos.create(pages);
        if k.vmos.write(&mut k.mm, id, 0, bytes).is_err() {
            let _ = k.vmos.destroy(&mut k.mm, id);
            return Err(Status::NoMemory);
        }
        let h = match ipc::vmo_object_create_in(
            &mut s.objs,
            &mut s.ktable,
            id,
            &mut quota(JobId::ROOT),
        ) {
            Ok(h) => h,
            Err(e) => {
                let _ = k.vmos.destroy(&mut k.mm, id);
                return Err(e);
            }
        };
        s.ktable
            .replace(
                &mut s.objs,
                h,
                Rights(Rights::READ.0 | Rights::MAP.0 | Rights::DUPLICATE.0 | Rights::TRANSFER.0),
            )
            .inspect_err(|_| {
                let _ = s.ktable.close(&mut s.objs, h);
            })
    })
}

#[cfg_attr(not(feature = "interactive"), allow(dead_code))]
impl KernelSocket {
    /// Up to `max` bytes the process wrote to its end.
    pub fn read(&self, max: usize) -> Result<Vec<u8>, Status> {
        let r = irq::without(|| {
            let s = state();
            ipc::socket_read_in(
                &mut s.objs,
                &s.ktable,
                self.handle,
                max,
                &mut kmem::get().vmos.jobs,
            )
        });
        notify();
        r
    }

    /// Bytes for the process to read; returns how many were accepted.
    pub fn write(&self, data: &[u8]) -> Result<usize, Status> {
        let r = irq::without(|| {
            let s = state();
            ipc::socket_write_in(
                &mut s.objs,
                &s.ktable,
                self.handle,
                data,
                &mut kmem::get().vmos.jobs,
            )
        });
        notify();
        r
    }

    /// The socket's signals (READABLE, PEER_CLOSED, ...), 0 if it names nothing.
    pub fn signals(&self) -> u32 {
        irq::without(|| {
            let s = state();
            ipc::signals_of(&s.objs, &s.ktable, self.handle).unwrap_or(0)
        })
    }
}

// ---------------------------------------------------------------------------
// Process syscalls (S3c-2a). They touch two processes (the caller and a target), so they
// work in short interrupts-off steps that never hold both at once.

fn bad(s: Status) -> Reply {
    Err((s, None, None))
}

#[allow(clippy::indexing_slicing)] // INVARIANT: a is [u64; 6] and `wide(i)` is called with constants below 6
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
fn process_call(n: usize, a: [u64; 6]) -> Reply {
    // Handles are 32 bits wide everywhere (SYSCALLS.md); no truncation.
    let wide = |i: usize| a[i] > u64::from(u32::MAX);
    match n {
        nr::PROCESS_CREATE if wide(0) => bad(Status::BadHandle),
        nr::PROCESS_CREATE => process_create(a),
        nr::JOB_KILL | nr::JOB_SET_SCHED if wide(0) => bad(Status::BadHandle),
        nr::JOB_SET_SCHED => job_set_sched(a),
        nr::THREAD_SET_SCHED | nr::THREAD_GET_SCHED if wide(0) => bad(Status::BadHandle),
        nr::THREAD_SET_SCHED => thread_set_sched(a),
        nr::THREAD_GET_SCHED => thread_get_sched(a),
        nr::JOB_KILL => job_kill(a),
        nr::JOB_ROOT | nr::RESOURCE_ROOT | nr::PROCESS_SELF => process_table_call(n, a),
        nr::PROCESS_MAP if wide(0) || wide(1) => bad(Status::BadHandle),
        nr::PROCESS_MAP => process_table_call(n, a),
        nr::PROCESS_UNMAP if wide(0) => bad(Status::BadHandle),
        nr::PROCESS_UNMAP => process_table_call(n, a).inspect(|_| {
            REAP_DIRTY.store(true, Ordering::SeqCst);
            reap_vmos();
        }),
        nr::PROCESS_START if wide(0) || wide(3) => bad(Status::BadHandle),
        nr::PROCESS_START => process_start(a),
        nr::PROCESS_KILL | nr::PROCESS_INFO if wide(0) => bad(Status::BadHandle),
        nr::PROCESS_KILL => process_kill(a),
        nr::PROCESS_SUSPEND | nr::PROCESS_RESUME if wide(0) => bad(Status::BadHandle),
        nr::PROCESS_SUSPEND => process_stop(a, true),
        nr::PROCESS_RESUME => process_stop(a, false),
        nr::PROCESS_INFO => process_table_call(n, a),
        nr::THREAD_CREATE if wide(0) => bad(Status::BadHandle),
        nr::THREAD_CREATE => thread_create(a),
        nr::THREAD_SPAWN if wide(0) => bad(Status::BadHandle),
        nr::THREAD_SPAWN => thread_spawn(a),
        nr::THREAD_INFO if wide(0) => bad(Status::BadHandle),
        nr::THREAD_INFO => process_table_call(n, a),
        nr::THREAD_INTERRUPT if wide(0) => bad(Status::BadHandle),
        nr::THREAD_INTERRUPT => thread_interrupt(a),
        _ => bad(Status::BadSyscall),
    }
}

/// What the process calls of `ksys` need of this machine.
struct KernelMachine;

impl ksys::Machine for KernelMachine {
    fn shootdown(&mut self) {
        crate::smp::shootdown();
    }
    fn flush_tlb(&mut self) {
        kmem::flush_tlb();
    }
}

/// The calls that only read and change the process table (`ksys::proc_call`).
fn process_table_call(n: usize, a: [u64; 6]) -> Reply {
    irq::without(|| -> Reply {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let k = kmem::get();
        let mut ctx = ksys::PCtx {
            objs: &mut s.objs,
            procs: &mut s.procs,
            me,
            mm: &mut k.mm,
            vmos: &mut k.vmos,
            machine: &mut KernelMachine,
        };
        ksys::proc_call(n, a, &mut ctx).unwrap_or_else(|| bad(Status::BadSyscall))
    })
}

fn caller_pid(s: &UserState) -> Result<u32, Status> {
    s.thread_pid
        .get(&thread::current())
        .copied()
        .ok_or(Status::BadState)
}

#[allow(clippy::large_stack_frames)]
// KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
#[allow(clippy::arithmetic_side_effects)]
// INVARIANT: pids are a counter far below u32::MAX; the new process's handle was made with room checked
fn process_create(a: [u64; 6]) -> Reply {
    irq::without(|| -> Reply {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        if s.procs.len() >= abi::limits::MAX_PROCESSES || !kmem::kernel_half_unchanged() {
            return bad(Status::NoMemory);
        }
        // The job the new process is charged to: the one named by the handle in a0 (MANAGE), or
        // the caller's own when a0 is 0.
        let creator = s.procs.get(&me).ok_or((Status::BadState, None, None))?;
        let job = if a[0] == 0 {
            creator.job
        } else {
            ipc::job_of(
                &s.objs,
                &creator.handles,
                Handle(a[0] as u32),
                Rights::MANAGE,
            )
            .map_err(|e| (e, None, None))?
        };
        let k = kmem::get();
        k.vmos
            .jobs
            .charge(job, Resource::Processes, 1)
            .map_err(|e| (job_status(e), None, None))?;
        // the process's root page table is charged to its job too (one page)
        if let Err(e) = k.vmos.jobs.charge(job, Resource::Pages, 1) {
            k.vmos.jobs.credit(job, Resource::Processes, 1);
            return bad(job_status(e));
        }
        k.vmos.jobs.retain(job);
        let give_back = |k: &mut kmem::Kmem| {
            k.vmos.jobs.credit(job, Resource::Pages, 1);
            k.vmos.jobs.credit(job, Resource::Processes, 1);
            k.vmos.jobs.release(job);
        };
        let Some(table) = PageTable::new_with_kernel_half(&mut k.mm, k.aspace.table()) else {
            give_back(k);
            return bad(Status::NoMemory);
        };
        // (pids wrap after 2^32 creations and skip the live ones: a loop of create and close must not end in an overflow)
        let pid = loop {
            let candidate = s.next_pid;
            s.next_pid = if candidate == u32::MAX {
                1
            } else {
                candidate + 1
            };
            if !s.procs.contains_key(&candidate) {
                break candidate;
            }
        };
        let UserState { objs, procs, .. } = s;
        let creator = procs.get_mut(&me).ok_or((Status::BadState, None, None))?;
        let (handle, obj) = match ipc::process_object_create_in(
            objs,
            &mut creator.handles,
            pid,
            true,
            &mut quota(job),
        ) {
            Ok(x) => x,
            Err(e) => {
                PageTable::destroy_user(table, &mut k.mm);
                give_back(k);
                return bad(e);
            }
        };
        procs.insert(
            pid,
            Process {
                aspace: AddressSpace::new(table),
                handles: HandleTable::new(MAX_HANDLES),
                asid: (pid & 0xffff) as u16,
                vmos: Vec::new(),
                entry: 0,
                arg: 0,
                exit_code: None,
                kernel_end: None,
                start_sp: 0,
                obj: Some(obj),
                state: ProcState::Created,
                detached: true,
                killed: None,
                stopped: false,
                threads_alive: 0,
                thread_objs: BTreeMap::new(),
                job,
                table_charge: BTreeMap::from([(u64::MAX, 1)]),
                stacks: BTreeMap::new(),
                stack_hint: abi::user::STACK_ARENA_BASE,
                privileged: false,
            },
        );
        Ok((Some(u64::from(handle.0)), None))
    })
}

#[allow(clippy::large_stack_frames)]
// KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
#[allow(clippy::expect_used)]
// INVARIANT: the caller and the target were looked up under the same lock a few lines above; the first thread's record exists
fn process_start(a: [u64; 6]) -> Reply {
    let (ph, bh) = (Handle(a[0] as u32), Handle(a[3] as u32));
    let (entry, sp, arg) = (a[1], a[2], a[4]);
    let (started, first_prio) = irq::without(|| -> Result<(usize, u8), ReplyErr> {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let caller = s.procs.get(&me).ok_or((Status::BadState, None, None))?;
        let pe = caller.handles.get(ph).map_err(|e| (e, None, None))?;
        let be = caller.handles.get(bh).map_err(|e| (e, None, None))?;
        let tpid = match s.objs.get(pe.obj) {
            Some(ipc::KObj::Process(p)) => p.pid,
            _ => return bad(Status::WrongType).map(|_| (0, 0)),
        };
        if !matches!(s.objs.get(be.obj), Some(ipc::KObj::Channel(_))) {
            return bad(Status::WrongType).map(|_| (0, 0));
        }
        if !pe.rights.contains(Rights::MANAGE) || !be.rights.contains(Rights::TRANSFER) {
            return bad(Status::AccessDenied).map(|_| (0, 0));
        }
        let target = s.procs.get(&tpid).ok_or((Status::BadState, None, None))?;
        if target.state != ProcState::Created
            || target.exit_code.is_some()
            || target.killed.is_some()
        {
            return bad(Status::BadState).map(|_| (0, 0));
        }
        if total_threads(s) >= abi::limits::MAX_USER_THREADS {
            return bad(Status::NoMemory).map(|_| (0, 0));
        }
        let entry_ok = target.aspace.region_perms_at(entry).is_some_and(|p| p.exec);
        let sp_ok = sp.is_multiple_of(16) && (abi::user::BASE..=abi::user::TOP).contains(&sp);
        if !entry_ok || !sp_ok {
            return bad(Status::InvalidArgs).map(|_| (0, 0));
        }
        // The thread's charge comes before anything changes: a refusal must leave the caller's
        // handle with the caller and the process exactly as it was (SYSCALLS.md: on any error
        // nothing changes), or a retry would find handle 1 taken.
        charge_thread(&mut kmem::get().vmos.jobs, target.job)
            .map_err(|e| (job_status(e), None, None))?;
        // Commit: the channel end moves into the new process as its handle 1.
        let UserState { procs, objs, .. } = s;
        let caller = procs.get_mut(&me).expect("caller exists");
        let moved = caller
            .handles
            .remove_for_transfer(objs, bh)
            .map_err(|e| (e, None, None))?;
        let target = procs.get_mut(&tpid).expect("target exists");
        let first = target
            .handles
            .insert(moved.obj, moved.rights)
            .expect("an empty table has room");
        assert_eq!(
            first,
            abi::BOOTSTRAP_HANDLE,
            "the bootstrap channel must be handle 1"
        );
        target.entry = entry;
        target.arg = arg;
        target.start_sp = sp;
        target.state = ProcState::Running;
        target.threads_alive = 1;
        // the first thread of a process starts at the default priority, clamped to its job's maximum
        let first_prio = clamp_prio(target.job, 10);
        Ok((new_start(s, tpid, entry, sp, arg, None), first_prio))
    })?;
    // The new thread (registered by itself once it runs). Spawned after the state borrow is
    // gone: spawning can switch to it at once.
    thread::spawn_free_detached("user", first_prio, user_thread_main, started);
    Ok((None, None))
}

fn process_kill(a: [u64; 6]) -> Reply {
    let ph = Handle(a[0] as u32);
    let code = a[1] as i32;
    let (tpid, me) = irq::without(|| -> Result<(u32, u32), ReplyErr> {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let tpid = ksys::kill_target(&s.objs, &s.procs, me, ph).map_err(|e| (e, None, None))?;
        Ok((tpid, me))
    })?;
    if tpid == me {
        exit_process(code);
    }
    request_kill(tpid, code);
    Ok((None, None))
}

/// `process_suspend(process)` and `process_resume(process)` (SIG2): stop or continue every thread of a process. Both need
/// KILL on the handle (the right to end a process is the right to stop it), are idempotent, and refuse an ended process
/// (BAD_STATE). A process may stop itself: its calling thread parks as it returns.
fn process_stop(a: [u64; 6], stop: bool) -> Reply {
    let ph = Handle(a[0] as u32);
    let tpid = irq::without(|| -> Result<u32, ReplyErr> {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let tpid = ksys::stop_target(&mut s.objs, &mut s.procs, me, ph, stop)
            .map_err(|e| (e, None, None))?;
        bump_attention();
        if stop {
            // a thread running user code on another hart parks at its next trap: the flag is set first and the kick comes
            // in the same locked region (as for a kill)
            crate::smp::kick_others();
        }
        Ok(tpid)
    })?;
    if !stop {
        wake_threads_of(tpid);
    }
    Ok((None, None))
}

/// User threads alive in the whole system.
fn total_threads(s: &UserState) -> usize {
    s.procs.values().map(|p| p.threads_alive).sum()
}

/// What one more thread costs a job (slice T, T1): a thread slot, and the four pages of its kernel stack
/// (billed to the job that owns the thread, for as long as it lives). All or nothing.
fn charge_thread(jobs: &mut kcore::job::Jobs, job: JobId) -> Result<(), JobError> {
    jobs.charge(job, Resource::Threads, 1)?;
    if let Err(e) = jobs.charge(job, Resource::Pages, KSTACK_PAGES) {
        jobs.credit(job, Resource::Threads, 1);
        return Err(e);
    }
    Ok(())
}

fn credit_thread(jobs: &mut kcore::job::Jobs, job: JobId) {
    jobs.credit(job, Resource::Threads, 1);
    jobs.credit(job, Resource::Pages, KSTACK_PAGES);
}

/// Frames in a kernel stack.
const KSTACK_PAGES: u64 = abi::layout::KSTACK_BYTES / 4096;

#[allow(clippy::large_stack_frames)]
// KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)] // INVARIANT: the target process was looked up under the same lock; threads_alive is below MAX_THREADS_PER_PROCESS (checked just above)
fn thread_create(a: [u64; 6]) -> Reply {
    let ph = Handle(a[0] as u32);
    let (entry, sp, arg) = (a[1], a[2], a[3]);
    let (token, handle, new_prio) = irq::without(|| -> Result<(usize, u32, u8), ReplyErr> {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let caller = s.procs.get(&me).ok_or((Status::BadState, None, None))?;
        let (_, tpid, _, _) = ipc::process_of(&s.objs, &caller.handles, ph, Rights::MANAGE)
            .map_err(|e| (e, None, None))?;
        let target = s.procs.get(&tpid).ok_or((Status::BadState, None, None))?;
        if target.state != ProcState::Running
            || target.exit_code.is_some()
            || target.killed.is_some()
        {
            return bad(Status::BadState).map(|_| (0, 0, 0));
        }
        let entry_ok = target.aspace.region_perms_at(entry).is_some_and(|p| p.exec);
        let sp_ok = sp.is_multiple_of(16) && (abi::user::BASE..=abi::user::TOP).contains(&sp);
        if !entry_ok || !sp_ok {
            return bad(Status::InvalidArgs).map(|_| (0, 0, 0));
        }
        if target.threads_alive >= abi::limits::MAX_THREADS_PER_PROCESS
            || total_threads(s) >= abi::limits::MAX_USER_THREADS
        {
            return bad(Status::NoMemory).map(|_| (0, 0, 0));
        }
        let tjob = target.job;
        // a new thread inherits its creator's base priority, clamped to its job's maximum
        let prio = clamp_prio(tjob, creator_prio(s));
        let UserState { objs, procs, .. } = s;
        let jobs = &mut kmem::get().vmos.jobs;
        charge_thread(jobs, tjob).map_err(|e| (job_status(e), None, None))?;
        let caller = procs.get_mut(&me).ok_or((Status::BadState, None, None))?;
        let (handle, obj) = match ipc::thread_object_create_in(
            objs,
            &mut caller.handles,
            tpid,
            &mut quota(caller.job),
        ) {
            Ok(x) => x,
            Err(e) => {
                credit_thread(jobs, tjob);
                return Err((e, None, None));
            }
        };
        procs.get_mut(&tpid).expect("target exists").threads_alive += 1;
        Ok((
            new_start(s, tpid, entry, sp, arg, Some(obj)),
            handle.0,
            prio,
        ))
    })?;
    // Spawned after the state borrow is gone: spawning can switch to the new thread at once.
    thread::spawn_free_detached("user", new_prio, user_thread_main, token);
    Ok((Some(u64::from(handle)), None))
}

/// `thread_spawn(process, args) -> handle, tid` (SYSCALLS.md, SCHED-SMP-SPEC 5.3): create and start a thread, with a
/// stack the kernel maps in the process's stack arena (freed when the thread ends) unless the caller brings its own.
#[allow(clippy::large_stack_frames)]
// KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing
)]
// INVARIANT: stack_bytes and guard_bytes are at most MAX_STACK (checked into `sizes_ok`), so the sums are far below u64::MAX; the target process was looked up under the same lock; the record read is of fixed size (40 bytes) at indices below 5
fn thread_spawn(a: [u64; 6]) -> Reply {
    use abi::spawn as sp;
    let ph = Handle(a[0] as u32);
    let (token, handle, tid_prio) = irq::without(|| -> Result<(usize, u32, u8), ReplyErr> {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        // the record first (a bad pointer is the first error), then the handle
        let raw = {
            let k = kmem::get();
            let c = s.procs.get_mut(&me).ok_or((Status::BadState, None, None))?;
            copy_from_user(&mut c.aspace, &mut k.mm, &mut k.vmos, a[1], sp::ARGS_BYTES)
                .map_err(|e| (e, None, None))?
        };
        let w = |i: usize| u64::from_le_bytes(raw[i * 8..i * 8 + 8].try_into().expect("8 bytes"));
        let (entry, arg, tp, stack_bytes, guard_bytes) = (w(0), w(1), w(2), w(3), w(4));
        let prio_req = (w(5) & 0xffff_ffff) as u32;
        let flags = (w(5) >> 32) as u32;
        let (caller_sp, reserved) = (w(6), w(7));
        let caller = s.procs.get(&me).ok_or((Status::BadState, None, None))?;
        let (_, tpid, _, _) = ipc::process_of(&s.objs, &caller.handles, ph, Rights::MANAGE)
            .map_err(|e| (e, None, None))?;
        let target = s.procs.get(&tpid).ok_or((Status::BadState, None, None))?;
        let tjob = target.job;
        // a priority above the job's maximum is refused (ACCESS_DENIED), not clamped: the program asked for it
        let max = kmem::get().vmos.jobs.sched(tjob).map_or(10, |j| j.max_prio);
        if (1..=63).contains(&prio_req) && prio_req as u8 > max {
            return bad(Status::AccessDenied).map(|_| (0, 0, 0));
        }
        if target.state != ProcState::Running
            || target.exit_code.is_some()
            || target.killed.is_some()
        {
            return bad(Status::BadState).map(|_| (0, 0, 0));
        }
        let own_stack = flags & sp::CALLER_STACK != 0;
        let entry_ok = target.aspace.region_perms_at(entry).is_some_and(|p| p.exec);
        let sizes_ok = stack_bytes <= sp::MAX_STACK && guard_bytes <= sp::MAX_STACK;
        let sp_ok = if own_stack {
            caller_sp.is_multiple_of(16) && (abi::user::BASE..=abi::user::TOP).contains(&caller_sp)
        } else {
            caller_sp == 0
        };
        // (with a caller stack the sizes mean nothing: asking for some is a mistake)
        let sizes_ok = sizes_ok && (!own_stack || (stack_bytes == 0 && guard_bytes == 0));
        if !entry_ok
            || !sizes_ok
            || !sp_ok
            || reserved != 0
            || prio_req > 63
            || flags & !(sp::ROUND_ROBIN | sp::DETACHED | sp::CALLER_STACK) != 0
        {
            return bad(Status::InvalidArgs).map(|_| (0, 0, 0));
        }
        if target.threads_alive >= abi::limits::MAX_THREADS_PER_PROCESS
            || total_threads(s) >= abi::limits::MAX_USER_THREADS
        {
            return bad(Status::NoMemory).map(|_| (0, 0, 0));
        }
        // where the stack goes: a gap in the arena for the guard and the stack, at or after the hint, wrapping once
        let stack_len = if stack_bytes == 0 {
            sp::DEFAULT_STACK
        } else {
            stack_bytes.next_multiple_of(4096)
        };
        let guard_len = if guard_bytes == 0 {
            4096
        } else {
            guard_bytes.next_multiple_of(4096)
        };
        let place = if own_stack {
            None
        } else {
            let (lo, hi) = (abi::user::STACK_ARENA_BASE, abi::user::STACK_ARENA_END);
            // the stack's guard below it, and one more guard above it: the next stack up has a guard of its own
            // below *its* start, so two stacks are never flush (a neighbour's overflow always faults)
            let total = guard_len + stack_len + guard_len;
            let at = target
                .aspace
                .find_gap(lo, hi, total, target.stack_hint)
                .or_else(|| target.aspace.find_gap(lo, hi, total, lo));
            match at {
                Some(at) => Some(at + guard_len),
                None => return bad(Status::NoMemory).map(|_| (0, 0, 0)),
            }
        };
        let prio = if prio_req == 0 {
            clamp_prio(tjob, creator_prio(s))
        } else {
            prio_req as u8
        };
        let detached = flags & sp::DETACHED != 0;
        let UserState { objs, procs, .. } = s;
        let jobs = &mut kmem::get().vmos.jobs;
        charge_thread(jobs, tjob).map_err(|e| (job_status(e), None, None))?;
        // the thread's object and handle (not for a detached thread)
        let made = if detached {
            None
        } else {
            let caller = procs.get_mut(&me).ok_or((Status::BadState, None, None))?;
            match ipc::thread_object_create_in(
                objs,
                &mut caller.handles,
                tpid,
                &mut quota(caller.job),
            ) {
                Ok(x) => Some(x),
                Err(e) => {
                    credit_thread(jobs, tjob);
                    return Err((e, None, None));
                }
            }
        };
        // the stack: a VMO of its own (nobody holds a handle), mapped read-write; its table pages are charged like any mapping's
        let mut stack = None;
        let mut top = caller_sp;
        if let Some(region) = place {
            let k = kmem::get();
            let t = procs.get_mut(&tpid).expect("target exists");
            let tables = stack_len / 4096 / 512 + 4;
            let undo = |made: Option<(Handle, kcore::object::ObjId)>,
                        procs: &mut BTreeMap<u32, Process>,
                        objs: &mut Kobjs,
                        jobs: &mut kcore::job::Jobs| {
                if let Some((h, _)) = made {
                    if let Some(c) = procs.get_mut(&me) {
                        let _ = c.handles.close(objs, h);
                    }
                }
                credit_thread(jobs, tjob);
            };
            if let Err(e) = k.vmos.jobs.charge(tjob, Resource::Pages, tables) {
                undo(made, procs, objs, &mut k.vmos.jobs);
                return Err((job_status(e), None, None));
            }
            let vmo = k.vmos.create_in(stack_len / 4096, tjob);
            let perms = Perms {
                read: true,
                write: true,
                exec: false,
                user: true,
            };
            if let Err(e) = t
                .aspace
                .map_region(&k.vmos, region, stack_len / 4096, vmo, 0, perms)
            {
                let _ = k.vmos.destroy(&mut k.mm, vmo);
                k.vmos.jobs.credit(tjob, Resource::Pages, tables);
                undo(made, procs, objs, &mut k.vmos.jobs);
                return Err(match e {
                    kcore::aspace::RegionError::TooMany => (Status::NoMemory, None, None),
                    _ => (Status::InvalidArgs, None, None),
                });
            }
            t.table_charge.insert(region, tables);
            t.stack_hint = region + stack_len;
            stack = Some(SpawnStack { region, vmo });
            top = region + stack_len;
        }
        procs.get_mut(&tpid).expect("target exists").threads_alive += 1;
        let token = new_start(s, tpid, entry, top, arg, made.map(|(_, o)| o));
        let st = s.starts.get_mut(&token).expect("just registered");
        st.tp = tp;
        st.stack = stack;
        st.rr = flags & sp::ROUND_ROBIN != 0;
        st.spawned = true;
        Ok((token, made.map_or(0, |(h, _)| h.0), prio))
    })?;
    // Spawned after the state borrow is gone: spawning can switch to the new thread at once.
    let tid = thread::spawn_free_detached("user", tid_prio, user_thread_main, token);
    Ok((Some(u64::from(handle)), Some(u64::from(tid))))
}

/// `thread_interrupt(thread)` (T56, SCHED-SMP-SPEC task 26): set the thread's interrupt flag and wake it. The next blocking
/// wait that would sleep (object_wait, port_wait, futex_wait, channel_call, with a deadline other than 0) returns
/// INTERRUPTED and takes the flag; one that is satisfied, or a poll, leaves it. It ends waits, not running code.
/// Needs MANAGE on a thread handle; a thread that has ended is BAD_STATE.
fn thread_interrupt(a: [u64; 6]) -> Reply {
    if a[0] > u64::from(u32::MAX) {
        return fail(Status::BadHandle);
    }
    let tid = irq::without(|| -> Result<Tid, ReplyErr> {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let (tid, _) = thread_target(s, me, a[0], Rights::MANAGE)?;
        s.interrupts.insert(tid);
        Ok(tid)
    })?;
    // (after the state closure has ended: a wake can switch threads)
    thread::wake(tid);
    Ok((None, None))
}

// ---- jobs (S4.5, OD56-OD58) ----

use ksys::job_status;

/// The base priority a new thread starts at (SCHED-SMP-SPEC 4.9): `want`, clamped to the job's `max_prio` (and at
/// least 1: a user thread is never the idle priority).
fn clamp_prio(job: JobId, want: u8) -> u8 {
    let max = kmem::get().vmos.jobs.sched(job).map_or(10, |s| s.max_prio);
    want.min(max).max(1)
}

/// The priority a thread created by the calling thread inherits: the caller's own base if it is a user thread, else
/// the default (10).
fn creator_prio(s: &UserState) -> u8 {
    let me = thread::current();
    if s.thread_pid.contains_key(&me) {
        thread::priority_of(me).unwrap_or(10)
    } else {
        10
    }
}

/// The kernel thread behind a thread handle with `need`, and the process it belongs to.
fn thread_target(s: &UserState, me: u32, h: u64, need: Rights) -> Result<(Tid, u32), ReplyErr> {
    ksys::thread_target(&s.objs, &s.procs, me, thread::current(), h, need)
        .map_err(|e| (e, None, None))
}

/// `thread_set_sched(thread, priority, flags)`: set a thread's base priority (1 to 63) and flags (bit 0: round robin
/// within its level). Needs MANAGE on the handle (none for 0, the caller), and the priority may not exceed the
/// thread's job's `max_prio`.
fn thread_set_sched(a: [u64; 6]) -> Reply {
    irq::without(|| -> Reply {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let (tid, prio, flags) = ksys::thread_set_sched_target(
            &s.objs,
            &s.procs,
            &kmem::get().vmos.jobs,
            me,
            thread::current(),
            a,
        )
        .map_err(|e| (e, None, None))?;
        s.sched_flags.insert(tid, flags);
        thread::set_priority(tid, prio);
        Ok((None, None))
    })
}

/// `thread_get_sched(thread) -> sched, tid`: `sched` packs the base priority (bits 0 to 7), the flags (8 to 15) and
/// the effective priority (16 to 23; equal to the base until donation exists). Needs INSPECT (none for 0).
fn thread_get_sched(a: [u64; 6]) -> Reply {
    irq::without(|| -> Reply {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let (tid, _) = thread_target(s, me, a[0], Rights::INSPECT)?;
        let prio = u64::from(thread::priority_of(tid).ok_or((Status::BadState, None, None))?);
        let flags = u64::from(s.sched_flags.get(&tid).copied().unwrap_or(0));
        Ok((Some(prio | flags << 8 | prio << 16), Some(u64::from(tid))))
    })
}

/// `job_set_sched(job, max_prio, rt_budget_ns, cpu_weight)`: needs SET_LIMITS. No value may exceed the parent job's;
/// lowering `max_prio` lowers the base priority of every thread in the job and below that is above it.
fn job_set_sched(a: [u64; 6]) -> Reply {
    irq::without(|| -> Reply {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let jobs = &mut kmem::get().vmos.jobs;
        let job =
            ksys::job_set_sched(&s.objs, &s.procs, jobs, me, a).map_err(|e| (e, None, None))?;
        // PR1: clamp the threads of the job and everything below it
        let clamp: Vec<(Tid, u8)> = s
            .thread_pid
            .iter()
            .filter_map(|(&tid, pid)| {
                let pj = s.procs.get(pid)?.job;
                let max = jobs.sched(pj)?.max_prio;
                let now = thread::priority_of(tid)?;
                (jobs.is_within(pj, job) && now > max).then_some((tid, max.max(1)))
            })
            .collect();
        thread::set_priorities(&clamp);
        Ok((None, None))
    })
}

/// `job_kill(job, code)`: end every process in the job and its descendants with exit code
/// `code` and forbid new processes and threads there (their limits become 0). Needs KILL. A
/// caller inside the job ends too, last.
fn job_kill(a: [u64; 6]) -> Reply {
    let code = a[1] as i32;
    let (victims, me_inside, me) = irq::without(|| -> Result<(Vec<u32>, bool, u32), ReplyErr> {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let (victims, me_inside) = ksys::job_kill_victims(
            &s.objs,
            &s.procs,
            &mut kmem::get().vmos.jobs,
            me,
            Handle(a[0] as u32),
        )
        .map_err(|e| (e, None, None))?;
        Ok((victims, me_inside, me))
    })?;
    for pid in victims {
        request_kill(pid, code);
    }
    if me_inside {
        let _ = me;
        exit_process(code);
    }
    Ok((None, None))
}
