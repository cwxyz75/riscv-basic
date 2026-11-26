//! Channels and events: the kernel objects of S3a, and the operations on them that
//! take handles (SYSCALLS.md, "Channels" and "Events"). Pure logic over a
//! [`Registry`] and a [`HandleTable`]; the syscall layer only copies bytes in and out.

use crate::handle::{Entry, HandleTable};
use crate::job::{JobId, Jobs, Resource};
use crate::object::{ObjId, Object, Registry};
use abi::limits::{
    MAX_MSG_BYTES, MAX_MSG_HANDLES, MAX_QUEUED_BYTES, MAX_QUEUED_MSGS, SOCKET_BUFFER,
};
use abi::rights::Rights;
use abi::signals;
use abi::status::Status;
use abi::Handle;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

/// One queued message: bytes, and the handles that travel with it (each holding a
/// reference to its object).
pub struct Message {
    pub data: Vec<u8>,
    pub handles: Vec<Entry>,
    /// The thread that sent this as a `channel_call` request and waits for the reply (L22), and the number of that call (a counter that names this one
    /// call among the caller's, so that a request left queued by an earlier call, read later, is not mistaken for the live one): the thread that reads
    /// it is lent the caller's priority.
    pub caller: Option<(u64, u64)>,
}

/// Kernel memory a queued message costs beyond its bytes (the queue node and the `Message`).
const MSG_OVERHEAD: usize = 64;
/// What one port binding costs.
const BINDING_COST: usize = 32;

impl Message {
    /// What this message counts against the IPC budget: bytes, a fixed overhead, and its
    /// handle entries (they are kernel memory too: found by the trust-boundary audit).
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: message sizes are bounded by MAX_MSG_BYTES and MAX_MSG_HANDLES
    fn cost(&self) -> usize {
        self.data.len() + MSG_OVERHEAD + self.handles.len() * core::mem::size_of::<Entry>()
    }
}

impl Channel {
    fn queued_bytes(&self) -> usize {
        self.queue.iter().map(|m| m.data.len()).sum()
    }
}

pub struct Channel {
    peer: Option<ObjId>,
    /// Messages written by the peer, waiting to be read from this end.
    queue: VecDeque<Message>,
    peer_closed: bool,
}

pub struct Event {
    signals: u32,
}

/// A process, as a kernel object: what a process handle names.
pub struct ProcessObj {
    pub pid: u64,
    pub terminated: bool,
    /// Stopped by `process_suspend`: raises STOPPED.
    pub stopped: bool,
    pub exit_code: i32,
    /// CPU nanoseconds run by the threads that have ended (the live ones are read from the scheduler when asked); the whole figure once
    /// the process is over (`process_info` 1, AB6).
    pub cpu_ns: u64,
    /// Destroying this object (the last handle closing) kills a still-running process. False
    /// for processes the kernel itself started: their `process_self` handles are only views.
    pub kill_on_close: bool,
}

/// A user thread, as a kernel object: what a thread handle names.
pub struct ThreadObj {
    pub pid: u64,
    /// The kernel thread behind it, once the spawner has registered it (KT9: the lookup from a handle to its thread).
    pub tid: Option<u64>,
    pub terminated: bool,
    /// Stopped by `thread_suspend` (EX1): raises SUSPENDED.
    pub suspended: bool,
    pub exit_code: i32,
    /// CPU nanoseconds the thread ran, final once it has ended (`thread_info` 1, AB6).
    pub cpu_ns: u64,
}

/// Which level an exception was offered at (EX2): the process's own channel, then the job's and each ancestor's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExcLevel {
    Process,
    Job(JobId),
}

/// A thread's exception, as a kernel object: what the exception handle names. The thread stays stopped until it is resolved: retried, passed to the
/// next level, or ended with its process; closing the last handle passes it on.
pub struct ExceptionObj {
    pub pid: u64,
    pub tid: u64,
    /// `abi::arch::fault` kind, the pc, the faulting address and the architecture's raw cause.
    pub kind: u32,
    pub pc: u64,
    pub address: u64,
    pub cause: u64,
    pub level: ExcLevel,
    /// Which exception of its thread this is (a counter the kernel keeps): a decision counts only for the one the thread is stopped for.
    pub serial: u64,
    /// `exception_resume` has acted on it (or the process ended): closing the handle then does nothing.
    pub resolved: bool,
}

/// One end of a byte-stream socket: what it holds is what its *peer* wrote.
pub struct SocketEnd {
    peer: Option<ObjId>,
    buf: VecDeque<u8>,
    peer_closed: bool,
    /// This end called `socket_shutdown`: it writes no more, its peer reads end-of-stream.
    shut: bool,
}

/// An interrupt object: one PLIC source. When it fires the kernel masks the source and sets
/// `fired` (the INTERRUPT signal); `interrupt_ack` clears it and unmasks.
pub struct IrqObj {
    pub irq: u32,
    pub fired: bool,
    /// How many times the kernel claimed this source (a storm would show here).
    pub claims: u64,
}

/// A device domain (IOMMU-SPEC 4): the domain's id in the kernel's store (`kcore::iodomain::DomainId::raw`), the job that pays for it
/// (the object holds a use of it until its last handle goes: 4.5), and whether a fault record waits in its ring (DEVICE_FAULT, 7.4).
/// The last handle destroys the domain (`needs_cleanup`).
pub struct IoDomainObj {
    pub domain: u64,
    pub job: JobId,
    pub fault: bool,
    /// `iodomain_destroy` was called (maps are BAD_STATE from then on; a second destroy does nothing).
    pub destroyed: bool,
}

/// A resolved device-domain handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoDomainRef {
    pub obj: ObjId,
    pub domain: u64,
    pub job: JobId,
    pub destroyed: bool,
}

/// One binding of a port: watch `obj` for `mask`, report as `key`.
struct Binding {
    /// The table and handle the binding was made through (L21c): it ends when that handle is closed, replaced or sent away.
    via: (u64, u64),
    obj: ObjId,
    mask: u32,
    key: u64,
    /// Disarm on delivery (`port_flags::ONESHOT`).
    oneshot: bool,
    /// A disarmed binding is never ready until `port_rearm`.
    armed: bool,
}

/// A wait set. Bindings hold no references: a destroyed object's binding is pruned.
pub struct PortObj {
    bindings: Vec<Binding>,
    /// Where the next poll starts, so ready bindings are reported in turn.
    next: usize,
}

/// Every kernel object kind so far; later milestones add variants.
pub enum KObj {
    Channel(Channel),
    Event(Event),
    /// A memory object, by id in the VMO store.
    Vmo(u64),
    Process(ProcessObj),
    Thread(ThreadObj),
    /// A thread's fault, offered to a handler (EX2).
    Exception(ExceptionObj),
    /// A pager (MM9): the id of its state in `kcore::pager::Pagers`.
    Pager(u64),
    Socket(SocketEnd),
    Port(PortObj),
    /// A job (S4.5): the ledger id; the object holds one use of it.
    Job(JobId),
    /// An interrupt source (S6a-2): its PLIC source number and whether it fired and awaits `ack`.
    Interrupt(IrqObj),
    /// The root resource (S6): the authority to mint device memory, DMA buffers and interrupts.
    Resource,
    /// A device domain (IOMMU6).
    IoDomain(IoDomainObj),
    /// The debug resource (HD4): the authority to write to the kernel's console and read its counters.
    Debug,
}

impl Object for KObj {
    fn for_each_watched(&self, f: &mut dyn FnMut(ObjId)) {
        if let KObj::Port(p) = self {
            for b in &p.bindings {
                f(b.obj);
            }
        }
    }

    fn held_count(&self) -> usize {
        match self {
            KObj::Channel(c) => c.queue.iter().map(|m| m.handles.len()).sum(),
            _ => 0,
        }
    }

    fn held(&self, out: &mut Vec<ObjId>) {
        if let KObj::Channel(c) = self {
            for m in &c.queue {
                out.extend(m.handles.iter().map(|e| e.obj));
            }
        }
    }

    fn peer(&self) -> Option<ObjId> {
        match self {
            KObj::Channel(c) => c.peer,
            KObj::Socket(s) => s.peer,
            _ => None,
        }
    }

    fn needs_cleanup(&self) -> bool {
        matches!(
            self,
            KObj::Vmo(_)
                | KObj::Process(_)
                | KObj::Job(_)
                | KObj::Interrupt(_)
                | KObj::IoDomain(_)
                | KObj::Exception(_)
                | KObj::Pager(_)
        )
    }

    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: binding counts are bounded by the port's binding limit
    fn meter(&self) -> usize {
        match self {
            KObj::Channel(c) => c.queue.iter().map(Message::cost).sum(),
            KObj::Socket(s) => s.buf.len(),
            KObj::Port(p) => p.bindings.len() * BINDING_COST,
            _ => 0,
        }
    }

    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: binding counts are bounded by the port's binding limit
    fn charged_bytes(&self) -> usize {
        match self {
            KObj::Channel(c) => c.queue.iter().map(Message::cost).sum(),
            KObj::Socket(s) => s.buf.len(),
            KObj::Port(p) => p.bindings.len() * BINDING_COST,
            _ => 0,
        }
    }

    fn peer_destroyed(&mut self) {
        match self {
            KObj::Channel(c) => {
                c.peer = None;
                c.peer_closed = true;
            }
            KObj::Socket(s) => {
                s.peer = None;
                s.peer_closed = true;
            }
            _ => {}
        }
    }
}

pub type Kobjs = Registry<KObj>;

