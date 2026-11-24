//! The state of user mode (TS3): the kernel objects, the processes, the threads' starts, the wait tables, and the kernel's own handles. The
//! syscall functions that act on it take `&mut UserState` and what the machine provides, so they run on the host.

use crate::proc::SpawnStack;
use alloc::string::String;
use alloc::vec::Vec;
use kcore::handle::HandleTable;
use kcore::ipc::Kobjs;
use kcore::sched::Tid;

/// What a new user thread needs to start (looked up by the thread itself: a thread's start argument is one word).
pub struct ThreadStart {
    pub pid: u64,
    pub entry: u64,
    pub sp: u64,
    pub arg: u64,
    pub obj: Option<kcore::object::ObjId>,
    /// `thread_spawn` extras: the `tp` register, the stack the kernel provisioned, round robin, and whether `a1` is the tid.
    pub tp: u64,
    pub stack: Option<SpawnStack>,
    pub rr: bool,
    pub spawned: bool,
    /// Made with `spawn::SUSPENDED` (EX1): the spawner builds its first register image and it runs only after `thread_resume`.
    pub suspended: bool,
    /// The value of register `a2` at the thread's first instruction: the base of the system library (AB7) for the first thread of a process, else 0.
    pub a2: u64,
}

/// Everything the kernel keeps about one user thread (KT9, T177). Made by the spawner (a reserved insert), dropped when the thread ends.
#[derive(Default)]
pub struct UThread {
    pub pid: u64,
    /// The thread's object (none for a detached thread), to mark it TERMINATED when it ends.
    pub obj: Option<kcore::object::ObjId>,
    /// The `channel_call` the thread is waiting in, by call number (L22): set when the request is sent, cleared when the call ends. A server that
    /// reads a request is lent the caller's priority only while this still says the caller waits in **that** call.
    pub call: Option<u64>,
    /// Scheduling flags (bit 0: round robin) set by `thread_set_sched`.
    pub sched_flags: u8,
    /// The suspend state (EX1): present exactly while the thread is suspended or stopped by it.
    pub suspend: Option<kcore::suspend::Suspend>,
    /// The GENERAL register image (`abi::thread_state`) of a thread that has not entered user mode yet (EX1): `thread_read_state` and
    /// `thread_write_state` act on it, and the thread starts from it. Made by the spawner.
    pub initial_regs: Option<kcore::fbox::FBox<[u64; 32]>>,
    /// Register writes for a thread parked at the return to user mode, applied by the thread itself to its own frame when it wakes (EX1).
    pub pending_regs: Option<kcore::fbox::FBox<PendingRegs>>,
    /// The exception serial the thread waits for, if it is stopped for one (EX2).
    pub exc_pending: Option<u64>,
    /// Asleep in the park at the return to user mode, so that a suspend that finds it there knows it is not blocked in a call.
    pub parked: bool,
    /// A pending `thread_interrupt`: the next blocking wait that would sleep returns INTERRUPTED and takes the flag.
    pub interrupt: bool,
    /// Inside `cleanup_dead` (a per-thread recursion guard, T63).
    pub cleaning: bool,
}

/// What `thread_write_state` leaves for a parked thread to apply to its own frame when it wakes.
#[derive(Default, Clone, Copy)]
pub struct PendingRegs {
    /// A new GENERAL image.
    pub general: Option<[u64; 32]>,
    /// FP state was written: the frame's FS says Initial so the return loads it.
    pub fp: bool,
    /// Vector state was written.
    pub vec: bool,
}

