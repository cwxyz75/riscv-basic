//! Channels and events: the kernel objects of S3a, and the operations on them that
//! take handles (SYSCALLS.md, "Channels" and "Events"). Pure logic over a
//! [`Registry`] and a [`HandleTable`]; the syscall layer only copies bytes in and out.

use crate::handle::{Entry, HandleTable};
use crate::job::{JobId, Jobs, Resource};
use crate::object::{ObjId, Object, Registry};
use abi::limits::{
    MAX_IPC_BYTES, MAX_MSG_BYTES, MAX_MSG_HANDLES, MAX_OBJECTS, MAX_PORT_BINDINGS,
    MAX_QUEUED_BYTES, MAX_QUEUED_MSGS, SOCKET_BUFFER,
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
}

/// Kernel memory a queued message costs beyond its bytes (the queue node and the `Message`).
const MSG_OVERHEAD: usize = 64;
/// What one port binding costs.
const BINDING_COST: usize = 32;

impl Message {
    /// What this message counts against the IPC budget: bytes, a fixed overhead, and its
    /// handle entries (they are kernel memory too: found by the trust-boundary audit).
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
    pub pid: u32,
    pub terminated: bool,
    pub exit_code: i32,
    /// Destroying this object (the last handle closing) kills a still-running process. False
    /// for processes the kernel itself started: their `process_self` handles are only views.
    pub kill_on_close: bool,
}

/// A user thread, as a kernel object: what a thread handle names.
pub struct ThreadObj {
    pub pid: u32,
    pub terminated: bool,
    pub exit_code: i32,
}

/// One end of a byte-stream socket: what it holds is what its *peer* wrote.
pub struct SocketEnd {
    peer: Option<ObjId>,
    buf: VecDeque<u8>,
    peer_closed: bool,
}

/// One binding of a port: watch `obj` for `mask`, report as `key`.
struct Binding {
    obj: ObjId,
    mask: u32,
    key: u64,
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
    Socket(SocketEnd),
    Port(PortObj),
    /// A job (S4.5): the ledger id; the object holds one use of it.
    Job(JobId),
}