/// The kernel memory one object body holds, in bytes (MM5): the size of the object (`KObj`) and the registry slot around it, rounded up to a
/// power of two (the doubling of the registry's vector is not counted), charged to the creating job's kernel bytes with the object and credited when the object is destroyed. Queued data has its own
/// charge (`IpcBytes`); handle tables, thread state and the rest are charged where they grow.
pub const OBJECT_BYTES: u64 =
    ((core::mem::size_of::<KObj>() + crate::object::SLOT_OVERHEAD) as u64).next_power_of_two();

/// Who is charged for a new object: the job of the creating process, and the ledger.
pub struct Quota<'a> {
    pub jobs: &'a mut Jobs,
    /// `None`: the objects are not charged to anyone (the pure tests' convenience calls).
    pub job: Option<JobId>,
}

impl Quota<'_> {
    /// Charge `n` new objects to the job (after applying what destroyed objects gave back).
    fn objects(&mut self, reg: &mut Kobjs, n: u64) -> Result<(), Status> {
        settle(reg, self.jobs);
        // (room for the object's destruction to record itself, before anything is charged or made: KT9)
        reg.reserve_for(n as usize)?;
        match self.job {
            Some(job) => {
                self.jobs
                    .charge(job, Resource::Objects, n)
                    .map_err(|_| Status::NoMemory)?;
                // (the bodies' memory: all or nothing with the count)
                if self
                    .jobs
                    .charge(job, Resource::KernelBytes, n.saturating_mul(OBJECT_BYTES))
                    .is_err()
                {
                    self.jobs.credit(job, Resource::Objects, n);
                    return Err(Status::NoMemory);
                }
                // each charged object holds a use of its job until its credit is applied
                // (`settle`), so the record cannot be freed while it still has usage
                for _ in 0..n {
                    self.jobs.retain(job);
                }
                Ok(())
            }
            None => Ok(()),
        }
    }
}

/// Apply the credits owed for objects destroyed since the last call: one object and the queued
/// bytes each held. Every function that charges a job calls this first, so a job that has just
/// closed things can use the room at once.
pub fn settle(reg: &mut Kobjs, jobs: &mut Jobs) {
    for (job, bytes) in reg.drain_credits() {
        jobs.credit(job, Resource::Objects, 1);
        jobs.credit(job, Resource::KernelBytes, OBJECT_BYTES);
        jobs.credit(job, Resource::IpcBytes, bytes as u64);
        jobs.release(job); // the use the object held (see `Quota::objects`)
    }
}

/// Charge `n` queued bytes to the job of `obj` (nothing for an object that is not charged).
fn charge_bytes(reg: &Kobjs, jobs: &mut Jobs, obj: ObjId, n: usize) -> Result<(), Status> {
    match reg.job_of(obj) {
        Some(job) => jobs
            .charge(job, Resource::IpcBytes, n as u64)
            .map_err(|_| Status::NoMemory),
        None => Ok(()),
    }
}

/// Charge without a refusal (the room was checked just before).
fn charge_bytes_unchecked(reg: &Kobjs, jobs: &mut Jobs, obj: ObjId, n: usize) {
    if let Some(job) = reg.job_of(obj) {
        let _ = jobs.charge(job, Resource::IpcBytes, n as u64);
    }
}

fn credit_bytes(reg: &Kobjs, jobs: &mut Jobs, obj: ObjId, n: usize) {
    if let Some(job) = reg.job_of(obj) {
        jobs.credit(job, Resource::IpcBytes, n as u64);
    }
}

/// What `channel_read` returns.
#[derive(Debug, PartialEq, Eq)]
pub struct Received {
    pub data: Vec<u8>,
    pub handles: Vec<Handle>,
}

/// Why a read could not deliver, with the sizes needed when the buffers were too small.
#[derive(Debug, PartialEq, Eq)]
pub enum ReadError {
    Status(Status),
    TooSmall { bytes: usize, handles: usize },
}

impl From<Status> for ReadError {
    fn from(s: Status) -> Self {
        ReadError::Status(s)
    }
}

/// Resolve `h` to a channel object, checking the type and a right.
fn channel_of(reg: &Kobjs, table: &HandleTable, h: Handle, need: Rights) -> Result<ObjId, Status> {
    let entry = table.get(h)?;
    match reg.get(entry.obj) {
        Some(KObj::Channel(_)) => {}
        Some(_) => return Err(Status::WrongType),
        None => return Err(Status::BadHandle),
    }
    if !entry.rights.contains(need) {
        return Err(Status::AccessDenied);
    }
    Ok(entry.obj)
}

/// A connected pair of channel endpoints, both with the default channel rights.
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)] // INVARIANT: live is below MAX_OBJECTS, so + 2 cannot overflow
pub fn channel_create_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    q: &mut Quota,
) -> Result<(Handle, Handle), Status> {
    if table.room() < 2 || table.reserve(2).is_err() || reg.live() + 2 > reg.cap() {
        return Err(Status::NoMemory);
    }
    q.objects(reg, 2)?;
    let end = || Channel {
        peer: None,
        queue: VecDeque::new(),
        peer_closed: false,
    };
    let (a_end, b_end) = (end(), end());
    let a = reg.create_charged(KObj::Channel(a_end), q.job);
    let b = reg.create_charged(KObj::Channel(b_end), q.job);
    for (me, other) in [(a, b), (b, a)] {
        if let Some(KObj::Channel(c)) = reg.get_mut(me) {
            c.peer = Some(other);
        }
    }
    let ha = table
        .insert(reg, a, Rights::CHANNEL)
        .expect("room was checked");
    let hb = table
        .insert(reg, b, Rights::CHANNEL)
        .expect("room was checked");
    Ok((ha, hb))
}

/// Queue a message on the peer of `ch`, moving `handles` out of the caller's table
/// into it. Changes nothing unless it succeeds. The checks run in the order SYSCALLS.md
/// lists them: sizes first (they bound the work of everything after), then handles,
/// then the channel itself.
pub fn channel_write_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    ch: Handle,
    data: &[u8],
    handles: &[Handle],
    jobs: &mut Jobs,
) -> Result<(), Status> {
    channel_write_from(reg, table, ch, data, handles, jobs, None)
}

/// `channel_write_in` for the request of a `channel_call` sent by thread `caller` (L22): the message carries the caller's id, and the thread that
/// reads it is lent the caller's priority until the call ends.
#[allow(
    clippy::too_many_arguments,
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::unreachable
)] // INVARIANT: i < handles.len() (the duplicate check loop); the queue and byte counts are bounded by the per-channel limits checked first
pub fn channel_write_from(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    ch: Handle,
    data: &[u8],
    handles: &[Handle],
    jobs: &mut Jobs,
    caller: Option<(u64, u64)>,
) -> Result<(), Status> {
    if data.len() > MAX_MSG_BYTES || handles.len() > MAX_MSG_HANDLES {
        return Err(Status::TooBig);
    }
    // BAD_HANDLE: the channel, then each listed handle.
    let ch_entry = table.get(ch)?;
    for &h in handles {
        table.get(h)?;
    }
    // WRONG_TYPE.
    match reg.get(ch_entry.obj) {
        Some(KObj::Channel(_)) => {}
        Some(_) => return Err(Status::WrongType),
        None => return Err(Status::BadHandle),
    }
    // ACCESS_DENIED: WRITE on the channel, TRANSFER on each listed handle.
    if !ch_entry.rights.contains(Rights::WRITE) {
        return Err(Status::AccessDenied);
    }
    for &h in handles {
        table.check_transferable(h)?;
    }
    // INVALID_ARGS: a handle listed twice.
    for (i, h) in handles.iter().enumerate() {
        if handles[..i].contains(h) {
            return Err(Status::InvalidArgs);
        }
    }
    let me = ch_entry.obj;
    let KObj::Channel(this) = reg.get(me).expect("resolved") else {
        unreachable!()
    };
    let peer = this.peer;
    // NOT_SUPPORTED: an endpoint of this very channel in the list. Sending either end
    // through the channel would put it in its own queue, a self-cycle.
    for &h in handles {
        let o = table.get(h).expect("validated").obj;
        if o == me || Some(o) == peer {
            return Err(Status::NotSupported);
        }
    }
    let peer = peer.ok_or(Status::PeerClosed)?;
    match reg.get(peer) {
        Some(KObj::Channel(p)) => {
            if p.queue.len() >= MAX_QUEUED_MSGS || p.queued_bytes() + data.len() > MAX_QUEUED_BYTES
            {
                return Err(Status::ShouldWait);
            }
        }
        _ => return Err(Status::PeerClosed),
    }
    // The system-wide budget for queued data (a user must not be able to exhaust kernel memory).
    let cost = data.len() + MSG_OVERHEAD + handles.len() * core::mem::size_of::<Entry>();
    if reg.meter() + cost > reg.ipc_cap() {
        return Err(Status::NoMemory);
    }
    // The job of the endpoint that receives the message pays for it, until it is read or the
    // endpoint goes; the charge comes before anything is allocated or moved, and is given back
    // if an allocation then fails.
    settle(reg, jobs);
    charge_bytes(reg, jobs, peer, cost)?;
    let undo = |reg: &Kobjs, jobs: &mut Jobs, e: Status| {
        credit_bytes(reg, jobs, peer, cost);
        Err(e)
    };
    match reg.get_mut(peer) {
        Some(KObj::Channel(p)) => {
            if p.queue.try_reserve(1).is_err() {
                return undo(reg, jobs, Status::NoMemory);
            }
        }
        _ => return undo(reg, jobs, Status::PeerClosed),
    }
    // Allocate before committing, so running out of memory changes nothing.
    let mut bytes: Vec<u8> = Vec::new();
    if bytes.try_reserve_exact(data.len()).is_err() {
        return undo(reg, jobs, Status::NoMemory);
    }
    bytes.extend_from_slice(data);
    let mut entries: Vec<Entry> = Vec::new();
    if entries.try_reserve_exact(handles.len()).is_err() {
        return undo(reg, jobs, Status::NoMemory);
    }
    // Commit: the handles leave the table and travel with the message.
    entries.extend(
        handles
            .iter()
            .map(|&h| table.remove_for_transfer(reg, h).expect("validated")),
    );
    for e in &entries {
        reg.debug_assert_not_white(e.obj);
    }
    if let Some(KObj::Channel(p)) = reg.get_mut(peer) {
        p.queue.push_back(Message {
            data: bytes,
            handles: entries,
            caller,
        });
        reg.meter_add(cost);
    }
    Ok(())
}