/// Everything the syscall layer keeps about user mode: the kernel objects, the processes and threads, the wait tables, and the kernel's own handles.
pub struct UserState {
    /// The system-wide ceilings (PL8): processes and user threads are checked against them, the object and IPC ones live in the registry.
    pub backstops: kcore::limits::Backstops,
    pub objs: Kobjs,
    pub procs: crate::ProcTable,
    /// Vacant address-space cells, made at boot (KT9, T177: an `Arc` cannot be made fallibly, so `process_create` arms one of these and teardown gives it
    /// back). Its capacity never changes after `fill_as_pool`.
    pub as_pool: Vec<alloc::sync::Arc<klock::aslock::AsCell>>,
    /// The next call number.
    pub next_call: u64,
    pub next_pid: u64,
    /// The address-space identifier allocator (L18): every process holds a tag from it.
    pub asids: kcore::asid::Asids,
    /// The generation new handle tables start their slots at (0; `ff=handlegen:` in test builds).
    pub first_handle_gen: u64,
    /// Pagers and the threads waiting for their pages (MM9).
    pub pagers: kcore::pager::Pagers,
    /// The kernel's end (a handle in `ktable`) of each pager's request channel, by pager id.
    pub pager_ends: kcore::fmap::FMap<u64, abi::Handle>,
    /// The next pager id.
    pub next_pager: u64,
    /// The kernel's end of each job's exception channel (EX2; handles in `ktable`).
    pub job_exc: kcore::fmap::FMap<kcore::job::JobId, abi::Handle>,
    /// The next exception serial.
    pub next_exc: u64,
    /// Thread starts waiting for their thread to run, by token.
    pub starts: kcore::fmap::FMap<usize, ThreadStart>,
    /// One record per user thread, from the moment its spawner registers it until it ends (KT9, T177: everything a thread's later life
    /// writes is a field here, so nothing after the spawn can fail for want of memory).
    pub threads: kcore::fmap::FMap<Tid, UThread>,
    /// The kernel threads (no record in `threads`) that are inside `cleanup_dead`.
    pub kernel_cleaning: kcore::fmap::FSet<Tid>,
    /// How many threads have `suspend`, `interrupt` and `pending_regs` set (the lock-free probes read these counts).
    pub n_suspended: usize,
    pub n_interrupts: usize,
    pub n_pending_regs: usize,
    pub next_token: usize,
    /// Handles the kernel itself holds (the creator's ends of bootstrap channels).
    pub ktable: HandleTable,
    /// The processes whose handle tables a running sweep has yet to shade (T206): taken from the end, the kernel's own table last.
    pub root_scan: Vec<u64>,
    /// The slot of the process table (the last of `root_scan`, or the kernel's own) a root scan has reached (T206).
    pub root_cursor: usize,
    /// Threads waiting on an object's signals (slice W step 3): woken by id when a touched object's
    /// signals match, instead of by the global queue.
    pub waits: kcore::wait::WaitTable,
    /// The interrupt object that owns each interrupt source (S6a-2).
    /// The interrupt object that owns each source, by source number: sized once at boot to the controller's source count (`with_irq_slots`;
    /// empty without a controller) and never grown, so no syscall allocates here.
    pub irq_objs: alloc::vec::Vec<Option<kcore::object::ObjId>>,
    /// Output of user programs, assembled into lines.
    pub line: String,
    pub pass: u32,
    pub fail: u32,
}

impl UserState {
    /// Make `n` vacant address-space cells for `process_create` (boot, and the host tests' worlds: an allocation failure there is a failure to start).
    pub fn fill_as_pool(&mut self, n: usize) {
        self.as_pool
            .reserve_exact(n.saturating_sub(self.as_pool.len()));
        while self.as_pool.len() < n {
            self.as_pool
                .push(alloc::sync::Arc::new(klock::aslock::AsCell::vacant()));
        }
    }

    /// Give a cell back to the pool (its space was taken out: `AsCell::vacate`). The pool's capacity was made by `fill_as_pool`, so this allocates nothing.
    pub fn return_as_cell(&mut self, cell: alloc::sync::Arc<klock::aslock::AsCell>) {
        if self.as_pool.len() < self.as_pool.capacity() {
            self.as_pool.push(cell);
        }
    }

