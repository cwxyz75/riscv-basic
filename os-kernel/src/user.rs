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
use core::sync::atomic::{AtomicU64, Ordering};
use kcore::aspace::{Access, AddressSpace};
use kcore::elf;
use kcore::handle::HandleTable;
use kcore::ipc::{self, Kobjs, ReadError};
use kcore::pagetable::{PageTable, Perms};
use kcore::sched::Tid;
use kcore::usercopy::{copy_from_user, copy_to_user, verify_writable};

/// The embedded user programs (see build.rs); empty if they were not built.
pub static INIT_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/init.elf"));
pub static CRASH_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/crash.elf"));

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
}

struct UserState {
    objs: Kobjs,
    procs: BTreeMap<u32, Process>,
    thread_pid: BTreeMap<Tid, u32>,
    next_pid: u32,
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
    irq::without(|| state().procs.insert(pid, process));
    let tid = thread::spawn("user", 10, user_thread_main, pid as usize);
    Ok((pid, tid))
}

fn user_thread_main(pid: usize) {
    let (entry, arg, satp) = irq::without(|| {
        let s = state();
        // Registered here, by the thread itself, before it can make a syscall: the spawner
        // cannot be preempted into leaving a window where the thread is unknown.
        s.thread_pid.insert(thread::current(), pid as u32);
        let p = &s.procs[&(pid as u32)];
        (p.entry, p.arg, satp_of(p))
    });
    let top = thread::current_stack_top();
    // SAFETY: `enter_user` switches to the process's address space and drops to U-mode at
    // its entry point with the stack and argument chosen above; it never returns.
    unsafe { enter_user(entry, STACK_TOP, arg, satp, top) }
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
    // Back to the kernel's own page table before the process's root is freed.
    kmem::use_kernel_table();
    let mut process = irq::without(|| {
        let s = state();
        s.thread_pid.remove(&tid);
        s.procs.remove(&pid).expect("process exists")
    });
    // A process that vanished without a recorded code (cannot happen today) is not mistaken
    // for `exit(-1)`.
    let code = process.exit_code.unwrap_or(i32::MIN);
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
        if !s.line.is_empty() {
            let line = core::mem::take(&mut s.line);
            let _ = writeln!(Console, "{line}");
        }
    });
    let k = kmem::get();
    process.aspace.destroy_user(&mut k.mm);
    for id in process.vmos {
        let _ = k.vmos.destroy(&mut k.mm, id);
    }
    kmem::flush_tlb();
    notify(); // peers of what this process held now see PEER_CLOSED
    code
}

/// End the calling user thread's process with `code`.
fn exit_process(code: i32) -> ! {
    irq::without(|| {
        let s = state();
        if let Some(&pid) = s.thread_pid.get(&thread::current()) {
            if let Some(p) = s.procs.get_mut(&pid) {
                p.exit_code = Some(code);
            }
        }
    });
    thread::exit()
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
        nr::CLOCK_GET => frame.set_return(0, Some(thread::ticks()), None),
        nr::OBJECT_WAIT => reply(frame, object_wait(a)),
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
                    | nr::EVENT_SIGNAL
                    // creating objects can sweep garbage, which closes peers
                    | nr::CHANNEL_CREATE
                    | nr::EVENT_CREATE
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

type Reply = Result<(Option<u64>, Option<u64>), (Status, Option<u64>, Option<u64>)>;

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
    if matches!(n, nr::CHANNEL_CREATE | nr::EVENT_CREATE) && s.objs.needs_sweep() {
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
        nr::HANDLE_CLOSE | nr::OBJECT_SIGNALS | nr::CHANNEL_WRITE if over(0) => {
            Some(Status::BadHandle)
        }
        nr::CHANNEL_READ | nr::CHANNEL_READ_TXID if over(0) => Some(Status::BadHandle),
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
        nr::EVENT_CREATE => {
            let e = ipc::event_create(objs, &mut p.handles).map_err(|e| (e, None, None))?;
            Ok((Some(u64::from(e.0)), None))
        }
        nr::EVENT_SIGNAL => ipc::event_signal(objs, &p.handles, h(0), a[1] as u32, a[2] as u32)
            .map(|_| (None, None))
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
                let _ = writeln!(Console, "user: {text}");
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
    let q = irq::without(|| state().signal_queue);
    thread::wake_all(q);
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

const WAITABLE_SIGNALS: u32 = abi::signals::READABLE
    | abi::signals::WRITABLE
    | abi::signals::PEER_CLOSED
    | abi::signals::USER_MASK;

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
            || signals_now(h).map_or(true, |s| s & mask != 0),
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