#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::unreachable)] // INVARIANT: `me` was resolved to a live channel by the caller in the same call
fn read_at(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    me: ObjId,
    pick: impl Fn(&VecDeque<Message>) -> Option<usize>,
    max_bytes: usize,
    max_handles: usize,
    jobs: &mut Jobs,
) -> Result<Received, ReadError> {
    let KObj::Channel(c) = reg.get(me).expect("resolved") else {
        unreachable!()
    };
    let Some(i) = pick(&c.queue) else {
        // Nothing to deliver. If the peer is gone nothing more can ever arrive, so say so,
        // even when other, non-matching messages remain queued (a `txid` read).
        return Err(if c.peer_closed {
            Status::PeerClosed
        } else {
            Status::ShouldWait
        }
        .into());
    };
    let msg = &c.queue[i];
    if msg.data.len() > max_bytes || msg.handles.len() > max_handles {
        return Err(ReadError::TooSmall {
            bytes: msg.data.len(),
            handles: msg.handles.len(),
        });
    }
    if table.room() < msg.handles.len() {
        return Err(Status::NoMemory.into());
    }
    let KObj::Channel(c) = reg.get_mut(me).expect("resolved") else {
        unreachable!()
    };
    // Everything that can fail for lack of memory is done before the message leaves the queue: the table's room and the result vector.
    let n = c.queue[i].handles.len();
    table.reserve(n)?;
    let mut handles = Vec::new();
    handles.try_reserve_exact(n).map_err(|_| Status::NoMemory)?;
    let msg = c.queue.remove(i).expect("picked");
    if let Some((caller, call)) = msg.caller {
        // a call's request was read: its reader is lent the caller's priority (the kernel does it after the call, L22)
        reg.note_dequeued_call(caller, call);
    }
    reg.meter_sub(msg.cost());
    credit_bytes(reg, jobs, me, msg.cost());
    for e in &msg.handles {
        handles.push(
            table
                .insert(reg, e.obj, e.rights)
                .expect("room was checked"),
        );
    }
    Ok(Received {
        data: msg.data,
        handles,
    })
}

/// What `channel_read` / `channel_read_txid` would do right now, without doing it: the
/// sizes (bytes, handles) of the message they would deliver, or the error they would
/// return (including BUFFER_TOO_SMALL with the sizes, and NO_MEMORY for a full table).
/// The syscall layer uses it to validate the destination buffers against the *actual*
/// message before dequeuing anything.
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::unreachable)] // INVARIANT: `me` was resolved to a live channel by the caller in the same call
pub fn channel_peek(
    reg: &Kobjs,
    table: &HandleTable,
    ch: Handle,
    txid: Option<u32>,
    max_bytes: usize,
    max_handles: usize,
) -> Result<(usize, usize), ReadError> {
    let me = channel_of(reg, table, ch, Rights::READ)?;
    let KObj::Channel(c) = reg.get(me).expect("resolved") else {
        unreachable!()
    };
    let pos = match txid {
        Some(t) => c
            .queue
            .iter()
            .position(|m| m.data.len() >= 4 && m.data[..4] == t.to_le_bytes()),
        None => (!c.queue.is_empty()).then_some(0),
    };
    let Some(i) = pos else {
        return Err(if c.peer_closed {
            Status::PeerClosed
        } else {
            Status::ShouldWait
        }
        .into());
    };
    let msg = &c.queue[i];
    if msg.data.len() > max_bytes || msg.handles.len() > max_handles {
        return Err(ReadError::TooSmall {
            bytes: msg.data.len(),
            handles: msg.handles.len(),
        });
    }
    if table.room() < msg.handles.len() {
        return Err(Status::NoMemory.into());
    }
    Ok((msg.data.len(), msg.handles.len()))
}

/// Read the oldest message. Handles arrive in the caller's table.
pub fn channel_read_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    ch: Handle,
    max_bytes: usize,
    max_handles: usize,
    jobs: &mut Jobs,
) -> Result<Received, ReadError> {
    let me = channel_of(reg, table, ch, Rights::READ)?;
    read_at(
        reg,
        table,
        me,
        |q| (!q.is_empty()).then_some(0),
        max_bytes,
        max_handles,
        jobs,
    )
}

/// Read the oldest message whose first four bytes (little endian) are `txid`.
#[allow(clippy::indexing_slicing)] // INVARIANT: the length test precedes the slice
pub fn channel_read_txid_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    ch: Handle,
    txid: u32,
    max_bytes: usize,
    max_handles: usize,
    jobs: &mut Jobs,
) -> Result<Received, ReadError> {
    let me = channel_of(reg, table, ch, Rights::READ)?;
    let pick = |q: &VecDeque<Message>| {
        q.iter()
            .position(|m| m.data.len() >= 4 && m.data[..4] == txid.to_le_bytes())
    };
    read_at(reg, table, me, pick, max_bytes, max_handles, jobs)
}

/// A new event with all user signals clear.
#[allow(clippy::expect_used)] // INVARIANT: the table's room was checked at the top
pub fn event_create_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    q: &mut Quota,
) -> Result<Handle, Status> {
    if table.room() < 1 || table.reserve(1).is_err() || reg.live() >= reg.cap() {
        return Err(Status::NoMemory);
    }
    q.objects(reg, 1)?;
    let id = reg.create_charged(KObj::Event(Event { signals: 0 }), q.job);
    Ok(table
        .insert(reg, id, Rights::EVENT)
        .expect("room was checked"))
}

/// Clear then set user signals on an event.
pub fn event_signal(
    reg: &mut Kobjs,
    table: &HandleTable,
    ev: Handle,
    clear: u32,
    set: u32,
) -> Result<(), Status> {
    let entry = table.get(ev)?;
    if !matches!(reg.get(entry.obj), Some(KObj::Event(_))) {
        return Err(if reg.is_alive(entry.obj) {
            Status::WrongType
        } else {
            Status::BadHandle
        });
    }
    if !entry.rights.contains(Rights::SIGNAL) {
        return Err(Status::AccessDenied);
    }
    if (clear | set) & !signals::USER_MASK != 0 {
        return Err(Status::InvalidArgs);
    }
    if let Some(KObj::Event(e)) = reg.get_mut(entry.obj) {
        e.signals = (e.signals & !clear) | set;
    }
    Ok(())
}

/// The signal word of the object a handle names.
pub fn signals_of(reg: &Kobjs, table: &HandleTable, h: Handle) -> Result<u32, Status> {
    let entry = table.get(h)?;
    object_signals(reg, entry.obj).ok_or(Status::BadHandle)
}

/// The signal word of the object `obj` (None if it is gone): what a waiter on it evaluates. Public for the
/// wake path (slice W) and its tests.
pub fn signals_of_object(reg: &Kobjs, obj: ObjId) -> Option<u32> {
    object_signals(reg, obj)
}

/// Audit (KT9, T177): the index of which ports bind which objects (`Registry::bound_by`) holds, for every live port, at least the multiplicity of each of its
/// bindings (a pair for a dead bound object may linger until the port prunes it), and no insert into it ever found no reserved room.
pub fn audit_bound_index(reg: &Kobjs) -> Result<(), alloc::string::String> {
    if reg.bound_short_inserts() != 0 {
        return Err("the bound index took an insert with no room reserved".into());
    }
    for port in reg.live_ids() {
        let Some(KObj::Port(p)) = reg.get(port) else {
            continue;
        };
        for b in p.bindings.iter() {
            let n = p.bindings.iter().filter(|c| c.obj == b.obj).count();
            if (reg.bound_count(b.obj, port) as usize) < n {
                return Err(alloc::format!(
                    "port {port:?} binds {:?} {n} time(s), the index says {}",
                    b.obj,
                    reg.bound_count(b.obj, port)
                ));
            }
        }
    }
    Ok(())
}

/// The objects a port binds (empty for anything that is not a live port).
pub fn port_bound_objects(reg: &Kobjs, port: ObjId) -> Vec<ObjId> {
    match reg.get(port) {
        Some(KObj::Port(p)) => p.bindings.iter().map(|b| b.obj).collect(),
        _ => Vec::new(),
    }
}

/// Is there a message with transaction id `txid` (its first four bytes, little endian) queued at channel end `obj`? (`channel_call`'s wait, L21d.)
pub fn channel_has_txid(reg: &Kobjs, obj: ObjId, txid: u32) -> bool {
    match reg.get(obj) {
        Some(KObj::Channel(c)) => c
            .queue
            .iter()
            .any(|m| m.data.get(..4) == Some(&txid.to_le_bytes()[..])),
        _ => false,
    }
}