    /// Start a sweep with its roots to be shaded a process table per slice (T206; `sweep_slice` does it): false if one runs.
    pub fn sweep_start(&mut self) -> bool {
        // (T177: the room first; with none, no sweep starts, which is safe: it only means garbage floats until the next one)
        let mut scan: Vec<u64> = Vec::new();
        if scan.try_reserve_exact(self.procs.len()).is_err() {
            return false;
        }
        if !self.objs.sweep_begin_pending() {
            return false;
        }
        scan.extend(self.procs.keys().rev().copied());
        self.root_scan = scan;
        self.root_cursor = 0;
        true
    }

    /// One slice of the running sweep: while roots are pending, shade the handles of the next process (a process that has gone since has none), then the
    /// kernel's own; after that, `Registry::sweep_step`. A handle made or moved while this runs shades its object (`HandleTable::insert`,
    /// `remove_for_transfer`).
    pub fn sweep_slice(&mut self, budget: usize) -> kcore::object::SweepStep {
        // (T206: references an earlier slice left owed are paid first, a bounded number per slice)
        if !self.objs.release_step(kcore::object::RELEASE_UNITS) {
            return kcore::object::SweepStep::More;
        }
        if self.objs.sweep_active() && self.objs.roots_are_pending() {
            // (T206: at most `budget` slots of one table per slice, the cursor kept between slices; a table with thousands of slots is not one section)
            let from = self.root_cursor;
            match self.root_scan.last().copied() {
                Some(pid) => {
                    let len = self.procs.get(&pid).map_or(0, |q| q.handles.slot_count());
                    let to = len.min(from.saturating_add(budget));
                    if let Some(q) = self.procs.get(&pid) {
                        for o in q.handles.objects_in(from, to) {
                            self.objs.shade(o);
                        }
                    }
                    if to >= len {
                        self.root_scan.pop();
                        self.root_cursor = 0;
                    } else {
                        self.root_cursor = to;
                    }
                }
                None => {
                    let len = self.ktable.slot_count();
                    let to = len.min(from.saturating_add(budget));
                    for o in self.ktable.objects_in(from, to) {
                        self.objs.shade(o);
                    }
                    if to >= len {
                        self.root_cursor = 0;
                        self.objs.sweep_roots_done();
                    } else {
                        self.root_cursor = to;
                    }
                }
            }
            return kcore::object::SweepStep::More;
        }
        self.objs.sweep_step(budget)
    }

    /// Every object something outside the registry can still reach: all processes' handles
    /// **and the kernel's own** (the creator ends of bootstrap channels). Any sweep must use
    /// all of these (a root left out is destroyed, whatever still points at it).
    pub fn roots(&self) -> Vec<kcore::object::ObjId> {
        self.procs
            .values()
            .flat_map(|q| q.handles.objects())
            .chain(self.ktable.objects())
            .collect()
    }
}

impl UserState {
    /// A state with room for `slots` interrupt sources (the controller's source count plus one; 0 without a controller).
    pub fn with_irq_slots(slots: usize) -> UserState {
        let mut s = UserState::new();
        s.irq_objs = alloc::vec![None; slots];
        s
    }

    pub fn new() -> UserState {
        let mut s = UserState {
            objs: Kobjs::new(),
            procs: kcore::fmap::FMap::new(),
            as_pool: Vec::new(),
            next_call: 1,
            next_pid: 1,
            asids: kcore::asid::Asids::new(16),
            first_handle_gen: 0,
            root_scan: Vec::new(),
            root_cursor: 0,
            job_exc: kcore::fmap::FMap::new(),
            pagers: kcore::pager::Pagers::new(),
            pager_ends: kcore::fmap::FMap::new(),
            next_pager: 1,
            next_exc: 1,
            starts: kcore::fmap::FMap::new(),
            threads: kcore::fmap::FMap::new(),
            kernel_cleaning: kcore::fmap::FSet::new(),
            n_suspended: 0,
            n_interrupts: 0,
            n_pending_regs: 0,
            next_token: 1,
            // (one end of each process's and job's exception channel lives here, and the creators' ends of bootstrap channels: bounded by the
            // object cap, whose charges the objects themselves carry)
            ktable: HandleTable::new(abi::limits::MAX_OBJECTS),
            backstops: kcore::limits::Backstops::DEFAULT,
            waits: kcore::wait::WaitTable::new(),
            irq_objs: alloc::vec::Vec::new(),
            line: String::new(),
            pass: 0,
            fail: 0,
        };
        // (the host tests' worlds and the kernel until it knows its RAM: a few vacant cells; the kernel refills to its process backstop)
        s.fill_as_pool(16);
        s
    }
}

