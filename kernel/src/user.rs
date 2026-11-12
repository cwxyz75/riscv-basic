//! User mode (S3b): processes, the program loader, the syscall dispatcher and the
//! handling of traps from U-mode. The logic that can be pure (ELF validation, user memory
//! copies, handle tables, channels) lives in `kcore` and is tested on the host; this file
//! wires it to the machine.

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
use kcore::job::{JobError, JobId, Limits, Resource};
use kcore::pagetable::{PageTable, Perms};
use kcore::sched::Tid;
use kcore::usercopy::{copy_from_user, copy_to_user, verify_writable};

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

struct Process {
    aspace: AddressSpace,
    handles: HandleTable,
    asid: u16,
    vmos: Vec<u64>,
    entry: u64,
    arg: u64,
    exit_code: Option<i32>,
    /// The creator's end of the bootstrap channel (a handle in `UserState::ktable`), until
    /// a test or server takes it with `take_bootstrap`.
    kernel_end: Option<Handle>,
    /// Where the first thread starts the stack (0: the default `STACK_TOP`).
    start_sp: u64,
    /// The process's object (for TERMINATED and kill-on-close), if it has one.
    obj: Option<kcore::object::ObjId>,
    state: ProcState,
    /// Started by `process_start`: its own thread tears it down when it ends and nobody
    /// joins that thread.
    detached: bool,
    /// `process_kill` asked for it to end, with this exit code; its thread dies at its next
    /// return to user mode (or wakes from a wait to do so).
    killed: Option<i32>,
    /// User threads that exist and have not ended (counted from `thread_create`, before the
    /// thread first runs). The process is torn down when this reaches 0.
    threads_alive: usize,
    /// The thread objects of the live threads, to mark them TERMINATED when they end.
    thread_objs: BTreeMap<Tid, kcore::object::ObjId>,
    /// The job this process is charged to (it holds one use of it): its process, its threads
    /// and, through the VMOs made for it, its memory.
    job: JobId,
    /// Pages charged to `job` for the page-table frames this process's mappings can need (a
    /// worst-case estimate per mapped region, plus the root table), by region start; credited
    /// when the region goes or the process ends. Table frames are not VMO pages, so without this
    /// a program could spend memory the page limit never sees.
    table_charge: BTreeMap<u64, u64>,
    /// Started by the kernel (`start`): may ask for the root job (`job_root`). Nothing a program
    /// creates is privileged.
    privileged: bool,
}

/// What a new user thread needs to start (looked up by the thread itself: a thread's start
/// argument is one word).
struct ThreadStart {
    pid: u32,
    entry: u64,
    sp: u64,
    arg: u64,
    obj: Option<kcore::object::ObjId>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProcState {
    /// Created and possibly populated, no thread yet.
    Created,
    Running,
}

struct UserState {
    objs: Kobjs,
    procs: BTreeMap<u32, Process>,
    thread_pid: BTreeMap<Tid, u32>,
    next_pid: u32,
    /// VMOs whose last handle is gone but that some address space still maps: freed when the
    /// last mapping goes (SYSCALLS.md: a VMO lives while a handle or a mapping does).
    pending_vmos: Vec<u64>,
    /// Thread starts waiting for their thread to run, by token.
    starts: BTreeMap<usize, ThreadStart>,
    next_token: usize,
    /// Handles the kernel itself holds (the creator's ends of bootstrap channels).
    ktable: HandleTable,
    /// The one wait queue every blocked thread sleeps on (OD43): any syscall that can change
    /// a signal wakes it, and each waiter re-checks its own condition.
    signal_queue: usize,
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

static mut USER: Option<UserState> = None;

fn state() -> &'static mut UserState {
    debug_assert!(!irq::enabled(), "user state touched with interrupts on");
    // SAFETY: one hart, interrupts off (debug-checked); every caller drops the reference
    // before any context switch, so no two live `&mut` alias.
    unsafe {
        let slot = &mut *addr_of_mut!(USER);
        slot.get_or_insert_with(|| UserState {
            objs: Kobjs::new(),
            procs: BTreeMap::new(),
            thread_pid: BTreeMap::new(),
            next_pid: 1,
            pending_vmos: Vec::new(),
            starts: BTreeMap::new(),
            next_token: 1,
            ktable: HandleTable::new(256),
            signal_queue: thread::new_queue(),
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
/// Jobs alive (the root included) and the root job's live process and thread counts: all back
/// to 1, 0 and 0 when nothing is left (a leak oracle for the ledger, INVARIANTS J2).
pub fn job_ledger() -> (usize, u64, u64) {
    let jobs = &kmem::get().vmos.jobs;
    let u = jobs.usage(JobId::ROOT).unwrap_or_default();
    (jobs.live(), u.processes, u.threads)
}

pub fn process_count() -> usize {
    irq::without(|| state().procs.len())
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
pub fn start(image: &[u8], arg: u64) -> Result<(u32, Tid), LoadError> {
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
        s.next_pid += 1;
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
        threads_alive: 1,
        thread_objs: BTreeMap::new(),
        job: JobId::ROOT,
        table_charge: BTreeMap::new(),
        privileged: true,
    };
    // Every process starts with one handle, value 1: the bootstrap channel (SYSCALLS.md).
    let mut process = process;
    let made = irq::without(|| -> bool {
        let s = state();
        let Ok((kernel_end, process_end)) = ipc::channel_create(&mut s.objs, &mut s.ktable) else {
            return false;
        };
        let entry = s
            .ktable
            .remove_for_transfer(process_end)
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
        let _ = jobs.charge(JobId::ROOT, Resource::Threads, 1);
        s.procs.insert(pid, process);
        new_start(s, pid, entry, STACK_TOP, arg, None)
    });
    let tid = thread::spawn("user", 10, user_thread_main, token);
    Ok((pid, tid))
}

/// Register a thread start and return its token.
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
        },
    );
    token
}

fn user_thread_main(token: usize) {
    let top = thread::current_stack_top();
    // Interrupts stay off from here until `enter_user` is in U-mode: a kill that arrives in
    // between would otherwise be seen only at the next trap, after user code had run.
    let _ = irq::save_and_disable();
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
        let satp = satp_of(p);
        (ts, satp, p.killed)
    };
    // Killed before it ever ran: it never runs user code.
    if let Some(code) = killed {
        irq::enable();
        leave(code, true);
    }
    // SAFETY: `enter_user` switches to the process's address space and drops to U-mode at
    // its entry point with the stack and argument chosen above; it never returns.
    unsafe { enter_user(ts.entry, ts.sp, ts.arg, satp, top) }
}

core::arch::global_asm!(
    r#"
    .section .text
    .globl enter_user
    .balign 4
enter_user:                      # a0 entry, a1 user sp, a2 arg, a3 satp, a4 kernel stack top
    csrci sstatus, 2             # interrupts off until we are in U-mode
    csrw  satp, a3
    sfence.vma zero, zero
    csrw  sepc, a0
    csrw  sscratch, a4           # from now on a trap is a trap from user mode
    li    t0, 256
    csrc  sstatus, t0            # SPP = 0: sret goes to U-mode
    li    t0, 32
    csrs  sstatus, t0            # SPIE = 1: interrupts on in U-mode
    mv    sp, a1
    mv    a0, a2
    li x1, 0
    li x3, 0
    li x4, 0
    li x5, 0
    li x6, 0
    li x7, 0
    li x8, 0
    li x9, 0
    li x11, 0
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
    fn enter_user(entry: u64, sp: u64, arg: u64, satp: u64, kernel_top: u64) -> !;
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
        let tables: u64 = process.table_charge.values().sum();
        k.vmos.jobs.credit(process.job, Resource::Pages, tables);
        k.vmos.jobs.credit(process.job, Resource::Processes, 1);
        k.vmos.jobs.release(process.job);
    });
    kmem::flush_tlb();
    REAP_DIRTY.store(true, Ordering::SeqCst); // its mappings are gone: pending VMOs may go
    notify(); // peers of what this process held now see PEER_CLOSED; waiters see TERMINATED
    code
}