/// The signal word of a live object (None if it is gone).
fn object_signals(reg: &Kobjs, obj: ObjId) -> Option<u32> {
    Some(match reg.get(obj)? {
        KObj::Event(e) => e.signals,
        KObj::Vmo(_)
        | KObj::Job(_)
        | KObj::Resource
        | KObj::Debug
        | KObj::Exception(_)
        | KObj::Pager(_) => 0,
        KObj::Interrupt(i) => {
            if i.fired {
                signals::INTERRUPT
            } else {
                0
            }
        }
        KObj::IoDomain(d) => {
            if d.fault {
                signals::DEVICE_FAULT
            } else {
                0
            }
        }
        KObj::Process(p) => {
            if p.terminated {
                signals::TERMINATED
            } else if p.stopped {
                signals::STOPPED
            } else {
                0
            }
        }
        KObj::Thread(t) => {
            if t.terminated {
                signals::TERMINATED
            } else if t.suspended {
                signals::SUSPENDED
            } else {
                0
            }
        }
        KObj::Channel(c) => {
            let mut s = 0;
            if !c.queue.is_empty() {
                s |= signals::READABLE;
            }
            if c.peer_closed {
                s |= signals::PEER_CLOSED;
            } else if let Some(KObj::Channel(p)) = c.peer.and_then(|p| reg.get(p)) {
                if p.queue.len() < MAX_QUEUED_MSGS && p.queued_bytes() < MAX_QUEUED_BYTES {
                    s |= signals::WRITABLE;
                }
            }
            s
        }
        KObj::Socket(e) => {
            let mut s = 0;
            if !e.buf.is_empty() {
                s |= signals::READABLE;
            }
            if e.peer_closed {
                s |= signals::PEER_CLOSED;
            } else if let Some(KObj::Socket(p)) = e.peer.and_then(|p| reg.get(p)) {
                if p.buf.len() < SOCKET_BUFFER && !e.shut {
                    s |= signals::WRITABLE;
                }
                if p.shut {
                    s |= signals::WRITE_CLOSED;
                }
            }
            s
        }
        KObj::Port(p) => {
            if p.bindings.iter().any(|b| binding_ready(reg, b)) {
                signals::READABLE
            } else {
                0
            }
        }
    })
}

/// Is the binding's object alive with a watched signal asserted? (Ports cannot be bound,
/// so this never recurses.)
fn binding_ready(reg: &Kobjs, b: &Binding) -> bool {
    b.armed && matches!(object_signals(reg, b.obj), Some(s) if s & b.mask != 0)
}

// --- sockets ---

fn socket_of(reg: &Kobjs, table: &HandleTable, h: Handle, need: Rights) -> Result<ObjId, Status> {
    let entry = table.get(h)?;
    match reg.get(entry.obj) {
        Some(KObj::Socket(_)) => {}
        Some(_) => return Err(Status::WrongType),
        None => return Err(Status::BadHandle),
    }
    if !entry.rights.contains(need) {
        return Err(Status::AccessDenied);
    }
    Ok(entry.obj)
}

/// A connected pair of socket endpoints with the default socket rights.
#[allow(clippy::arithmetic_side_effects, clippy::expect_used)] // INVARIANT: live is below MAX_OBJECTS, so + 2 cannot overflow
pub fn socket_create_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    q: &mut Quota,
) -> Result<(Handle, Handle), Status> {
    if table.room() < 2 || table.reserve(2).is_err() || reg.live() + 2 > reg.cap() {
        return Err(Status::NoMemory);
    }
    q.objects(reg, 2)?;
    let end = || SocketEnd {
        peer: None,
        buf: VecDeque::new(),
        peer_closed: false,
        shut: false,
    };
    let a = reg.create_charged(KObj::Socket(end()), q.job);
    let b = reg.create_charged(KObj::Socket(end()), q.job);
    for (me, other) in [(a, b), (b, a)] {
        if let Some(KObj::Socket(s)) = reg.get_mut(me) {
            s.peer = Some(other);
        }
    }
    let ha = table
        .insert(reg, a, Rights::SOCKET)
        .expect("room was checked");
    let hb = table
        .insert(reg, b, Rights::SOCKET)
        .expect("room was checked");
    Ok((ha, hb))
}

/// The checks of `socket_write` that come before the source memory is touched: handle, type,
/// rights, size.
pub fn socket_write_check(
    reg: &Kobjs,
    table: &HandleTable,
    h: Handle,
    len: usize,
) -> Result<(), Status> {
    socket_of(reg, table, h, Rights::WRITE)?;
    if len > SOCKET_BUFFER {
        return Err(Status::TooBig);
    }
    Ok(())
}

/// Append as much of `data` as fits to the peer's buffer; returns how many bytes that was.
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: buf.len() <= SOCKET_BUFFER is the buffer's own bound
pub fn socket_write_in(
    reg: &mut Kobjs,
    table: &HandleTable,
    h: Handle,
    data: &[u8],
    jobs: &mut Jobs,
) -> Result<usize, Status> {
    settle(reg, jobs);
    let me = socket_of(reg, table, h, Rights::WRITE)?;
    if data.len() > SOCKET_BUFFER {
        return Err(Status::TooBig);
    }
    let meter = reg.meter();
    if matches!(reg.get(me), Some(KObj::Socket(s)) if s.shut) {
        return Err(Status::BadState);
    }
    let peer = match reg.get(me) {
        Some(KObj::Socket(s)) if !s.peer_closed => s.peer,
        _ => None,
    }
    .ok_or(Status::PeerClosed)?;
    let Some(KObj::Socket(p)) = reg.get_mut(peer) else {
        return Err(Status::PeerClosed);
    };
    let end_room = SOCKET_BUFFER - p.buf.len();
    if data.is_empty() {
        return Ok(0);
    }
    if end_room == 0 {
        return Err(Status::ShouldWait);
    }
    // Both limits apply: this end's buffer and the system-wide budget (NO_MEMORY, not
    // SHOULD_WAIT: other sockets' readers will not make room for this one).
    let global_room = reg.ipc_cap().saturating_sub(meter);
    if global_room == 0 {
        return Err(Status::NoMemory);
    }
    // ... and the job of the receiving endpoint's quota (its creator pays for what is queued).
    let job = reg.job_of(peer);
    let job_room = job.map_or(usize::MAX, |j| jobs.room(j, Resource::IpcBytes) as usize);
    if job_room == 0 {
        return Err(Status::NoMemory);
    }
    let n = end_room.min(global_room).min(job_room).min(data.len());
    let Some(KObj::Socket(p)) = reg.get_mut(peer) else {
        return Err(Status::PeerClosed);
    };
    p.buf.try_reserve(n).map_err(|_| Status::NoMemory)?;
    p.buf.extend(&data[..n]);
    reg.meter_add(n);
    charge_bytes_unchecked(reg, jobs, peer, n);
    Ok(n)
}

/// How many bytes `socket_read` would return for `max` (so the caller can check the
/// destination before anything is removed), or the error a read would give.
pub fn socket_peek(
    reg: &Kobjs,
    table: &HandleTable,
    h: Handle,
    max: usize,
) -> Result<usize, Status> {
    let me = socket_of(reg, table, h, Rights::READ)?;
    if max > SOCKET_BUFFER {
        return Err(Status::TooBig);
    }
    let Some(KObj::Socket(s)) = reg.get(me) else {
        return Err(Status::BadHandle);
    };
    if max == 0 {
        return Ok(0);
    }
    if s.buf.is_empty() {
        let peer_shut = matches!(s.peer.and_then(|p| reg.get(p)), Some(KObj::Socket(p)) if p.shut);
        return Err(if s.peer_closed || peer_shut {
            Status::PeerClosed
        } else {
            Status::ShouldWait
        });
    }
    Ok(max.min(s.buf.len()))
}

/// Remove and return up to `max` bytes (see [`socket_peek`]).
pub fn socket_read_in(
    reg: &mut Kobjs,
    table: &HandleTable,
    h: Handle,
    max: usize,
    jobs: &mut Jobs,
) -> Result<Vec<u8>, Status> {
    let n = socket_peek(reg, table, h, max)?;
    let me = socket_of(reg, table, h, Rights::READ)?;
    let Some(KObj::Socket(s)) = reg.get_mut(me) else {
        return Err(Status::BadHandle);
    };
    // (the result is allocated before anything leaves the buffer, so a failed allocation loses no data)
    let mut out: Vec<u8> = Vec::new();
    out.try_reserve_exact(n).map_err(|_| Status::NoMemory)?;
    out.extend(s.buf.drain(..n));
    if s.buf.is_empty() {
        // give the memory back: an idle socket holds nothing
        s.buf = VecDeque::new();
    }
    reg.meter_sub(n);
    credit_bytes(reg, jobs, me, n);
    Ok(out)
}

/// Close this end's write side (half-close): the peer drains what is queued and then reads
/// end-of-stream; this end can still read; its own writes now fail with BAD_STATE. Idempotent.
/// Rights: WRITE.
pub fn socket_shutdown(reg: &mut Kobjs, table: &HandleTable, h: Handle) -> Result<(), Status> {
    let me = socket_of(reg, table, h, Rights::WRITE)?;
    if let Some(KObj::Socket(s)) = reg.get_mut(me) {
        s.shut = true;
    }
    Ok(())
}

// --- ports ---

