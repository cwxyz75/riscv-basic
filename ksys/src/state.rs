//! The state of user mode (TS3): the kernel objects, the processes, the threads' starts, the wait tables, and the kernel's own handles. The
//! syscall functions that act on it take `&mut UserState` and what the machine provides, so they run on the host.

use crate::proc::{Process, SpawnStack};
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
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
}

/// Everything the syscall layer keeps about user mode: the kernel objects, the processes and threads, the wait tables, and the kernel's own handles.
pub struct UserState {
    pub objs: Kobjs,
    pub procs: BTreeMap<u64, Box<Process>>,
    pub thread_pid: BTreeMap<Tid, u64>,
    /// Scheduling flags (bit 0: round robin) set by `thread_set_sched`, by thread.
    pub sched_flags: BTreeMap<Tid, u8>,
    pub next_pid: u64,
    /// The generation new handle tables start their slots at (0; `ff=handlegen:` in test builds).
    pub first_handle_gen: u64,
    /// Threads that are inside `cleanup_dead` (a per-thread recursion guard, T63: a global flag made every other
    /// hart skip the cleanup while one cleaner was preempted).
    pub cleaning: BTreeSet<Tid>,
    /// Threads with a pending `thread_interrupt`: the next blocking wait that would sleep returns INTERRUPTED and takes the flag.
    pub interrupts: BTreeSet<Tid>,
    /// Thread starts waiting for their thread to run, by token.
    pub starts: BTreeMap<usize, ThreadStart>,
    pub next_token: usize,
    /// Handles the kernel itself holds (the creator's ends of bootstrap channels).
    pub ktable: HandleTable,
    /// The futex wait table (S7b).
    pub futexes: kcore::futex::Futexes,
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
        UserState {
            objs: Kobjs::new(),
            procs: BTreeMap::new(),
            thread_pid: BTreeMap::new(),
            sched_flags: BTreeMap::new(),
            next_pid: 1,
            first_handle_gen: 0,
            cleaning: BTreeSet::new(),
            interrupts: BTreeSet::new(),
            starts: BTreeMap::new(),
            next_token: 1,
            ktable: HandleTable::new(256),
            futexes: kcore::futex::Futexes::new(),
            waits: kcore::wait::WaitTable::new(),
            irq_objs: alloc::vec::Vec::new(),
            line: String::new(),
            pass: 0,
            fail: 0,
        }
    }
}

impl Default for UserState {
    fn default() -> Self {
        UserState::new()
    }
}