impl UserState {
    /// The process a user thread belongs to (none: not a registered user thread).
    pub fn pid_of(&self, tid: Tid) -> Option<u64> {
        self.threads.get(&tid).map(|t| t.pid)
    }

    /// The record of a registered user thread.
    pub fn thread(&self, tid: Tid) -> Option<&UThread> {
        self.threads.get(&tid)
    }

    pub fn thread_mut(&mut self, tid: Tid) -> Option<&mut UThread> {
        self.threads.get_mut(&tid)
    }

    /// The thread's suspend state, if it has one.
    pub fn suspend_of(&self, tid: Tid) -> Option<&kcore::suspend::Suspend> {
        self.threads.get(&tid).and_then(|t| t.suspend.as_ref())
    }

    pub fn suspend_of_mut(&mut self, tid: Tid) -> Option<&mut kcore::suspend::Suspend> {
        self.threads.get_mut(&tid).and_then(|t| t.suspend.as_mut())
    }

    /// The thread's suspend state, made if it has none (the thread must be registered: false otherwise).
    pub fn suspend_entry(&mut self, tid: Tid) -> Option<&mut kcore::suspend::Suspend> {
        let t = self.threads.get_mut(&tid)?;
        if t.suspend.is_none() {
            self.n_suspended = self.n_suspended.saturating_add(1);
        }
        Some(t.suspend.get_or_insert_with(Default::default))
    }

    /// The thread begins `cleanup_dead`: false if it is already inside one (the guard against recursing into itself, T63). A kernel thread has no
    /// record, so its flag is a set entry; if that cannot be made the cleanup runs unguarded (it only guards a recursion).
    pub fn begin_cleaning(&mut self, tid: Tid) -> bool {
        if let Some(t) = self.threads.get_mut(&tid) {
            return !core::mem::replace(&mut t.cleaning, true);
        }
        self.kernel_cleaning.try_insert(tid).unwrap_or(true)
    }

    pub fn end_cleaning(&mut self, tid: Tid) {
        if let Some(t) = self.threads.get_mut(&tid) {
            t.cleaning = false;
        } else {
            self.kernel_cleaning.remove(&tid);
        }
    }

    /// Drop the thread's suspend state.
    pub fn suspend_remove(&mut self, tid: Tid) {
        if let Some(t) = self.threads.get_mut(&tid) {
            if t.suspend.take().is_some() {
                self.n_suspended = self.n_suspended.saturating_sub(1);
            }
        }
    }

    /// Set or clear the thread's pending-interrupt flag; true if it was set.
    pub fn set_interrupt(&mut self, tid: Tid, on: bool) -> bool {
        set_interrupt_in(&mut self.threads, &mut self.n_interrupts, tid, on)
    }

    pub fn interrupt_pending(&self, tid: Tid) -> bool {
        self.threads.get(&tid).is_some_and(|t| t.interrupt)
    }

    /// Take the thread's pending register writes.
    pub fn take_pending_regs(&mut self, tid: Tid) -> Option<PendingRegs> {
        let t = self.threads.get_mut(&tid)?;
        let p = t.pending_regs.take()?;
        self.n_pending_regs = self.n_pending_regs.saturating_sub(1);
        Some(p.into_inner())
    }