fn port_of(reg: &Kobjs, table: &HandleTable, h: Handle, need: Rights) -> Result<ObjId, Status> {
    let entry = table.get(h)?;
    match reg.get(entry.obj) {
        Some(KObj::Port(_)) => {}
        Some(_) => return Err(Status::WrongType),
        None => return Err(Status::BadHandle),
    }
    if !entry.rights.contains(need) {
        return Err(Status::AccessDenied);
    }
    Ok(entry.obj)
}

#[allow(clippy::expect_used)] // INVARIANT: the table's room was checked at the top
pub fn port_create_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    q: &mut Quota,
) -> Result<Handle, Status> {
    if table.room() < 1 || table.reserve(1).is_err() || reg.live() >= reg.cap() {
        return Err(Status::NoMemory);
    }
    q.objects(reg, 1)?;
    let obj = reg.create_charged(
        KObj::Port(PortObj {
            bindings: Vec::new(),
            next: 0,
        }),
        q.job,
    );
    Ok(table
        .insert(reg, obj, Rights::PORT)
        .expect("room was checked"))
}

/// Drop the bindings whose object has been destroyed (the spec says they disappear at once,
/// so every operation that looks at the binding set prunes first).
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: i < bindings.len() (loop bound)
fn prune_port(reg: &mut Kobjs, jobs: &mut Jobs, port_obj: ObjId) {
    // One pass (not a search and a shift per dead binding: quadratic under the big kernel lock when
    // thousands of bindings die at once, review 2026-11-14). `next` keeps pointing at the same
    // surviving binding. The bindings are moved out while they are checked against the registry and moved back: no allocation, so a pruning
    // cannot fail for lack of memory (KT9).
    let mut bindings = match reg.get_mut(port_obj) {
        Some(KObj::Port(p)) => core::mem::take(&mut p.bindings),
        _ => return,
    };
    let next = match reg.get(port_obj) {
        Some(KObj::Port(p)) => p.next,
        _ => 0,
    };
    let before_next = bindings
        .iter()
        .take(next)
        .filter(|b| !reg.is_alive(b.obj))
        .count();
    let before = bindings.len();
    // (kept in order, by swapping them down: a dead binding's pair leaves the index as it goes, which a `retain` closure could not do)
    let mut keep = 0;
    for i in 0..before {
        if reg.is_alive(bindings[i].obj) {
            bindings.swap(keep, i);
            keep += 1;
        } else {
            reg.bound_remove(bindings[i].obj, port_obj);
        }
    }
    bindings.truncate(keep);
    let removed = before - bindings.len();
    if let Some(KObj::Port(p)) = reg.get_mut(port_obj) {
        p.bindings = bindings;
        p.next -= before_next.min(p.next);
    }
    reg.meter_sub(removed * BINDING_COST);
    credit_bytes(reg, jobs, port_obj, removed * BINDING_COST);
}

pub fn port_bind_in(
    reg: &mut Kobjs,
    table: &HandleTable,
    port: Handle,
    object: Handle,
    mask: u64,
    key: u64,
    jobs: &mut Jobs,
) -> Result<(), Status> {
    port_bind_flags_in(reg, table, port, object, mask, key, 0, jobs)
}

/// `port_bind` with flags (`abi::port_flags`; any other bit is INVALID_ARGS). The number of bindings
/// is limited by the port's job quota (each costs `BINDING_COST`), not by a fixed count.
#[allow(clippy::too_many_arguments)]
pub fn port_bind_flags_in(
    reg: &mut Kobjs,
    table: &HandleTable,
    port: Handle,
    object: Handle,
    mask: u64,
    key: u64,
    flags: u64,
    jobs: &mut Jobs,
) -> Result<(), Status> {
    settle(reg, jobs);
    let pe = table.get(port)?;
    let oe = table.get(object)?;
    let port_obj = port_of(reg, table, port, Rights::MANAGE)?;
    match reg.get(oe.obj) {
        Some(_) => {}
        None => return Err(Status::BadHandle),
    }
    if !oe.rights.contains(Rights::WAIT) {
        return Err(Status::AccessDenied);
    }
    let _ = pe;
    if mask == 0
        || mask > u64::from(u32::MAX)
        || (mask as u32) & !signals::WAITABLE != 0
        || matches!(reg.get(oe.obj), Some(KObj::Port(_)))
    {
        return Err(Status::InvalidArgs);
    }
    if flags & !abi::port_flags::ONESHOT != 0 {
        return Err(Status::InvalidArgs);
    }
    let mask = mask as u32;
    prune_port(reg, jobs, port_obj);
    let Some(KObj::Port(p)) = reg.get_mut(port_obj) else {
        return Err(Status::BadHandle);
    };
    if p.bindings.iter().any(|b| b.key == key) {
        return Err(Status::InvalidArgs);
    }
    p.bindings.try_reserve(1).map_err(|_| Status::NoMemory)?;
    reg.bound_reserve().map_err(|_| Status::NoMemory)?;
    // a binding costs queued-data budget of the port's job, like a queued message would
    charge_bytes(reg, jobs, port_obj, BINDING_COST)?;
    let Some(KObj::Port(p)) = reg.get_mut(port_obj) else {
        return Err(Status::BadHandle);
    };
    p.bindings.push(Binding {
        via: (table.id(), object.0),
        obj: oe.obj,
        mask,
        key,
        oneshot: flags & abi::port_flags::ONESHOT != 0,
        armed: true,
    });
    reg.meter_add(BINDING_COST);
    reg.watch_adjust(oe.obj, 1);
    reg.bound_add(oe.obj, port_obj);
    Ok(())
}

/// Handles to bound objects were removed (`Registry::take_gone`): every binding of every port made through one of them ends (L21c, T61: closing,
/// replacing or sending the handle a binding was made through ends it). The ports are touched, so that their waiters look again.
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: i < bindings.len() (loop bound); `next` is at most the length and counts the bindings before it
pub fn end_bindings_made_through(reg: &mut Kobjs, jobs: &mut Jobs, gone: &[(ObjId, u64, u64)]) {
    if gone.is_empty() {
        return;
    }
    // (a set, so that a process that dies with thousands of bound handles costs one lookup per binding, not one comparison per pair)
    let gone: alloc::collections::BTreeSet<(ObjId, u64, u64)> = gone.iter().copied().collect();
    let ports: Vec<ObjId> = reg
        .live_ids()
        .into_iter()
        .filter(|&i| matches!(reg.get(i), Some(KObj::Port(_))))
        .collect();
    for port in ports {
        // (looked at without logging it: only a port that loses a binding is touched)
        let any = match reg.get(port) {
            Some(KObj::Port(p)) => p
                .bindings
                .iter()
                .any(|b| gone.contains(&(b.obj, b.via.0, b.via.1))),
            _ => false,
        };
        if !any {
            continue;
        }
        let mut ended: Vec<ObjId> = Vec::new();
        let mut bindings = match reg.get_mut(port) {
            Some(KObj::Port(p)) => core::mem::take(&mut p.bindings),
            _ => continue,
        };
        let mut before_next = 0;
        let next = match reg.get(port) {
            Some(KObj::Port(p)) => p.next,
            _ => 0,
        };
        let mut i = 0;
        bindings.retain(|b| {
            let end = gone.contains(&(b.obj, b.via.0, b.via.1));
            if end {
                ended.push(b.obj);
                if i < next {
                    before_next += 1;
                }
            }
            i += 1;
            !end
        });
        if let Some(KObj::Port(p)) = reg.get_mut(port) {
            p.bindings = bindings;
            p.next -= before_next.min(p.next);
        }
        for o in &ended {
            reg.watch_adjust(*o, -1);
            reg.bound_remove(*o, port);
        }
        reg.meter_sub(ended.len() * BINDING_COST);
        credit_bytes(reg, jobs, port, ended.len() * BINDING_COST);
    }
}

#[allow(clippy::arithmetic_side_effects)] // INVARIANT: next is at least 1 when a binding is removed behind it
pub fn port_unbind_in(
    reg: &mut Kobjs,
    table: &HandleTable,
    port: Handle,
    key: u64,
    jobs: &mut Jobs,
) -> Result<(), Status> {
    let port_obj = port_of(reg, table, port, Rights::MANAGE)?;
    prune_port(reg, jobs, port_obj);
    let Some(KObj::Port(p)) = reg.get_mut(port_obj) else {
        return Err(Status::BadHandle);
    };
    let at = p
        .bindings
        .iter()
        .position(|b| b.key == key)
        .ok_or(Status::InvalidArgs)?;
    let gone_obj = p.bindings.remove(at).obj;
    if p.next > at {
        p.next -= 1;
    }
    reg.watch_adjust(gone_obj, -1);
    reg.bound_remove(gone_obj, port_obj);
    reg.meter_sub(BINDING_COST);
    credit_bytes(reg, jobs, port_obj, BINDING_COST);
    Ok(())
}

/// One ready binding as `(key, observed)`, rotating, after pruning bindings whose object is
/// gone; `None` if nothing is ready. Needs WAIT on the port.
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: n is the binding count and is non-zero here; next < n
pub fn port_poll_in(
    reg: &mut Kobjs,
    table: &HandleTable,
    port: Handle,
    jobs: &mut Jobs,
) -> Result<Option<(u64, u32)>, Status> {
    let port_obj = port_of(reg, table, port, Rights::WAIT)?;
    prune_port(reg, jobs, port_obj);
    let picked = match reg.get(port_obj) {
        Some(KObj::Port(p)) => {
            let n = p.bindings.len();
            (0..n)
                .map(|i| (p.next + i) % n)
                .find(|&i| binding_ready(reg, &p.bindings[i]))
                .map(|i| (i, p.bindings[i].key, p.bindings[i].mask, p.bindings[i].obj))
        }
        _ => return Err(Status::BadHandle),
    };
    let observed =
        picked.and_then(|(_, key, mask, obj)| object_signals(reg, obj).map(|s| (key, s & mask)));
    let Some(KObj::Port(p)) = reg.get_mut(port_obj) else {
        return Err(Status::BadHandle);
    };
    if let Some((i, ..)) = picked {
        p.next = i + 1;
        if p.bindings[i].oneshot {
            p.bindings[i].armed = false; // delivered: stays quiet until `port_rearm`
        }
    }
    Ok(observed)
}