impl Object for KObj {
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
        matches!(self, KObj::Vmo(_) | KObj::Process(_) | KObj::Job(_))
    }

    fn meter(&self) -> usize {
        match self {
            KObj::Channel(c) => c.queue.iter().map(Message::cost).sum(),
            KObj::Socket(s) => s.buf.len(),
            KObj::Port(p) => p.bindings.len() * BINDING_COST,
            _ => 0,
        }
    }

    fn charged_bytes(&self) -> usize {
        match self {
            KObj::Channel(c) => c.queue.iter().map(Message::cost).sum(),
            KObj::Socket(s) => s.buf.len(),
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
        match self.job {
            Some(job) => {
                self.jobs
                    .charge(job, Resource::Objects, n)
                    .map_err(|_| Status::NoMemory)?;
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
    for (job, bytes) in reg.take_credits() {
        jobs.credit(job, Resource::Objects, 1);
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
pub fn channel_create_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    q: &mut Quota,
) -> Result<(Handle, Handle), Status> {
    if table.room() < 2 || reg.live() + 2 > MAX_OBJECTS {
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
    let ha = table.insert(a, Rights::CHANNEL).expect("room was checked");
    let hb = table.insert(b, Rights::CHANNEL).expect("room was checked");
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
    if reg.meter() + cost > MAX_IPC_BYTES {
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
            .map(|&h| table.remove_for_transfer(h).expect("validated")),
    );
    if let Some(KObj::Channel(p)) = reg.get_mut(peer) {
        p.queue.push_back(Message {
            data: bytes,
            handles: entries,
        });
        reg.meter_add(cost);
    }
    Ok(())
}

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
    let msg = c.queue.remove(i).expect("picked");
    reg.meter_sub(msg.cost());
    credit_bytes(reg, jobs, me, msg.cost());
    let handles = msg
        .handles
        .iter()
        .map(|e| table.insert(e.obj, e.rights).expect("room was checked"))
        .collect();
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
pub fn event_create_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    q: &mut Quota,
) -> Result<Handle, Status> {
    if table.room() < 1 || reg.live() >= MAX_OBJECTS {
        return Err(Status::NoMemory);
    }
    q.objects(reg, 1)?;
    let id = reg.create_charged(KObj::Event(Event { signals: 0 }), q.job);
    Ok(table.insert(id, Rights::EVENT).expect("room was checked"))
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

/// The signal word of a live object (None if it is gone).
fn object_signals(reg: &Kobjs, obj: ObjId) -> Option<u32> {
    Some(match reg.get(obj)? {
        KObj::Event(e) => e.signals,
        KObj::Vmo(_) | KObj::Job(_) => 0,
        KObj::Process(p) => {
            if p.terminated {
                signals::TERMINATED
            } else {
                0
            }
        }
        KObj::Thread(t) => {
            if t.terminated {
                signals::TERMINATED
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
                if p.buf.len() < SOCKET_BUFFER {
                    s |= signals::WRITABLE;
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
    matches!(object_signals(reg, b.obj), Some(s) if s & b.mask != 0)
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
pub fn socket_create_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    q: &mut Quota,
) -> Result<(Handle, Handle), Status> {
    if table.room() < 2 || reg.live() + 2 > MAX_OBJECTS {
        return Err(Status::NoMemory);
    }
    q.objects(reg, 2)?;
    let end = || SocketEnd {
        peer: None,
        buf: VecDeque::new(),
        peer_closed: false,
    };
    let a = reg.create_charged(KObj::Socket(end()), q.job);
    let b = reg.create_charged(KObj::Socket(end()), q.job);
    for (me, other) in [(a, b), (b, a)] {
        if let Some(KObj::Socket(s)) = reg.get_mut(me) {
            s.peer = Some(other);
        }
    }
    let ha = table.insert(a, Rights::SOCKET).expect("room was checked");
    let hb = table.insert(b, Rights::SOCKET).expect("room was checked");
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
    let global_room = MAX_IPC_BYTES.saturating_sub(meter);
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
        return Err(if s.peer_closed {
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
    let out: Vec<u8> = s.buf.drain(..n).collect();
    if s.buf.is_empty() {
        // give the memory back: an idle socket holds nothing
        s.buf = VecDeque::new();
    }
    reg.meter_sub(n);
    credit_bytes(reg, jobs, me, n);
    Ok(out)
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

pub fn port_create_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    q: &mut Quota,
) -> Result<Handle, Status> {
    if table.room() < 1 || reg.live() >= MAX_OBJECTS {
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
    Ok(table.insert(obj, Rights::PORT).expect("room was checked"))
}

/// Drop the bindings whose object has been destroyed (the spec says they disappear at once,
/// so every operation that looks at the binding set prunes first).
fn prune_port(reg: &mut Kobjs, port_obj: ObjId) {
    let dead: Vec<u64> = match reg.get(port_obj) {
        Some(KObj::Port(p)) => p
            .bindings
            .iter()
            .filter(|b| !reg.is_alive(b.obj))
            .map(|b| b.key)
            .collect(),
        _ => return,
    };
    let mut removed = 0;
    if let Some(KObj::Port(p)) = reg.get_mut(port_obj) {
        for k in dead {
            if let Some(at) = p.bindings.iter().position(|b| b.key == k) {
                p.bindings.remove(at);
                removed += 1;
                if p.next > at {
                    p.next -= 1;
                }
            }
        }
    }
    reg.meter_sub(removed * BINDING_COST);
}

pub fn port_bind(
    reg: &mut Kobjs,
    table: &HandleTable,
    port: Handle,
    object: Handle,
    mask: u64,
    key: u64,
) -> Result<(), Status> {
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
    let mask = mask as u32;
    prune_port(reg, port_obj);
    let Some(KObj::Port(p)) = reg.get_mut(port_obj) else {
        return Err(Status::BadHandle);
    };
    if p.bindings.iter().any(|b| b.key == key) {
        return Err(Status::InvalidArgs);
    }
    if p.bindings.len() >= MAX_PORT_BINDINGS {
        return Err(Status::NoMemory);
    }
    p.bindings.try_reserve(1).map_err(|_| Status::NoMemory)?;
    p.bindings.push(Binding {
        obj: oe.obj,
        mask,
        key,
    });
    reg.meter_add(BINDING_COST);
    Ok(())
}

pub fn port_unbind(
    reg: &mut Kobjs,
    table: &HandleTable,
    port: Handle,
    key: u64,
) -> Result<(), Status> {
    let port_obj = port_of(reg, table, port, Rights::MANAGE)?;
    prune_port(reg, port_obj);
    let Some(KObj::Port(p)) = reg.get_mut(port_obj) else {
        return Err(Status::BadHandle);
    };
    let at = p
        .bindings
        .iter()
        .position(|b| b.key == key)
        .ok_or(Status::InvalidArgs)?;
    p.bindings.remove(at);
    if p.next > at {
        p.next -= 1;
    }
    reg.meter_sub(BINDING_COST);
    Ok(())
}

/// One ready binding as `(key, observed)`, rotating, after pruning bindings whose object is
/// gone; `None` if nothing is ready. Needs WAIT on the port.
pub fn port_poll(
    reg: &mut Kobjs,
    table: &HandleTable,
    port: Handle,
) -> Result<Option<(u64, u32)>, Status> {
    let port_obj = port_of(reg, table, port, Rights::WAIT)?;
    prune_port(reg, port_obj);
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
    }
    Ok(observed)
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
        .map_or(0, |p| p.koid());
    Ok((entry.obj.koid(), peer))
}

/// A VMO object for VMO store id `id`, with a handle in `table` (default VMO rights).
pub fn vmo_object_create_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    id: u64,
    q: &mut Quota,
) -> Result<Handle, Status> {
    if table.room() < 1 || reg.live() >= MAX_OBJECTS {
        return Err(Status::NoMemory);
    }
    q.objects(reg, 1)?;
    let obj = reg.create_charged(KObj::Vmo(id), q.job);
    Ok(table.insert(obj, Rights::VMO).expect("room was checked"))
}

/// A job object for ledger job `id` (whose use the object takes over), with a handle in
/// `table` carrying `rights`.
pub fn job_object_create(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    id: JobId,
    rights: Rights,
    q: &mut Quota,
) -> Result<Handle, Status> {
    if table.room() < 1 || reg.live() >= MAX_OBJECTS {
        return Err(Status::NoMemory);
    }
    q.objects(reg, 1)?;
    let obj = reg.create_charged(KObj::Job(id), q.job);
    Ok(table.insert(obj, rights).expect("room was checked"))
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
pub fn process_object_create_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    pid: u32,
    kill_on_close: bool,
    q: &mut Quota,
) -> Result<(Handle, ObjId), Status> {
    if table.room() < 1 || reg.live() >= MAX_OBJECTS {
        return Err(Status::NoMemory);
    }
    q.objects(reg, 1)?;
    let obj = reg.create_charged(
        KObj::Process(ProcessObj {
            pid,
            terminated: false,
            exit_code: 0,
            kill_on_close,
        }),
        q.job,
    );
    Ok((
        table
            .insert(obj, Rights::PROCESS)
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
) -> Result<(ObjId, u32, bool, i32), Status> {
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

/// A thread object for a user thread of `pid`, with a handle in `table` (default thread rights).
pub fn thread_object_create_in(
    reg: &mut Kobjs,
    table: &mut HandleTable,
    pid: u32,
    q: &mut Quota,
) -> Result<(Handle, ObjId), Status> {
    if table.room() < 1 || reg.live() >= MAX_OBJECTS {
        return Err(Status::NoMemory);
    }
    q.objects(reg, 1)?;
    let obj = reg.create_charged(
        KObj::Thread(ThreadObj {
            pid,
            terminated: false,
            exit_code: 0,
        }),
        q.job,
    );
    Ok((
        table.insert(obj, Rights::THREAD).expect("room was checked"),
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

/// Record that the thread behind `obj` has ended (raises TERMINATED). A thread object that
/// has already been destroyed (every handle closed) is ignored.
pub fn thread_mark_terminated(reg: &mut Kobjs, obj: ObjId, code: i32) {
    if let Some(KObj::Thread(t)) = reg.get_mut(obj) {
        t.terminated = true;
        t.exit_code = code;
    }
}

// --- convenience calls that charge nobody (the pure tests, and anything that has no job) ---

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
    pid: u32,
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
    pid: u32,
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