    /// The thread's pending register writes, made if it has none (an allocation: an R point, NO_MEMORY when it fails).
    pub fn pending_regs_entry(
        &mut self,
        tid: Tid,
    ) -> Result<&mut PendingRegs, abi::status::Status> {
        let t = self
            .threads
            .get_mut(&tid)
            .ok_or(abi::status::Status::BadState)?;
        if t.pending_regs.is_none() {
            let b = kcore::fbox::FBox::try_new(PendingRegs::default())
                .map_err(|_| abi::status::Status::NoMemory)?;
            t.pending_regs = Some(b);
            self.n_pending_regs = self.n_pending_regs.saturating_add(1);
        }
        t.pending_regs
            .as_deref_mut()
            .ok_or(abi::status::Status::BadState)
    }

    /// Register a thread: its record goes in with the room `reserve_thread` made. The record of a tid already there is replaced.
    #[inline(never)]
    pub fn register_thread(&mut self, tid: Tid, t: UThread, stack: Option<SpawnStack>) {
        let pid = t.pid;
        self.threads.insert_reserved(tid, t);
        if let Some(p) = self.procs.get_mut(&pid) {
            p.live_tids.insert_reserved(tid);
            if let Some(st) = stack {
                p.stacks.insert_reserved(tid, st);
            }
        }
    }

    /// Register a bare thread record (the tests' stand-in for a spawner: no image, no stack). Makes its own room.
    pub fn add_thread(
        &mut self,
        tid: Tid,
        pid: u64,
        obj: Option<kcore::object::ObjId>,
    ) -> Result<(), abi::status::Status> {
        self.reserve_thread(pid)?;
        self.register_thread(
            tid,
            UThread {
                pid,
                obj,
                ..Default::default()
            },
            None,
        );
        if let Some(o) = obj {
            kcore::ipc::thread_set_tid(&mut self.objs, o, tid);
        }
        Ok(())
    }

    /// Drop a thread's record (it ended), keeping the counts.
    #[inline(never)]
    pub fn unregister_thread(&mut self, tid: Tid) -> Option<UThread> {
        let t = self.threads.remove(&tid)?;
        if t.suspend.is_some() {
            self.n_suspended = self.n_suspended.saturating_sub(1);
        }
        if t.interrupt {
            self.n_interrupts = self.n_interrupts.saturating_sub(1);
        }
        if t.pending_regs.is_some() {
            self.n_pending_regs = self.n_pending_regs.saturating_sub(1);
        }
        Some(t)
    }

    /// Room, before a call changes anything, for one more thread record, one more `live_tids` entry of `pid` and one more start.
    pub fn reserve_thread(&mut self, pid: u64) -> Result<(), abi::status::Status> {
        self.threads
            .reserve(1)
            .map_err(|_| abi::status::Status::NoMemory)?;
        self.starts
            .reserve(1)
            .map_err(|_| abi::status::Status::NoMemory)?;
        // (room for the new thread to wait for a page: T248)
        self.pagers
            .reserve_room(self.threads.len().saturating_add(1))
            .map_err(|_| abi::status::Status::NoMemory)?;
        match self.procs.get_mut(&pid) {
            Some(p) => {
                p.live_tids
                    .reserve(1)
                    .map_err(|_| abi::status::Status::NoMemory)?;
                p.stacks
                    .reserve(1)
                    .map_err(|_| abi::status::Status::NoMemory)
            }
            None => Ok(()),
        }
    }
}

impl Default for UserState {
    fn default() -> Self {
        UserState::new()
    }
}

/// The per-thread records.
pub type Threads = kcore::fmap::FMap<Tid, UThread>;

/// `UserState::set_interrupt` on the two fields it touches, for a caller that holds them by name while the rest of the state is borrowed (KT14: a callback that
/// opened the whole state again aliased the `&mut` the caller held).
pub fn set_interrupt_in(
    threads: &mut Threads,
    n_interrupts: &mut usize,
    tid: Tid,
    on: bool,
) -> bool {
    let Some(t) = threads.get_mut(&tid) else {
        return false;
    };
    let had = core::mem::replace(&mut t.interrupt, on);
    if had != on {
        if on {
            *n_interrupts = n_interrupts.saturating_add(1);
        } else {
            *n_interrupts = n_interrupts.saturating_sub(1);
        }
    }
    had
}