/// Up to `max` ready bindings at once (`port_wait_many`, AB4): each ready binding at most once, in the order `max` successive
/// `port_poll_in` calls would give them, with the same effects (the cursor moves past the last one given, a delivered one-shot binding
/// disarms). Empty if nothing is ready. Needs WAIT on the port.
#[allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: n is the binding count and is non-zero in the loop; next < n
pub fn port_poll_many_in(
    reg: &mut Kobjs,
    table: &HandleTable,
    port: Handle,
    jobs: &mut Jobs,
    max: usize,
) -> Result<Vec<(u64, u32)>, Status> {
    let port_obj = port_of(reg, table, port, Rights::WAIT)?;
    prune_port(reg, jobs, port_obj);
    // (both lists are reserved before anything is taken: NO_MEMORY here leaves the port as it was, T248)
    let mut picked: Vec<(usize, u64, u32, ObjId)> = Vec::new();
    let mut observed: Vec<(u64, u32)> = Vec::new();
    match reg.get(port_obj) {
        Some(KObj::Port(p)) => {
            let n = p.bindings.len();
            let room = n.min(max);
            picked
                .try_reserve_exact(room)
                .map_err(|_| Status::NoMemory)?;
            observed
                .try_reserve_exact(room)
                .map_err(|_| Status::NoMemory)?;
            picked.extend(
                (0..n)
                    .map(|i| (p.next + i) % n)
                    .filter(|&i| binding_ready(reg, &p.bindings[i]))
                    .take(max)
                    .map(|i| (i, p.bindings[i].key, p.bindings[i].mask, p.bindings[i].obj)),
            );
        }
        _ => return Err(Status::BadHandle),
    }
    observed.extend(
        picked
            .iter()
            .filter_map(|&(_, key, mask, obj)| object_signals(reg, obj).map(|s| (key, s & mask))),
    );
    let Some(KObj::Port(p)) = reg.get_mut(port_obj) else {
        return Err(Status::BadHandle);
    };
    for &(i, ..) in &picked {
        p.next = i + 1;
        if p.bindings[i].oneshot {
            p.bindings[i].armed = false;
        }
    }
    Ok(observed)
}

/// Arm binding `key` again (a one-shot binding that was delivered; harmless on one that is armed): it
/// is ready at once if its object's signals match now. Rights: MANAGE on the port.
pub fn port_rearm_in(
    reg: &mut Kobjs,
    table: &HandleTable,
    port: Handle,
    key: u64,
    jobs: &mut Jobs,
) -> Result<(), Status> {
    let port_obj = port_of(reg, table, port, Rights::MANAGE)?;
    prune_port(reg, jobs, port_obj);
    let Some(KObj::Port(p)) = reg.get_mut(port_obj) else {
        return Err(Status::BadHandle);
    };
    let b = p
        .bindings
        .iter_mut()
        .find(|b| b.key == key)
        .ok_or(Status::InvalidArgs)?;
    b.armed = true;
    Ok(())
}

/// The identity of the object a handle names, and of its peer (0 if it has none or the peer is
/// gone): `(koid, peer_koid)`. A process can use it to see that two handles it holds are the two
/// ends of one channel.
pub fn object_info(reg: &Kobjs, table: &HandleTable, h: Handle) -> Result<(u64, u64), Status> {
    let entry = table.get(h)?;
    let obj = reg.get(entry.obj).ok_or(Status::BadHandle)?;
    let peer = obj
        .peer()
        .filter(|&p| reg.is_alive(p))
        .and_then(|p| reg.koid(p))
        .unwrap_or(0);
    Ok((reg.koid(entry.obj).ok_or(Status::BadHandle)?, peer))
}

/// A VMO object for VMO store id `id`, with a handle in `table` (default VMO rights).
/// The root resource object, with a handle carrying `Rights::RESOURCE`.
#[allow(clippy::expect_used)] // INVARIANT: the table's room was checked at the top
pub fn resource_object_create_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    q: &mut Quota,
) -> Result<Handle, Status> {
    if table.room() < 1 || table.reserve(1).is_err() || reg.live() >= reg.cap() {
        return Err(Status::NoMemory);
    }
    q.objects(reg, 1)?;
    let obj = reg.create_charged(KObj::Resource, q.job);
    Ok(table
        .insert(reg, obj, Rights::RESOURCE)
        .expect("room was checked"))
}

/// The debug resource object, with a handle carrying `Rights::DEBUG` (HD4).
#[allow(clippy::expect_used)] // INVARIANT: the table's room was checked at the top
pub fn debug_object_create_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    q: &mut Quota,
) -> Result<Handle, Status> {
    if table.room() < 1 || table.reserve(1).is_err() || reg.live() >= reg.cap() {
        return Err(Status::NoMemory);
    }
    q.objects(reg, 1)?;
    let obj = reg.create_charged(KObj::Debug, q.job);
    Ok(table
        .insert(reg, obj, Rights::DEBUG)
        .expect("room was checked"))
}

/// Check that `h` is a debug resource handle with the right `need` (WRITE to write, INSPECT to read a counter).
pub fn debug_of(reg: &Kobjs, table: &HandleTable, h: Handle, need: Rights) -> Result<(), Status> {
    let entry = table.get(h)?;
    match reg.get(entry.obj) {
        Some(KObj::Debug) => {
            if entry.rights.contains(need) {
                Ok(())
            } else {
                Err(Status::AccessDenied)
            }
        }
        Some(_) => Err(Status::WrongType),
        None => Err(Status::BadHandle),
    }
}

/// A new interrupt object for PLIC source `irq` (the caller has checked the resource, the range
/// and that no live object already owns the source).
#[allow(clippy::expect_used)] // INVARIANT: the table's room was checked at the top
pub fn interrupt_object_create_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    irq: u32,
    q: &mut Quota,
) -> Result<(Handle, ObjId), Status> {
    if table.room() < 1 || table.reserve(1).is_err() || reg.live() >= reg.cap() {
        return Err(Status::NoMemory);
    }
    q.objects(reg, 1)?;
    let obj = reg.create_charged(
        KObj::Interrupt(IrqObj {
            irq,
            fired: false,
            claims: 0,
        }),
        q.job,
    );
    let h = table
        .insert(reg, obj, Rights::INTERRUPT)
        .expect("room was checked");
    Ok((h, obj))
}

/// The source of an interrupt object handle that carries `need`.
pub fn interrupt_of(
    reg: &Kobjs,
    table: &HandleTable,
    h: Handle,
    need: Rights,
) -> Result<(ObjId, u32), Status> {
    let entry = table.get(h)?;
    match reg.get(entry.obj) {
        Some(KObj::Interrupt(i)) => {
            if entry.rights.contains(need) {
                Ok((entry.obj, i.irq))
            } else {
                Err(Status::AccessDenied)
            }
        }
        Some(_) => Err(Status::WrongType),
        None => Err(Status::BadHandle),
    }
}

/// The source fired: returns true if `obj` is a live interrupt object for `irq` that was not
/// already waiting for its ack (the caller has already masked the source).
#[allow(clippy::arithmetic_side_effects)] // INVARIANT: a u64 claim counter
pub fn interrupt_fire(reg: &mut Kobjs, obj: ObjId, irq: u32) -> bool {
    match reg.get_mut(obj) {
        Some(KObj::Interrupt(i)) if i.irq == irq => {
            i.claims += 1;
            let first = !i.fired;
            i.fired = true;
            first
        }
        _ => false,
    }
}

/// Acknowledge: BAD_STATE unless the source fired. Returns the source to unmask and the number
/// of claims so far.
pub fn interrupt_ack(reg: &mut Kobjs, obj: ObjId) -> Result<(u32, u64), Status> {
    match reg.get_mut(obj) {
        Some(KObj::Interrupt(i)) if i.fired => {
            i.fired = false;
            Ok((i.irq, i.claims))
        }
        Some(KObj::Interrupt(_)) => Err(Status::BadState),
        _ => Err(Status::BadHandle),
    }
}

/// A new device-domain object for store domain `domain`, paid for by `job` (whose use it takes: released by the object's cleanup), with a
/// handle carrying `Rights::IODOMAIN` (IOMMU-SPEC 4.1). The object and the handle are charged to `q`'s job (the caller's).
#[allow(clippy::expect_used)] // INVARIANT: the table's room was checked at the top
pub fn iodomain_object_create_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    domain: u64,
    job: JobId,
    q: &mut Quota,
) -> Result<(Handle, ObjId), Status> {
    if table.room() < 1 || table.reserve(1).is_err() || reg.live() >= reg.cap() {
        return Err(Status::NoMemory);
    }
    q.objects(reg, 1)?;
    q.jobs.retain(job);
    let obj = reg.create_charged(
        KObj::IoDomain(IoDomainObj {
            domain,
            job,
            fault: false,
            destroyed: false,
        }),
        q.job,
    );
    let h = table
        .insert(reg, obj, Rights::IODOMAIN)
        .expect("room was checked");
    Ok((h, obj))
}