fn exit_process(code: i32) -> ! {
    leave(code, true)
}

/// The calling thread ends. With `whole`, its process ends too (`process_exit`, a fault, a
/// kill): every other thread is told to die. Without it (`thread_exit`) only this thread goes,
/// and the process ends when the last thread does. The process is torn down by its last thread
/// (a process started by `process_start`), or by `finish` (one the kernel started).
fn leave(code: i32, whole: bool) -> ! {
    let me = irq::without(|| {
        let s = state();
        let tid = thread::current();
        let pid = s.thread_pid.remove(&tid)?;
        let UserState { objs, procs, .. } = s;
        let p = procs.get_mut(&pid)?;
        // A code the process was already given by a kill wins over this thread's own.
        let code = p.killed.unwrap_or(code);
        if let Some(o) = p.thread_objs.remove(&tid) {
            ipc::thread_mark_terminated(objs, o, code);
        }
        p.threads_alive -= 1;
        kmem::get().vmos.jobs.credit(p.job, Resource::Threads, 1);
        if whole {
            p.exit_code.get_or_insert(code);
            p.killed.get_or_insert(code);
        } else if p.threads_alive == 0 {
            p.exit_code.get_or_insert(code);
        }
        Some((pid, p.detached, p.threads_alive))
    });
    match me {
        Some((_, _, alive)) if alive > 0 => notify(), // siblings and waiters on this thread
        Some((pid, true, _)) => {
            teardown(pid);
        }
        // the last thread of a kernel-started process: `finish` tears it down, but waiters
        // on this thread must not have to wait for that poll
        _ => notify(),
    }
    thread::exit()
}

/// The exit code the calling thread's process was told to die with (`process_kill`), if any.
fn killed_now() -> Option<i32> {
    let s = state();
    let pid = s.thread_pid.get(&thread::current())?;
    s.procs.get(pid)?.killed
}

/// The satp value last written for user mode (so a switch is only made when needed).
static ACTIVE_SATP: AtomicU64 = AtomicU64::new(0);

fn satp_of(p: &Process) -> u64 {
    (8u64 << 60) | (u64::from(p.asid) << 44) | p.aspace.table().root()
}

/// Make the calling thread's process the current address space, just before returning to
/// user mode. Needed because the hart's satp belongs to whichever user thread last ran:
/// threads of different processes are switched between, and the kernel half is shared so
/// kernel code runs under any of them, but returning to user mode must use *this* one.
pub fn activate_current() {
    // A process that was killed while its thread was not running dies here, at the point
    // where it would otherwise return to user mode.
    if let Some(code) = irq::without(killed_now) {
        exit_process(code);
    }
    let satp = irq::without(|| {
        let s = state();
        s.thread_pid
            .get(&thread::current())
            .and_then(|pid| s.procs.get(pid))
            .map(satp_of)
    });
    if let Some(satp) = satp {
        if ACTIVE_SATP.swap(satp, Ordering::SeqCst) != satp || kmem::satp() != satp {
            // SAFETY: `satp` names a live process root whose upper half is the kernel's; the
            // flush drops the translations of whatever table was active.
            unsafe { core::arch::asm!("csrw satp, {}", "sfence.vma zero, zero", in(reg) satp) };
        }
    }
}

/// A trap from user mode (called from the trap handler with interrupts off).
pub fn user_trap(frame: &mut crate::trap::TrapFrame) {
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
                        p.aspace
                            .handle_fault(&mut k.mm, &mut k.vmos, va, access)
                            .is_ok()
                    }
                    _ => false,
                }
            };
            if resolved {
                kmem::flush_tlb();
            } else {
                exit_process(-1000 - cause as i32);
            }
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