/// The domain of a device-domain handle that carries `need`: (object, store domain, job).
pub fn iodomain_of(
    reg: &Kobjs,
    table: &HandleTable,
    h: Handle,
    need: Rights,
) -> Result<IoDomainRef, Status> {
    let entry = table.get(h)?;
    match reg.get(entry.obj) {
        Some(KObj::IoDomain(d)) => {
            if entry.rights.contains(need) {
                Ok(IoDomainRef {
                    obj: entry.obj,
                    domain: d.domain,
                    job: d.job,
                    destroyed: d.destroyed,
                })
            } else {
                Err(Status::AccessDenied)
            }
        }
        Some(_) => Err(Status::WrongType),
        None => Err(Status::BadHandle),
    }
}

/// Raise or clear DEVICE_FAULT on a device-domain object (the fault path and `iodomain_fault_read`); false if `obj` is not one.
pub fn iodomain_set_fault(reg: &mut Kobjs, obj: ObjId, fault: bool) -> bool {
    match reg.get_mut(obj) {
        Some(KObj::IoDomain(d)) => {
            d.fault = fault;
            true
        }
        _ => false,
    }
}

/// Mark a device-domain object destroyed (`iodomain_destroy`); true if it was not already.
pub fn iodomain_mark_destroyed(reg: &mut Kobjs, obj: ObjId) -> bool {
    match reg.get_mut(obj) {
        Some(KObj::IoDomain(d)) if !d.destroyed => {
            d.destroyed = true;
            true
        }
        _ => false,
    }
}

/// Check that `h` is a root resource handle with MANAGE (the right to mint).
pub fn resource_of(reg: &Kobjs, table: &HandleTable, h: Handle) -> Result<(), Status> {
    let entry = table.get(h)?;
    match reg.get(entry.obj) {
        Some(KObj::Resource) => {
            if entry.rights.contains(Rights::MANAGE) {
                Ok(())
            } else {
                Err(Status::AccessDenied)
            }
        }
        Some(_) => Err(Status::WrongType),
        None => Err(Status::BadHandle),
    }
}

pub fn vmo_object_create_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    id: u64,
    q: &mut Quota,
) -> Result<Handle, Status> {
    vmo_object_create_with(reg, table, id, Rights::VMO, q)
}

/// A VMO object with a handle carrying exactly `rights`.
#[allow(clippy::expect_used)] // INVARIANT: the table's room was checked at the top
pub fn vmo_object_create_with(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    id: u64,
    rights: Rights,
    q: &mut Quota,
) -> Result<Handle, Status> {
    if table.room() < 1 || table.reserve(1).is_err() || reg.live() >= reg.cap() {
        return Err(Status::NoMemory);
    }
    q.objects(reg, 1)?;
    let obj = reg.create_charged(KObj::Vmo(id), q.job);
    Ok(table.insert(reg, obj, rights).expect("room was checked"))
}

/// A job object for ledger job `id` (whose use the object takes over), with a handle in
/// `table` carrying `rights`.
#[allow(clippy::expect_used)] // INVARIANT: the table's room was checked at the top
pub fn job_object_create(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    id: JobId,
    rights: Rights,
    q: &mut Quota,
) -> Result<Handle, Status> {
    if table.room() < 1 || table.reserve(1).is_err() || reg.live() >= reg.cap() {
        return Err(Status::NoMemory);
    }
    q.objects(reg, 1)?;
    let obj = reg.create_charged(KObj::Job(id), q.job);
    Ok(table.insert(reg, obj, rights).expect("room was checked"))
}

/// The job behind a job handle that carries `need`.
pub fn job_of(reg: &Kobjs, table: &HandleTable, h: Handle, need: Rights) -> Result<JobId, Status> {
    let entry = table.get(h)?;
    match reg.get(entry.obj) {
        Some(KObj::Job(id)) => {
            if entry.rights.contains(need) {
                Ok(*id)
            } else {
                Err(Status::AccessDenied)
            }
        }
        Some(_) => Err(Status::WrongType),
        None => Err(Status::BadHandle),
    }
}

/// A pager object (MM9) with a handle in `table`, charged to `q`.
#[allow(clippy::expect_used)] // INVARIANT: the table's room was checked at the top
pub fn pager_object_create_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    id: u64,
    rights: Rights,
    q: &mut Quota,
) -> Result<(Handle, ObjId), Status> {
    if table.room() < 1 || table.reserve(1).is_err() || reg.live() >= reg.cap() {
        return Err(Status::NoMemory);
    }
    q.objects(reg, 1)?;
    let obj = reg.create_charged(KObj::Pager(id), q.job);
    Ok((
        table.insert(reg, obj, rights).expect("room was checked"),
        obj,
    ))
}

/// The pager id behind a pager handle that carries `need`.
pub fn pager_of(reg: &Kobjs, table: &HandleTable, h: Handle, need: Rights) -> Result<u64, Status> {
    let entry = table.get(h)?;
    match reg.get(entry.obj) {
        Some(KObj::Pager(id)) => {
            if entry.rights.contains(need) {
                Ok(*id)
            } else {
                Err(Status::AccessDenied)
            }
        }
        Some(_) => Err(Status::WrongType),
        None => Err(Status::BadHandle),
    }
}

/// The VMO store id behind a VMO handle that carries `need`.
pub fn vmo_of(reg: &Kobjs, table: &HandleTable, h: Handle, need: Rights) -> Result<u64, Status> {
    let entry = table.get(h)?;
    match reg.get(entry.obj) {
        Some(KObj::Vmo(id)) => {
            if entry.rights.contains(need) {
                Ok(*id)
            } else {
                Err(Status::AccessDenied)
            }
        }
        Some(_) => Err(Status::WrongType),
        None => Err(Status::BadHandle),
    }
}

/// A process object for `pid`, with a handle in `table` (default process rights).
#[allow(clippy::expect_used)] // INVARIANT: the table's room was checked at the top
pub fn process_object_create_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    pid: u64,
    kill_on_close: bool,
    q: &mut Quota,
) -> Result<(Handle, ObjId), Status> {
    if table.room() < 1 || table.reserve(1).is_err() || reg.live() >= reg.cap() {
        return Err(Status::NoMemory);
    }
    q.objects(reg, 1)?;
    let obj = reg.create_charged(
        KObj::Process(ProcessObj {
            pid,
            terminated: false,
            stopped: false,
            exit_code: 0,
            cpu_ns: 0,
            kill_on_close,
        }),
        q.job,
    );
    Ok((
        table
            .insert(reg, obj, Rights::PROCESS)
            .expect("room was checked"),
        obj,
    ))
}

/// The process object behind a process handle that carries `need`: its id and state.
pub fn process_of(
    reg: &Kobjs,
    table: &HandleTable,
    h: Handle,
    need: Rights,
) -> Result<(ObjId, u64, bool, i32), Status> {
    let entry = table.get(h)?;
    match reg.get(entry.obj) {
        Some(KObj::Process(p)) => {
            if entry.rights.contains(need) {
                Ok((entry.obj, p.pid, p.terminated, p.exit_code))
            } else {
                Err(Status::AccessDenied)
            }
        }
        Some(_) => Err(Status::WrongType),
        None => Err(Status::BadHandle),
    }
}

/// Record that the process behind `obj` has ended (raises TERMINATED).
pub fn process_mark_terminated(reg: &mut Kobjs, obj: ObjId, code: i32) {
    if let Some(KObj::Process(p)) = reg.get_mut(obj) {
        p.terminated = true;
        p.exit_code = code;
    }
}

/// Record that the process behind `obj` was stopped or continued (raises or clears STOPPED).
pub fn process_set_stopped(reg: &mut Kobjs, obj: ObjId, stopped: bool) {
    if let Some(KObj::Process(p)) = reg.get_mut(obj) {
        p.stopped = stopped;
    }
}

/// Record that the thread behind `obj` has stopped for `thread_suspend` or runs again (raises or clears SUSPENDED).
/// Record the kernel thread a thread object stands for.
pub fn thread_set_tid(reg: &mut Kobjs, obj: ObjId, tid: u64) {
    if let Some(KObj::Thread(t)) = reg.get_mut(obj) {
        t.tid = Some(tid);
    }
}

pub fn thread_set_suspended(reg: &mut Kobjs, obj: ObjId, suspended: bool) {
    if let Some(KObj::Thread(t)) = reg.get_mut(obj) {
        t.suspended = suspended && !t.terminated;
    }
}

/// An exception object, with a handle in `table` (the kernel's own table, from which the handle travels in the message), charged to `q`.
#[allow(clippy::expect_used)] // INVARIANT: the table's room was checked at the top
pub fn exception_object_create_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    e: ExceptionObj,
    q: &mut Quota,
) -> Result<(Handle, ObjId), Status> {
    if table.room() < 1 || table.reserve(1).is_err() || reg.live() >= reg.cap() {
        return Err(Status::NoMemory);
    }
    q.objects(reg, 1)?;
    let obj = reg.create_charged(KObj::Exception(e), q.job);
    Ok((
        table
            .insert(reg, obj, Rights::EXCEPTION)
            .expect("room was checked"),
        obj,
    ))
}