fn syscall(frame: &mut crate::trap::TrapFrame) {
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
        nr::CLOCK_GET => frame.set_return(0, Some(thread::ticks()), None),
        nr::OBJECT_WAIT => reply(frame, object_wait(a)),
        nr::PORT_WAIT => reply(frame, port_wait(a)),
        nr::PROCESS_CREATE
        | nr::JOB_CREATE
        | nr::JOB_SET_LIMITS
        | nr::JOB_INFO
        | nr::JOB_KILL
        | nr::JOB_ROOT
        | nr::PROCESS_SELF
        | nr::PROCESS_MAP
        | nr::PROCESS_UNMAP
        | nr::PROCESS_START
        | nr::PROCESS_KILL
        | nr::PROCESS_INFO
        | nr::THREAD_CREATE
        | nr::THREAD_INFO => {
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
                    | nr::PORT_BIND
                    | nr::PORT_UNBIND
            ) {
                notify();
            }
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
        nr::CHANNEL_CREATE | nr::EVENT_CREATE | nr::SOCKET_CREATE | nr::PORT_CREATE
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
        ..
    } = s;
    let p = procs.get_mut(&pid).expect("process exists");
    let h = |i: usize| Handle(a[i] as u32);
    // Every 32-bit argument must really fit 32 bits: a value with high bits set names no
    // handle, grants no right and sets no signal, instead of being silently truncated.
    let over = |i: usize| a[i] > u64::from(u32::MAX);
    let bad_args = match n {
        nr::HANDLE_CLOSE | nr::OBJECT_SIGNALS | nr::OBJECT_INFO | nr::CHANNEL_WRITE if over(0) => {
            Some(Status::BadHandle)
        }
        nr::CHANNEL_READ | nr::CHANNEL_READ_TXID if over(0) => Some(Status::BadHandle),
        nr::VMO_READ | nr::VMO_WRITE | nr::VMO_GET_SIZE if over(0) => Some(Status::BadHandle),
        nr::VMO_CREATE if over(1) => Some(Status::BadHandle),
        nr::SOCKET_WRITE | nr::SOCKET_READ | nr::PORT_UNBIND if over(0) => Some(Status::BadHandle),
        nr::PORT_BIND if over(0) || over(1) => Some(Status::BadHandle),
        nr::HANDLE_DUPLICATE | nr::HANDLE_REPLACE if over(0) => Some(Status::BadHandle),
        nr::HANDLE_DUPLICATE | nr::HANDLE_REPLACE if over(1) => Some(Status::AccessDenied),
        nr::EVENT_SIGNAL if over(0) => Some(Status::BadHandle),
        nr::EVENT_SIGNAL if over(1) || over(2) => Some(Status::InvalidArgs),
        nr::CHANNEL_READ_TXID if over(1) => Some(Status::InvalidArgs),
        _ => None,
    };
    if let Some(status) = bad_args {
        return fail(status);
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
        nr::HANDLE_CLOSE => p
            .handles
            .close(objs, h(0))
            .map(|_| (None, None))
            .map_err(|e| (e, None, None)),
        nr::HANDLE_DUPLICATE => {
            let nh = p
                .handles
                .duplicate(objs, h(0), Rights(a[1] as u32))
                .map_err(|e| (e, None, None))?;
            Ok((Some(u64::from(nh.0)), None))
        }
        nr::HANDLE_REPLACE => {
            let nh = p
                .handles
                .replace(h(0), Rights(a[1] as u32))
                .map_err(|e| (e, None, None))?;
            Ok((Some(u64::from(nh.0)), None))
        }
        nr::CHANNEL_CREATE => {
            let (x, y) = ipc::channel_create(objs, &mut p.handles).map_err(|e| (e, None, None))?;
            Ok((Some(u64::from(x.0)), Some(u64::from(y.0))))
        }
        nr::SOCKET_CREATE => {
            let (x, y) = ipc::socket_create(objs, &mut p.handles).map_err(|e| (e, None, None))?;
            Ok((Some(u64::from(x.0)), Some(u64::from(y.0))))
        }
        nr::SOCKET_WRITE => {
            let len = a[2] as usize;
            ipc::socket_write_check(objs, &p.handles, h(0), len).map_err(|e| (e, None, None))?;
            let data = copy_from_user(&mut p.aspace, &mut k.mm, &mut k.vmos, a[1], len)
                .map_err(|e| (e, None, None))?;
            ipc::socket_write(objs, &p.handles, h(0), &data)
                .map(|n| (Some(n as u64), None))
                .map_err(|e| (e, None, None))
        }
        nr::SOCKET_READ => {
            let max = a[2] as usize;
            // What a read would deliver, checked against the destination before anything is
            // removed, so a bad pointer never loses data.
            let n = ipc::socket_peek(objs, &p.handles, h(0), max).map_err(|e| (e, None, None))?;
            verify_writable(&mut p.aspace, &mut k.mm, &mut k.vmos, a[1], n)
                .map_err(|e| (e, None, None))?;
            let data =
                ipc::socket_read(objs, &p.handles, h(0), max).map_err(|e| (e, None, None))?;
            let _ = copy_to_user(&mut p.aspace, &mut k.mm, &mut k.vmos, a[1], &data);
            Ok((Some(data.len() as u64), None))
        }
        nr::PORT_CREATE => {
            let port = ipc::port_create(objs, &mut p.handles).map_err(|e| (e, None, None))?;
            Ok((Some(u64::from(port.0)), None))
        }
        nr::PORT_BIND => ipc::port_bind(objs, &p.handles, h(0), h(1), a[2], a[3])
            .map(|_| (None, None))
            .map_err(|e| (e, None, None)),
        nr::PORT_UNBIND => ipc::port_unbind(objs, &p.handles, h(0), a[1])
            .map(|_| (None, None))
            .map_err(|e| (e, None, None)),
        nr::EVENT_CREATE => {
            let e = ipc::event_create(objs, &mut p.handles).map_err(|e| (e, None, None))?;
            Ok((Some(u64::from(e.0)), None))
        }
        nr::EVENT_SIGNAL => ipc::event_signal(objs, &p.handles, h(0), a[1] as u32, a[2] as u32)
            .map(|_| (None, None))
            .map_err(|e| (e, None, None)),
        nr::OBJECT_INFO => ipc::object_info(objs, &p.handles, h(0))
            .map(|(k, peer)| (Some(k), Some(peer)))
            .map_err(|e| (e, None, None)),
        nr::OBJECT_SIGNALS => ipc::signals_of(objs, &p.handles, h(0))
            .map(|s| (Some(u64::from(s)), None))
            .map_err(|e| (e, None, None)),
        nr::CHANNEL_WRITE => {
            let (nbytes, nhandles) = (a[2] as usize, a[4] as usize);
            // Sizes first: they bound everything that follows (SYSCALLS.md).
            if nbytes > MAX_MSG_BYTES || nhandles > MAX_MSG_HANDLES {
                return fail(Status::TooBig);
            }
            let data = copy_from_user(&mut p.aspace, &mut k.mm, &mut k.vmos, a[1], nbytes)
                .map_err(|e| (e, None, None))?;
            let raw = copy_from_user(&mut p.aspace, &mut k.mm, &mut k.vmos, a[3], nhandles * 4)
                .map_err(|e| (e, None, None))?;
            ipc::channel_write(objs, &mut p.handles, h(0), &data, &handle_list(&raw))
                .map(|_| (None, None))
                .map_err(|e| (e, None, None))
        }
        nr::CHANNEL_READ | nr::CHANNEL_READ_TXID => {
            let (txid, bytes_at, max_bytes, handles_at, max_handles) = if n == nr::CHANNEL_READ {
                (None, a[1], a[2] as usize, a[3], a[4] as usize)
            } else {
                (Some(a[1] as u32), a[2], a[3] as usize, a[4], a[5] as usize)
            };
            // A message never exceeds the limits, so larger declared buffers act as the limit.
            let (mb, mh) = (
                max_bytes.min(MAX_MSG_BYTES),
                max_handles.min(MAX_MSG_HANDLES),
            );
            // Find out what this read would deliver, and check the destinations against the
            // *actual* message (not the declared capacity) before dequeuing anything, so a
            // bad pointer can never lose a message and a generous `max_bytes` needs no
            // generous mapping.
            let (nbytes, nhandles) = match ipc::channel_peek(objs, &p.handles, h(0), txid, mb, mh) {
                Ok(sizes) => sizes,
                Err(ReadError::TooSmall { bytes, handles }) => {
                    return Err((
                        Status::BufferTooSmall,
                        Some(bytes as u64),
                        Some(handles as u64),
                    ))
                }
                Err(ReadError::Status(st)) => return fail(st),
            };
            verify_writable(&mut p.aspace, &mut k.mm, &mut k.vmos, bytes_at, nbytes)
                .map_err(|e| (e, None, None))?;
            verify_writable(
                &mut p.aspace,
                &mut k.mm,
                &mut k.vmos,
                handles_at,
                nhandles * 4,
            )
            .map_err(|e| (e, None, None))?;
            let got = match txid {
                None => ipc::channel_read(objs, &mut p.handles, h(0), mb, mh),
                Some(t) => ipc::channel_read_txid(objs, &mut p.handles, h(0), t, mb, mh),
            };
            match got {
                Ok(r) => {
                    let raw: Vec<u8> = r.handles.iter().flat_map(|h| h.0.to_le_bytes()).collect();
                    // The destinations were verified, so these cannot fail.
                    let _ = copy_to_user(&mut p.aspace, &mut k.mm, &mut k.vmos, bytes_at, &r.data);
                    let _ = copy_to_user(&mut p.aspace, &mut k.mm, &mut k.vmos, handles_at, &raw);
                    Ok((Some(r.data.len() as u64), Some(r.handles.len() as u64)))
                }
                Err(ReadError::TooSmall { bytes, handles }) => Err((
                    Status::BufferTooSmall,
                    Some(bytes as u64),
                    Some(handles as u64),
                )),
                Err(ReadError::Status(st)) => Err((st, None, None)),
            }
        }
        nr::VMO_CREATE => {
            if a[0] == 0 || a[0] > abi::limits::MAX_VMO_PAGES {
                return fail(Status::InvalidArgs);
            }
            // a1: the job the memory is charged to (a handle with MANAGE), or 0 for the caller's own
            let job = if a[1] == 0 {
                p.job
            } else {
                ipc::job_of(objs, &p.handles, h(1), Rights::MANAGE).map_err(|e| (e, None, None))?
            };
            let id = k.vmos.create_in(a[0], job);
            match ipc::vmo_object_create(objs, &mut p.handles, id) {
                Ok(h) => Ok((Some(u64::from(h.0)), None)),
                Err(e) => {
                    let _ = k.vmos.destroy(&mut k.mm, id);
                    fail(e)
                }
            }
        }
        nr::VMO_GET_SIZE => {
            let id =
                ipc::vmo_of(objs, &p.handles, h(0), Rights::NONE).map_err(|e| (e, None, None))?;
            let pages = k.vmos.pages(id).ok_or((Status::BadHandle, None, None))?;
            Ok((Some(pages * 4096), None))
        }
        nr::VMO_READ | nr::VMO_WRITE => {
            let writing = n == nr::VMO_WRITE;
            let need = if writing { Rights::WRITE } else { Rights::READ };
            let id = ipc::vmo_of(objs, &p.handles, h(0), need).map_err(|e| (e, None, None))?;
            let (offset, buf, len) = (a[1], a[2], a[3]);
            if len > MAX_MSG_BYTES as u64 {
                return fail(Status::TooBig);
            }
            let len = len as usize;
            let size = k.vmos.pages(id).ok_or((Status::BadHandle, None, None))? * 4096;
            if offset.checked_add(len as u64).is_none_or(|end| end > size) {
                return fail(Status::InvalidArgs);
            }
            if writing {
                let data = copy_from_user(&mut p.aspace, &mut k.mm, &mut k.vmos, buf, len)
                    .map_err(|e| (e, None, None))?;
                k.vmos
                    .write(&mut k.mm, id, offset, &data)
                    .map_err(|_| (Status::NoMemory, None, None))?;
            } else {
                verify_writable(&mut p.aspace, &mut k.mm, &mut k.vmos, buf, len)
                    .map_err(|e| (e, None, None))?;
                let mut data = alloc::vec![0u8; len];
                k.vmos
                    .read(&mut k.mm, id, offset, &mut data)
                    .map_err(|_| (Status::InvalidArgs, None, None))?;
                let _ = copy_to_user(&mut p.aspace, &mut k.mm, &mut k.vmos, buf, &data);
            }
            Ok((None, None))
        }
        _ => fail(Status::BadSyscall),
    }
}

/// Write user output to the console as whole lines. Each `debug_write` call is flushed at
/// its end (a call that does not finish with a newline gets one), and a line longer than
/// 512 bytes is cut, so a program can never leave a half line behind to swallow its next
/// message. A well-formed `##TEST name PASS|FAIL ...` line is a result and is counted;
/// any other line starting `##` is prefixed so user code cannot forge the kernel's
/// verdict lines (`##DONE`, `##PANIC`, `##BOOT`).
fn emit(line: &mut String, pass: &mut u32, fail: &mut u32, bytes: &[u8]) {
    let mut flush = |text: String| {
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
                *pass += 1;
                let _ = writeln!(Console, "{text}");
            }
            Some(false) => {
                *fail += 1;
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

/// Wake every thread waiting on a signal (they re-check their own conditions).
pub fn notify() {
    cleanup_dead();
    reap_vmos();
    let q = irq::without(|| state().signal_queue);
    thread::wake_all(q);
}

static CLEANING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Do the kernel work that destroying objects calls for: free the pages of a VMO that has
/// no handle left, and kill a process whose last handle was closed. (The registry cannot
/// reach the frame database or the process table, so it hands the dead objects back.)
fn cleanup_dead() {
    if CLEANING.swap(true, Ordering::SeqCst) {
        return; // a cleanup is already running further up the stack; it will see our work
    }
    loop {
        let dead = irq::without(|| state().objs.drain_dead());
        if dead.is_empty() {
            // Clear the flag and look once more in the same interrupts-off step, so work
            // pushed between the last drain and the clear is not stranded.
            let again = irq::without(|| {
                CLEANING.store(false, Ordering::SeqCst);
                state().objs.has_dead()
            });
            if again && !CLEANING.swap(true, Ordering::SeqCst) {
                continue;
            }
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
fn request_kill(pid: u32, code: i32) {
    let direct = irq::without(|| {
        let s = state();
        match s.procs.get_mut(&pid) {
            Some(p) if p.state == ProcState::Created => {
                p.exit_code = Some(code);
                true
            }
            Some(p) => {
                p.killed.get_or_insert(code);
                false
            }
            None => false,
        }
    });
    if direct {
        teardown(pid);
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
        let q = irq::without(|| state().signal_queue);
        // A handle that stops naming anything ends the wait (the check below then reports it).
        thread::wait_deadline(
            q,
            || killed_now().is_some() || signals_now(h).map_or(true, |s| s & mask != 0),
            deadline,
        );
    }
    // The handle may have gone while we slept: BAD_HANDLE, not a wait that quietly times out.
    let seen = observed().map_err(|e| (e, None, None))?;
    if seen & mask != 0 {
        Ok((Some(u64::from(seen)), None))
    } else {
        Err((Status::TimedOut, Some(u64::from(seen)), None))
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
        with_proc(|objs, p, _| ipc::port_poll(objs, &p.handles, h))
            .map_err(|e| (e, None, None))?
            .map_err(|e| (e, None, None))
    };
    // Poll, and while nothing is ready and the deadline has not passed, sleep until the port
    // is READABLE and poll again (the readiness seen by the sleep check can be gone by the time
    // we poll, so one wait is not enough).
    let mut got = poll()?;
    while got.is_none()
        && deadline != 0
        && thread::ticks() < deadline
        && irq::without(killed_now).is_none()
    {
        let q = irq::without(|| state().signal_queue);
        // Ready, killed, or the handle stopped naming a port (the poll then says so).
        thread::wait_deadline(
            q,
            || {
                killed_now().is_some()
                    || signals_now(h).map_or(true, |s| s & abi::signals::READABLE != 0)
            },
            deadline,
        );
        got = poll()?;
    }
    match got {
        Some((key, observed)) => Ok((Some(key), Some(u64::from(observed)))),
        None => fail(Status::TimedOut),
    }
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn le64(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

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
        ipc::channel_write(objs, &mut p.handles, ch, &data, &handle_list(&raw))?;
        Ok(le32(&data, 0))
    })
    .map_err(|e| (e, None, None))?
    .map_err(|e| (e, None, None))?;
    notify();
    // Wait for the reply with that transaction id (or the peer closing, or the deadline).
    let q = irq::without(|| state().signal_queue);
    let ready = || {
        let s = state();
        let Some(&pid) = s.thread_pid.get(&thread::current()) else {
            return true;
        };
        let Some(p) = s.procs.get(&pid) else {
            return true;
        };
        if p.killed.is_some() {
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
    thread::wait_deadline(q, ready, deadline);
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
                let got = ipc::channel_read_txid(
                    objs,
                    &mut p.handles,
                    ch,
                    txid,
                    rd_max_bytes,
                    rd_max_handles,
                )
                .map_err(|_| (Status::BadState, None, None))?;
                let raw: Vec<u8> = got.handles.iter().flat_map(|h| h.0.to_le_bytes()).collect();
                let _ = copy_to_user(&mut p.aspace, &mut k.mm, &mut k.vmos, rd_bytes, &got.data);
                let _ = copy_to_user(&mut p.aspace, &mut k.mm, &mut k.vmos, rd_handles, &raw);
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
    out
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
            ipc::channel_write(&mut s.objs, &mut s.ktable, self.handle, data, &[])
        });
        notify();
        r
    }

    /// Take the oldest message (handles in it are closed: the kernel does not keep them).
    pub fn read(&self) -> Result<Vec<u8>, Status> {
        let r = irq::without(|| {
            let s = state();
            let got = ipc::channel_read(
                &mut s.objs,
                &mut s.ktable,
                self.handle,
                MAX_MSG_BYTES,
                MAX_MSG_HANDLES,
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
            let h = match ipc::vmo_object_create(&mut s.objs, &mut s.ktable, id) {
                Ok(h) => h,
                Err(e) => {
                    let _ = k.vmos.destroy(&mut k.mm, id);
                    return Err(e);
                }
            };
            let r = ipc::channel_write(&mut s.objs, &mut s.ktable, self.handle, data, &[h]);
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
            ipc::channel_write(&mut s.objs, &mut s.ktable, self.handle, data, handles)
        });
        notify();
        r
    }

    /// Wait until a message is readable or the peer has closed; false on timeout.
    pub fn wait_readable(&self, deadline: u64) -> bool {
        let q = irq::without(|| state().signal_queue);
        let h = self.handle;
        thread::wait_deadline(
            q,
            || {
                let s = state();
                ipc::signals_of(&s.objs, &s.ktable, h)
                    .map(|sig| sig & (abi::signals::READABLE | abi::signals::PEER_CLOSED) != 0)
                    .unwrap_or(true)
            },
            deadline,
        )
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
        let (kernel, user) = ipc::socket_create(&mut s.objs, &mut s.ktable)?;
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
        let h = match ipc::vmo_object_create(&mut s.objs, &mut s.ktable, id) {
            Ok(h) => h,
            Err(e) => {
                let _ = k.vmos.destroy(&mut k.mm, id);
                return Err(e);
            }
        };
        s.ktable
            .replace(
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
            ipc::socket_read(&mut s.objs, &s.ktable, self.handle, max)
        });
        notify();
        r
    }

    /// Bytes for the process to read; returns how many were accepted.
    pub fn write(&self, data: &[u8]) -> Result<usize, Status> {
        let r = irq::without(|| {
            let s = state();
            ipc::socket_write(&mut s.objs, &s.ktable, self.handle, data)
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

const PF_R: u64 = 1;
const PF_W: u64 = 2;
const PF_X: u64 = 4;

fn bad(s: Status) -> Reply {
    Err((s, None, None))
}

fn process_call(n: usize, a: [u64; 6]) -> Reply {
    // Handles are 32 bits wide everywhere (SYSCALLS.md); no truncation.
    let wide = |i: usize| a[i] > u64::from(u32::MAX);
    match n {
        nr::PROCESS_CREATE if wide(0) => bad(Status::BadHandle),
        nr::PROCESS_CREATE => process_create(a),
        nr::JOB_CREATE if wide(0) => bad(Status::BadHandle),
        nr::JOB_CREATE => job_create(a),
        nr::JOB_SET_LIMITS | nr::JOB_INFO | nr::JOB_KILL if wide(0) => bad(Status::BadHandle),
        nr::JOB_SET_LIMITS => job_set_limits(a),
        nr::JOB_INFO => job_info(a),
        nr::JOB_KILL => job_kill(a),
        nr::JOB_ROOT => job_root(),
        nr::PROCESS_SELF => process_self(),
        nr::PROCESS_MAP if wide(0) || wide(1) => bad(Status::BadHandle),
        nr::PROCESS_MAP => process_map(a),
        nr::PROCESS_UNMAP if wide(0) => bad(Status::BadHandle),
        nr::PROCESS_UNMAP => process_unmap(a),
        nr::PROCESS_START if wide(0) || wide(3) => bad(Status::BadHandle),
        nr::PROCESS_START => process_start(a),
        nr::PROCESS_KILL | nr::PROCESS_INFO if wide(0) => bad(Status::BadHandle),
        nr::PROCESS_KILL => process_kill(a),
        nr::PROCESS_INFO => process_info(a),
        nr::THREAD_CREATE if wide(0) => bad(Status::BadHandle),
        nr::THREAD_CREATE => thread_create(a),
        nr::THREAD_INFO if wide(0) => bad(Status::BadHandle),
        nr::THREAD_INFO => thread_info(a),
        _ => bad(Status::BadSyscall),
    }
}

fn caller_pid(s: &UserState) -> Result<u32, Status> {
    s.thread_pid
        .get(&thread::current())
        .copied()
        .ok_or(Status::BadState)
}

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
        let pid = s.next_pid;
        s.next_pid += 1;
        let UserState { objs, procs, .. } = s;
        let creator = procs.get_mut(&me).ok_or((Status::BadState, None, None))?;
        let (handle, obj) = match ipc::process_object_create(objs, &mut creator.handles, pid, true)
        {
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
                threads_alive: 0,
                thread_objs: BTreeMap::new(),
                job,
                table_charge: BTreeMap::from([(u64::MAX, 1)]),
                privileged: false,
            },
        );
        Ok((Some(u64::from(handle.0)), None))
    })
}

fn process_self() -> Reply {
    irq::without(|| -> Reply {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let UserState { objs, procs, .. } = s;
        let p = procs.get_mut(&me).ok_or((Status::BadState, None, None))?;
        let rights =
            Rights(Rights::MANAGE.0 | Rights::INSPECT.0 | Rights::WAIT.0 | Rights::DUPLICATE.0);
        if p.handles.room() < 1 {
            return bad(Status::NoMemory);
        }
        match p.obj.filter(|&o| objs.is_alive(o)) {
            Some(obj) => {
                // another view of the same process object
                objs.retain(obj);
                let h = p.handles.insert(obj, rights).expect("room was checked");
                Ok((Some(u64::from(h.0)), None))
            }
            None => {
                let (h, obj) = ipc::process_object_create(objs, &mut p.handles, me, false)
                    .map_err(|e| (e, None, None))?;
                // narrow the default rights: a self handle cannot be sent away
                let h = p.handles.replace(h, rights).expect("just created");
                p.obj = Some(obj);
                Ok((Some(u64::from(h.0)), None))
            }
        }
    })
}

/// Resolve a process handle and a VMO handle of the caller, checking them in the order
/// SYSCALLS.md lists (BAD_HANDLE, WRONG_TYPE, ACCESS_DENIED). Returns the target pid and
/// the VMO id.
fn resolve_map_args(
    s: &UserState,
    me: u32,
    ph: Handle,
    vh: Handle,
    flags: u64,
) -> Result<(u32, u64), Status> {
    let caller = s.procs.get(&me).ok_or(Status::BadState)?;
    let pe = caller.handles.get(ph)?;
    let ve = caller.handles.get(vh)?;
    let tpid = match s.objs.get(pe.obj) {
        Some(ipc::KObj::Process(p)) => p.pid,
        _ => return Err(Status::WrongType),
    };
    let vid = match s.objs.get(ve.obj) {
        Some(ipc::KObj::Vmo(id)) => *id,
        _ => return Err(Status::WrongType),
    };
    let mut need = Rights(Rights::MAP.0 | Rights::READ.0);
    if flags & PF_W != 0 {
        need = need.union(Rights::WRITE);
    }
    if flags & PF_X != 0 {
        need = need.union(Rights::EXECUTE);
    }
    if !pe.rights.contains(Rights::MANAGE) || !ve.rights.contains(need) {
        return Err(Status::AccessDenied);
    }
    Ok((tpid, vid))
}

fn process_map(a: [u64; 6]) -> Reply {
    let (ph, vh) = (Handle(a[0] as u32), Handle(a[1] as u32));
    let (vaddr, offset, length, flags) = (a[2], a[3], a[4], a[5]);
    irq::without(|| -> Reply {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let (tpid, vid) = resolve_map_args(s, me, ph, vh, flags).map_err(|e| (e, None, None))?;
        let k = kmem::get();
        let target = s
            .procs
            .get_mut(&tpid)
            .ok_or((Status::BadState, None, None))?;
        if target.exit_code.is_some() || target.killed.is_some() {
            return bad(Status::BadState);
        }
        let aligned = |x: u64| x.is_multiple_of(4096);
        let in_user = vaddr >= abi::user::BASE
            && vaddr
                .checked_add(length)
                .is_some_and(|end| end <= abi::user::TOP);
        let vmo_pages = k.vmos.pages(vid).ok_or((Status::BadHandle, None, None))?;
        let inside_vmo = offset
            .checked_add(length)
            .is_some_and(|end| end <= vmo_pages * 4096);
        if !aligned(vaddr)
            || !aligned(offset)
            || !aligned(length)
            || length == 0
            || flags & !(PF_R | PF_W | PF_X) != 0
            || (flags & PF_W != 0 && flags & PF_X != 0)
            || !in_user
            || !inside_vmo
        {
            return bad(Status::InvalidArgs);
        }
        let perms = Perms {
            read: true,
            write: flags & PF_W != 0,
            exec: flags & PF_X != 0,
            user: true,
        };
        // Page tables for this region, worst case: one leaf table per 512 pages it spans (plus
        // one for each end not aligned to them) and two interior ones.
        let tables = length / 4096 / 512 + 4;
        k.vmos
            .jobs
            .charge(target.job, Resource::Pages, tables)
            .map_err(|e| (job_status(e), None, None))?;
        if let Err(e) =
            target
                .aspace
                .map_region(&k.vmos, vaddr, length / 4096, vid, offset / 4096, perms)
        {
            k.vmos.jobs.credit(target.job, Resource::Pages, tables);
            return Err(match e {
                kcore::aspace::RegionError::TooMany => (Status::NoMemory, None, None),
                _ => (Status::InvalidArgs, None, None),
            });
        }
        target.table_charge.insert(vaddr, tables);
        Ok((None, None))
    })
}

fn process_unmap(a: [u64; 6]) -> Reply {
    let ph = Handle(a[0] as u32);
    irq::without(|| -> Reply {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let caller = s.procs.get(&me).ok_or((Status::BadState, None, None))?;
        let (_, tpid, _, _) = ipc::process_of(&s.objs, &caller.handles, ph, Rights::MANAGE)
            .map_err(|e| (e, None, None))?;
        let k = kmem::get();
        let target = s
            .procs
            .get_mut(&tpid)
            .ok_or((Status::BadState, None, None))?;
        if target.exit_code.is_some() || target.killed.is_some() {
            return bad(Status::BadState);
        }
        target
            .aspace
            .unmap_region(&mut k.mm, a[1])
            .map_err(|_| (Status::InvalidArgs, None, None))?;
        if let Some(tables) = target.table_charge.remove(&a[1]) {
            k.vmos.jobs.credit(target.job, Resource::Pages, tables);
        }
        kmem::flush_tlb();
        Ok((None, None))
    })
    .inspect(|_| {
        REAP_DIRTY.store(true, Ordering::SeqCst);
        reap_vmos();
    })
}

fn process_start(a: [u64; 6]) -> Reply {
    let (ph, bh) = (Handle(a[0] as u32), Handle(a[3] as u32));
    let (entry, sp, arg) = (a[1], a[2], a[4]);
    let started = irq::without(|| -> Result<usize, (Status, Option<u64>, Option<u64>)> {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let caller = s.procs.get(&me).ok_or((Status::BadState, None, None))?;
        let pe = caller.handles.get(ph).map_err(|e| (e, None, None))?;
        let be = caller.handles.get(bh).map_err(|e| (e, None, None))?;
        let tpid = match s.objs.get(pe.obj) {
            Some(ipc::KObj::Process(p)) => p.pid,
            _ => return bad(Status::WrongType).map(|_| 0),
        };
        if !matches!(s.objs.get(be.obj), Some(ipc::KObj::Channel(_))) {
            return bad(Status::WrongType).map(|_| 0);
        }
        if !pe.rights.contains(Rights::MANAGE) || !be.rights.contains(Rights::TRANSFER) {
            return bad(Status::AccessDenied).map(|_| 0);
        }
        let target = s.procs.get(&tpid).ok_or((Status::BadState, None, None))?;
        if target.state != ProcState::Created
            || target.exit_code.is_some()
            || target.killed.is_some()
        {
            return bad(Status::BadState).map(|_| 0);
        }
        if total_threads(s) >= abi::limits::MAX_USER_THREADS {
            return bad(Status::NoMemory).map(|_| 0);
        }
        let entry_ok = target.aspace.region_perms_at(entry).is_some_and(|p| p.exec);
        let sp_ok = sp.is_multiple_of(16) && (abi::user::BASE..=abi::user::TOP).contains(&sp);
        if !entry_ok || !sp_ok {
            return bad(Status::InvalidArgs).map(|_| 0);
        }
        // The thread's charge comes before anything changes: a refusal must leave the caller's
        // handle with the caller and the process exactly as it was (SYSCALLS.md: on any error
        // nothing changes), or a retry would find handle 1 taken.
        kmem::get()
            .vmos
            .jobs
            .charge(target.job, Resource::Threads, 1)
            .map_err(|e| (job_status(e), None, None))?;
        // Commit: the channel end moves into the new process as its handle 1.
        let UserState { procs, .. } = s;
        let caller = procs.get_mut(&me).expect("caller exists");
        let moved = caller
            .handles
            .remove_for_transfer(bh)
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
        Ok(new_start(s, tpid, entry, sp, arg, None))
    })?;
    // The new thread (registered by itself once it runs). Spawned after the state borrow is
    // gone: spawning can switch to it at once.
    thread::spawn_detached("user", 10, user_thread_main, started);
    Ok((None, None))
}

fn process_kill(a: [u64; 6]) -> Reply {
    let ph = Handle(a[0] as u32);
    let code = a[1] as i32;
    let (tpid, me) = irq::without(|| -> Result<(u32, u32), ReplyErr> {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let caller = s.procs.get(&me).ok_or((Status::BadState, None, None))?;
        let (_, tpid, terminated, _) = ipc::process_of(&s.objs, &caller.handles, ph, Rights::KILL)
            .map_err(|e| (e, None, None))?;
        if terminated || !s.procs.contains_key(&tpid) {
            return Err((Status::BadState, None, None));
        }
        Ok((tpid, me))
    })?;
    if tpid == me {
        exit_process(code);
    }
    request_kill(tpid, code);
    Ok((None, None))
}

fn process_info(a: [u64; 6]) -> Reply {
    let ph = Handle(a[0] as u32);
    irq::without(|| -> Reply {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let caller = s.procs.get(&me).ok_or((Status::BadState, None, None))?;
        let (_, _, terminated, code) =
            ipc::process_of(&s.objs, &caller.handles, ph, Rights::INSPECT)
                .map_err(|e| (e, None, None))?;
        if !terminated {
            return bad(Status::BadState);
        }
        Ok((Some(code as i64 as u64), None))
    })
}

/// User threads alive in the whole system.
fn total_threads(s: &UserState) -> usize {
    s.procs.values().map(|p| p.threads_alive).sum()
}

fn thread_create(a: [u64; 6]) -> Reply {
    let ph = Handle(a[0] as u32);
    let (entry, sp, arg) = (a[1], a[2], a[3]);
    let (token, handle) = irq::without(|| -> Result<(usize, u32), ReplyErr> {
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
            return bad(Status::BadState).map(|_| (0, 0));
        }
        let entry_ok = target.aspace.region_perms_at(entry).is_some_and(|p| p.exec);
        let sp_ok = sp.is_multiple_of(16) && (abi::user::BASE..=abi::user::TOP).contains(&sp);
        if !entry_ok || !sp_ok {
            return bad(Status::InvalidArgs).map(|_| (0, 0));
        }
        if target.threads_alive >= abi::limits::MAX_THREADS_PER_PROCESS
            || total_threads(s) >= abi::limits::MAX_USER_THREADS
        {
            return bad(Status::NoMemory).map(|_| (0, 0));
        }
        let tjob = target.job;
        let UserState { objs, procs, .. } = s;
        let jobs = &mut kmem::get().vmos.jobs;
        jobs.charge(tjob, Resource::Threads, 1)
            .map_err(|e| (job_status(e), None, None))?;
        let caller = procs.get_mut(&me).ok_or((Status::BadState, None, None))?;
        let (handle, obj) = match ipc::thread_object_create(objs, &mut caller.handles, tpid) {
            Ok(x) => x,
            Err(e) => {
                jobs.credit(tjob, Resource::Threads, 1);
                return Err((e, None, None));
            }
        };
        procs.get_mut(&tpid).expect("target exists").threads_alive += 1;
        Ok((new_start(s, tpid, entry, sp, arg, Some(obj)), handle.0))
    })?;
    // Spawned after the state borrow is gone: spawning can switch to the new thread at once.
    thread::spawn_detached("user", 10, user_thread_main, token);
    Ok((Some(u64::from(handle)), None))
}

fn thread_info(a: [u64; 6]) -> Reply {
    let th = Handle(a[0] as u32);
    irq::without(|| -> Reply {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let caller = s.procs.get(&me).ok_or((Status::BadState, None, None))?;
        let (terminated, code) = ipc::thread_of(&s.objs, &caller.handles, th, Rights::INSPECT)
            .map_err(|e| (e, None, None))?;
        if !terminated {
            return bad(Status::BadState);
        }
        Ok((Some(code as i64 as u64), None))
    })
}

// ---- jobs (S4.5, OD56-OD58) ----

/// What a failed ledger call looks like to a program: running into a limit is "out of memory"
/// (the one resource-exhausted status), a job that is gone is a bad state.
fn job_status(e: JobError) -> Status {
    match e {
        JobError::Exceeded | JobError::TooManyJobs => Status::NoMemory,
        JobError::NoSuchJob => Status::BadState,
    }
}

/// Limits from three syscall words (`u64::MAX` means no limit).
fn limits_of(a: [u64; 6], at: usize) -> Limits {
    Limits {
        processes: a[at],
        threads: a[at + 1],
        pages: a[at + 2],
        jobs: a[at + 3],
    }
}

/// `job_create(parent, processes, threads, pages) -> handle`: a child job of the job `parent`
/// (needs MANAGE). The child can never use more than any ancestor allows, whatever its own
/// limits say.
fn job_create(a: [u64; 6]) -> Reply {
    irq::without(|| -> Reply {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let UserState { objs, procs, .. } = s;
        let caller = procs.get_mut(&me).ok_or((Status::BadState, None, None))?;
        let parent = ipc::job_of(objs, &caller.handles, Handle(a[0] as u32), Rights::MANAGE)
            .map_err(|e| (e, None, None))?;
        let jobs = &mut kmem::get().vmos.jobs;
        let id = jobs
            .create(parent, limits_of(a, 1))
            .map_err(|e| (job_status(e), None, None))?;
        match ipc::job_object_create(objs, &mut caller.handles, id, Rights::JOB) {
            Ok(h) => Ok((Some(u64::from(h.0)), None)),
            Err(e) => {
                jobs.release(id);
                Err((e, None, None))
            }
        }
    })
}

/// `job_set_limits(job, processes, threads, pages)`: needs MANAGE. Usage above a new limit is
/// not undone: later charges fail until it is back under.
fn job_set_limits(a: [u64; 6]) -> Reply {
    irq::without(|| -> Reply {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let caller = s.procs.get(&me).ok_or((Status::BadState, None, None))?;
        let job = ipc::job_of(
            &s.objs,
            &caller.handles,
            Handle(a[0] as u32),
            Rights::MANAGE,
        )
        .map_err(|e| (e, None, None))?;
        kmem::get()
            .vmos
            .jobs
            .set_limits(job, limits_of(a, 1))
            .map_err(|e| (job_status(e), None, None))?;
        Ok((None, None))
    })
}

/// `job_info(job, which) -> used, limit` for `which` 0 processes, 1 threads, 2 pages (everything
/// in the job and below it). Needs INSPECT.
fn job_info(a: [u64; 6]) -> Reply {
    irq::without(|| -> Reply {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let caller = s.procs.get(&me).ok_or((Status::BadState, None, None))?;
        let job = ipc::job_of(
            &s.objs,
            &caller.handles,
            Handle(a[0] as u32),
            Rights::INSPECT,
        )
        .map_err(|e| (e, None, None))?;
        let res = match a[1] {
            0 => Resource::Processes,
            1 => Resource::Threads,
            2 => Resource::Pages,
            3 => Resource::Jobs,
            _ => return bad(Status::InvalidArgs),
        };
        let jobs = &kmem::get().vmos.jobs;
        let used = jobs
            .usage(job)
            .ok_or((Status::BadState, None, None))?
            .get(res);
        let limits = jobs.limits(job).ok_or((Status::BadState, None, None))?;
        let limit = match res {
            Resource::Processes => limits.processes,
            Resource::Threads => limits.threads,
            Resource::Pages => limits.pages,
            Resource::Jobs => limits.jobs,
        };
        Ok((Some(used), Some(limit)))
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
        let caller = s.procs.get(&me).ok_or((Status::BadState, None, None))?;
        let job = ipc::job_of(&s.objs, &caller.handles, Handle(a[0] as u32), Rights::KILL)
            .map_err(|e| (e, None, None))?;
        let jobs = &mut kmem::get().vmos.jobs;
        let mut limits = jobs.limits(job).ok_or((Status::BadState, None, None))?;
        limits.processes = 0;
        limits.threads = 0;
        let _ = jobs.set_limits(job, limits);
        let inside: Vec<u32> = s
            .procs
            .iter()
            .filter(|(_, p)| jobs.is_within(p.job, job))
            .map(|(&pid, _)| pid)
            .collect();
        let me_inside = inside.contains(&me);
        Ok((
            inside.into_iter().filter(|&p| p != me).collect(),
            me_inside,
            me,
        ))
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

/// `job_root() -> handle`: only a process the kernel started (`init`) may ask; it gets a handle
/// with every job right to the root job. Everything else a program holds is derived from this one
/// (or from a job its parent gave it), so no program can name a job it was not given.
fn job_root() -> Reply {
    irq::without(|| -> Reply {
        let s = state();
        let me = caller_pid(s).map_err(|e| (e, None, None))?;
        let UserState { objs, procs, .. } = s;
        let caller = procs.get_mut(&me).ok_or((Status::BadState, None, None))?;
        if !caller.privileged {
            return bad(Status::AccessDenied);
        }
        let jobs = &mut kmem::get().vmos.jobs;
        jobs.retain(JobId::ROOT);
        match ipc::job_object_create(objs, &mut caller.handles, JobId::ROOT, Rights::JOB) {
            Ok(h) => Ok((Some(u64::from(h.0)), None)),
            Err(e) => {
                jobs.release(JobId::ROOT);
                Err((e, None, None))
            }
        }
    })
}