/// The exception behind a handle that carries `need`: a copy of what it says and whether it is resolved.
pub fn exception_of(
    reg: &Kobjs,
    table: &HandleTable,
    h: Handle,
    need: Rights,
) -> Result<(ObjId, ExceptionObj), Status> {
    let entry = table.get(h)?;
    match reg.get(entry.obj) {
        Some(KObj::Exception(e)) => {
            if entry.rights.contains(need) {
                Ok((
                    entry.obj,
                    ExceptionObj {
                        pid: e.pid,
                        tid: e.tid,
                        kind: e.kind,
                        pc: e.pc,
                        address: e.address,
                        cause: e.cause,
                        level: e.level,
                        serial: e.serial,
                        resolved: e.resolved,
                    },
                ))
            } else {
                Err(Status::AccessDenied)
            }
        }
        Some(_) => Err(Status::WrongType),
        None => Err(Status::BadHandle),
    }
}

/// Mark the exception object resolved (it is acted on once).
/// Has the peer of channel end `h` (a handle in `table`) closed its end?
pub fn channel_peer_closed(reg: &Kobjs, table: &HandleTable, h: Handle) -> bool {
    match table.get(h).ok().and_then(|e| reg.get(e.obj)) {
        Some(KObj::Channel(c)) => c.peer_closed,
        _ => true,
    }
}

pub fn exception_set_resolved(reg: &mut Kobjs, obj: ObjId) {
    if let Some(KObj::Exception(e)) = reg.get_mut(obj) {
        e.resolved = true;
    }
}

/// A thread object for a user thread of `pid`, with a handle in `table` (default thread rights).
#[allow(clippy::expect_used)] // INVARIANT: the table's room was checked at the top
pub fn thread_object_create_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    pid: u64,
    q: &mut Quota,
) -> Result<(Handle, ObjId), Status> {
    if table.room() < 1 || table.reserve(1).is_err() || reg.live() >= reg.cap() {
        return Err(Status::NoMemory);
    }
    q.objects(reg, 1)?;
    let obj = reg.create_charged(
        KObj::Thread(ThreadObj {
            pid,
            tid: None,
            terminated: false,
            suspended: false,
            exit_code: 0,
            cpu_ns: 0,
        }),
        q.job,
    );
    Ok((
        table
            .insert(reg, obj, Rights::THREAD)
            .expect("room was checked"),
        obj,
    ))
}

/// The thread object behind a thread handle that carries `need`: (terminated, exit code).
pub fn thread_of(
    reg: &Kobjs,
    table: &HandleTable,
    h: Handle,
    need: Rights,
) -> Result<(bool, i32), Status> {
    let entry = table.get(h)?;
    match reg.get(entry.obj) {
        Some(KObj::Thread(t)) => {
            if entry.rights.contains(need) {
                Ok((t.terminated, t.exit_code))
            } else {
                Err(Status::AccessDenied)
            }
        }
        Some(_) => Err(Status::WrongType),
        None => Err(Status::BadHandle),
    }
}

/// What `thread_info` 1 needs (AB6): the thread object, its process, whether it has ended, and its final CPU nanoseconds (meaningful once ended).
pub fn thread_cpu_info(
    reg: &Kobjs,
    table: &HandleTable,
    h: Handle,
    need: Rights,
) -> Result<(ObjId, u64, bool, u64), Status> {
    let entry = table.get(h)?;
    match reg.get(entry.obj) {
        Some(KObj::Thread(t)) => {
            if entry.rights.contains(need) {
                Ok((entry.obj, t.pid, t.terminated, t.cpu_ns))
            } else {
                Err(Status::AccessDenied)
            }
        }
        Some(_) => Err(Status::WrongType),
        None => Err(Status::BadHandle),
    }
}

/// What `process_info` 1 needs (AB6): its pid, whether it has ended, and the CPU nanoseconds of its ended threads (all of them once it has ended).
pub fn process_cpu_info(
    reg: &Kobjs,
    table: &HandleTable,
    h: Handle,
    need: Rights,
) -> Result<(u64, bool, u64), Status> {
    let entry = table.get(h)?;
    match reg.get(entry.obj) {
        Some(KObj::Process(p)) => {
            if entry.rights.contains(need) {
                Ok((p.pid, p.terminated, p.cpu_ns))
            } else {
                Err(Status::AccessDenied)
            }
        }
        Some(_) => Err(Status::WrongType),
        None => Err(Status::BadHandle),
    }
}

/// A thread ended having run `ns` nanoseconds: the thread object keeps it, and the process object's total grows by it.
pub fn thread_cpu_final(
    reg: &mut Kobjs,
    thread_obj: Option<ObjId>,
    process_obj: Option<ObjId>,
    ns: u64,
) {
    if let Some(KObj::Thread(t)) = thread_obj.and_then(|o| reg.get_mut(o)) {
        t.cpu_ns = ns;
    }
    if let Some(KObj::Process(p)) = process_obj.and_then(|o| reg.get_mut(o)) {
        p.cpu_ns = p.cpu_ns.saturating_add(ns);
    }
}

/// Record that the thread behind `obj` has ended (raises TERMINATED). A thread object that
/// has already been destroyed (every handle closed) is ignored.
pub fn thread_mark_terminated(reg: &mut Kobjs, obj: ObjId, code: i32) {
    if let Some(KObj::Thread(t)) = reg.get_mut(obj) {
        t.terminated = true;
        t.exit_code = code;
    }
}

// --- convenience calls for registries with no charged objects (the pure tests). They settle into a
// scratch ledger, so they must not be used on a registry that holds charged objects. ---

pub fn channel_create(
    reg: &mut Kobjs,
    table: &mut HandleTable,
) -> Result<(Handle, Handle), Status> {
    channel_create_in(
        reg,
        table,
        &mut Quota {
            jobs: &mut Jobs::new(),
            job: None,
        },
    )
}
pub fn event_create(reg: &mut Kobjs, table: &mut HandleTable) -> Result<Handle, Status> {
    event_create_in(
        reg,
        table,
        &mut Quota {
            jobs: &mut Jobs::new(),
            job: None,
        },
    )
}
pub fn socket_create(reg: &mut Kobjs, table: &mut HandleTable) -> Result<(Handle, Handle), Status> {
    socket_create_in(
        reg,
        table,
        &mut Quota {
            jobs: &mut Jobs::new(),
            job: None,
        },
    )
}
pub fn port_create(reg: &mut Kobjs, table: &mut HandleTable) -> Result<Handle, Status> {
    port_create_in(
        reg,
        table,
        &mut Quota {
            jobs: &mut Jobs::new(),
            job: None,
        },
    )
}
pub fn vmo_object_create(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    id: u64,
) -> Result<Handle, Status> {
    vmo_object_create_in(
        reg,
        table,
        id,
        &mut Quota {
            jobs: &mut Jobs::new(),
            job: None,
        },
    )
}
pub fn process_object_create(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    pid: u64,
    kill_on_close: bool,
) -> Result<(Handle, ObjId), Status> {
    process_object_create_in(
        reg,
        table,
        pid,
        kill_on_close,
        &mut Quota {
            jobs: &mut Jobs::new(),
            job: None,
        },
    )
}
pub fn thread_object_create(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    pid: u64,
) -> Result<(Handle, ObjId), Status> {
    thread_object_create_in(
        reg,
        table,
        pid,
        &mut Quota {
            jobs: &mut Jobs::new(),
            job: None,
        },
    )
}
pub fn channel_write(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    ch: Handle,
    data: &[u8],
    handles: &[Handle],
) -> Result<(), Status> {
    channel_write_in(reg, table, ch, data, handles, &mut Jobs::new())
}
pub fn channel_read(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    ch: Handle,
    max_bytes: usize,
    max_handles: usize,
) -> Result<Received, ReadError> {
    channel_read_in(reg, table, ch, max_bytes, max_handles, &mut Jobs::new())
}
pub fn channel_read_txid(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    ch: Handle,
    txid: u32,
    max_bytes: usize,
    max_handles: usize,
) -> Result<Received, ReadError> {
    channel_read_txid_in(
        reg,
        table,
        ch,
        txid,
        max_bytes,
        max_handles,
        &mut Jobs::new(),
    )
}
pub fn socket_write(
    reg: &mut Kobjs,
    table: &HandleTable,
    h: Handle,
    data: &[u8],
) -> Result<usize, Status> {
    socket_write_in(reg, table, h, data, &mut Jobs::new())
}
pub fn socket_read(
    reg: &mut Kobjs,
    table: &HandleTable,
    h: Handle,
    max: usize,
) -> Result<Vec<u8>, Status> {
    socket_read_in(reg, table, h, max, &mut Jobs::new())
}
pub fn port_bind(
    reg: &mut Kobjs,
    table: &HandleTable,
    port: Handle,
    object: Handle,
    mask: u64,
    key: u64,
) -> Result<(), Status> {
    port_bind_in(reg, table, port, object, mask, key, &mut Jobs::new())
}
pub fn port_unbind(
    reg: &mut Kobjs,
    table: &HandleTable,
    port: Handle,
    key: u64,
) -> Result<(), Status> {
    port_unbind_in(reg, table, port, key, &mut Jobs::new())
}
pub fn port_poll_many(
    reg: &mut Kobjs,
    table: &HandleTable,
    port: Handle,
    max: usize,
) -> Result<Vec<(u64, u32)>, Status> {
    port_poll_many_in(reg, table, port, &mut Jobs::new(), max)
}
pub fn port_poll(
    reg: &mut Kobjs,
    table: &HandleTable,
    port: Handle,
) -> Result<Option<(u64, u32)>, Status> {
    port_poll_in(reg, table, port, &mut Jobs::new())
}
