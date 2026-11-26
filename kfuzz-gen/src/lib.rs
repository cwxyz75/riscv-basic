//! A seeded generator of syscall sequences (KT8). It keeps a small model of what the calling process plausibly holds (live handles by kind,
//! the handles it closed, a window of user memory) and produces the next call: about 70 % valid (the right call on a live handle of the
//! right kind with sane arguments), 25 % near-valid (a wrong kind, a closed handle, a size off by one, a pointer out of the window, a
//! right widened) and 5 % garbage (any syscall number the generator knows, with random or extreme arguments). The same seed gives the same
//! sequence. Pure: `no_std`, no unsafe code, no hardware, and no knowledge of the kernel except the syscall numbers.
//!
//! The harness (the host test over `ksys`, or the guest program) executes each [`Call`] after applying its `pokes` to user memory, and
//! tells the model what happened with [`Model::observe`].
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use abi::syscall as nr;
use alloc::vec::Vec;

/// What a handle is believed to name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Channel,
    Socket,
    Event,
    Port,
    Vmo,
    Job,
    /// Received through a channel: the kind is not known to the model.
    Any,
}

/// The window of user memory the harness maps read-write, and one page past it that is mapped read-only. Pointers the generator calls
/// valid lie inside `[base, base + len)`.
#[derive(Clone, Copy, Debug)]
pub struct Window {
    pub base: u64,
    pub len: u64,
    pub read_only: u64,
}

/// One call to make, and the bytes to put in user memory first (a message body, a handle list, a limits record).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub nr: usize,
    pub args: [u64; 6],
    pub pokes: Vec<(u64, Vec<u8>)>,
}

/// What the generator believes the process holds.
#[derive(Clone, Debug, Default)]
pub struct Model {
    live: Vec<(u64, Kind)>,
    closed: Vec<u64>,
    /// A handle the generator never closes, replaces, sends or sets limits on (the harness's root job handle: the job calls need a job to act
    /// on, and limits on the root would starve every other call).
    pinned: Option<u64>,
}

impl Model {
    /// A model that starts with these handles.
    pub fn with(handles: &[(u64, Kind)]) -> Model {
        Model {
            live: handles.to_vec(),
            closed: Vec::new(),
            pinned: None,
        }
    }

    /// Pin a handle (see `pinned`).
    pub fn pin(&mut self, h: u64) {
        self.pinned = Some(h);
    }

    /// Handles a successful `channel_read` delivered, with the kind the harness learned for each (`Kind::Any` if it cannot tell).
    pub fn received(&mut self, handles: &[(u64, Kind)]) {
        for &(h, k) in handles {
            self.add(h, k);
        }
    }

    pub fn live(&self) -> &[(u64, Kind)] {
        &self.live
    }

    fn add(&mut self, h: u64, k: Kind) {
        self.closed.retain(|&c| c != h);
        self.live.push((h, k));
    }

    fn remove(&mut self, h: u64) {
        if let Some(i) = self.live.iter().position(|&(x, _)| x == h) {
            self.live.swap_remove(i);
            // (a ring: the stale handles are the recent ones too, not only the first 64)
            if self.closed.len() < 64 {
                self.closed.push(h);
            } else {
                let at = (h as usize) % 64;
                if let Some(slot) = self.closed.get_mut(at) {
                    *slot = h;
                }
            }
        }
    }

    /// Tell the model what a call did: `ok` is whether it succeeded, `a1` and `a2` its result registers.
    pub fn observe(&mut self, call: &Call, ok: bool, a1: Option<u64>, a2: Option<u64>) {
        if !ok {
            return;
        }
        let kind_of = |m: &Model, h: u64| {
            m.live
                .iter()
                .find(|&&(x, _)| x == h)
                .map_or(Kind::Any, |&(_, k)| k)
        };
        match call.nr {
            nr::CHANNEL_CREATE => {
                self.add(a1.unwrap_or(0), Kind::Channel);
                self.add(a2.unwrap_or(0), Kind::Channel);
            }
            nr::SOCKET_CREATE => {
                self.add(a1.unwrap_or(0), Kind::Socket);
                self.add(a2.unwrap_or(0), Kind::Socket);
            }
            nr::EVENT_CREATE => self.add(a1.unwrap_or(0), Kind::Event),
            nr::PORT_CREATE => self.add(a1.unwrap_or(0), Kind::Port),
            nr::VMO_CREATE => self.add(a1.unwrap_or(0), Kind::Vmo),
            nr::JOB_CREATE => self.add(a1.unwrap_or(0), Kind::Job),
            nr::HANDLE_CLOSE => self.remove(call.args[0]),
            nr::HANDLE_DUPLICATE => {
                let k = kind_of(self, call.args[0]);
                self.add(a1.unwrap_or(0), k);
            }
            nr::HANDLE_REPLACE => {
                let k = kind_of(self, call.args[0]);
                self.remove(call.args[0]);
                self.add(a1.unwrap_or(0), k);
            }
            nr::CHANNEL_WRITE => {
                // the listed handles left the table
                let n = call.args[4] as usize;
                if let Some((_, raw)) = call.pokes.iter().find(|(at, _)| *at == call.args[3]) {
                    for c in raw.as_chunks::<4>().0.iter().take(n) {
                        self.remove(u64::from(u32::from_le_bytes(*c)));
                    }
                }
            }
            _ => {}
        }
    }

    /// Handles of `kind` the model believes live.
    fn of(&self, kind: Kind) -> Vec<u64> {
        self.live
            .iter()
            .filter(|&&(_, k)| k == kind || k == Kind::Any)
            .map(|&(h, _)| h)
            .collect()
    }

    /// Live handles that may be closed, replaced or sent (everything but the pinned one).
    fn movable(&self) -> Vec<u64> {
        self.live
            .iter()
            .map(|&(h, _)| h)
            .filter(|&h| Some(h) != self.pinned)
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Valid,
    Near,
    Garbage,
}

/// The generator: a seeded stream of calls.
pub struct Gen {
    state: u64,
    window: Window,
    /// The last call made, for the repeat operator (a full queue needs the same write many times).
    last: Option<Call>,
}

/// Every call the generator can make (the ones the syscall layer handles on the host).
pub const CALLS: &[usize] = &[
    nr::HANDLE_CLOSE,
    nr::HANDLE_DUPLICATE,
    nr::HANDLE_REPLACE,
    nr::CHANNEL_CREATE,
    nr::CHANNEL_WRITE,
    nr::CHANNEL_READ,
    nr::CHANNEL_READ_TXID,
    nr::SOCKET_CREATE,
    nr::SOCKET_WRITE,
    nr::SOCKET_READ,
    nr::SOCKET_SHUTDOWN,
    nr::EVENT_CREATE,
    nr::EVENT_SIGNAL,
    nr::OBJECT_INFO,
    nr::OBJECT_SIGNALS,
    nr::PORT_CREATE,
    nr::PORT_BIND,
    nr::PORT_REARM,
    nr::PORT_UNBIND,
    nr::VMO_CREATE,
    nr::VMO_READ,
    nr::VMO_WRITE,
    nr::VMO_GET_SIZE,
    nr::VMO_COMMIT,
    nr::JOB_CREATE,
    nr::JOB_SET_LIMITS,
    nr::JOB_INFO,
    nr::JOB_SET_IO_LIMITS,
];

/// The kind of handle each call wants first.
fn wants(n: usize) -> Option<Kind> {
    Some(match n {
        nr::CHANNEL_WRITE | nr::CHANNEL_READ | nr::CHANNEL_READ_TXID => Kind::Channel,
        nr::SOCKET_WRITE | nr::SOCKET_READ | nr::SOCKET_SHUTDOWN => Kind::Socket,
        nr::EVENT_SIGNAL => Kind::Event,
        nr::PORT_BIND | nr::PORT_REARM | nr::PORT_UNBIND => Kind::Port,
        nr::VMO_READ | nr::VMO_WRITE | nr::VMO_GET_SIZE | nr::VMO_COMMIT => Kind::Vmo,
        nr::JOB_CREATE | nr::JOB_SET_LIMITS | nr::JOB_INFO | nr::JOB_SET_IO_LIMITS => Kind::Job,
        _ => return None,
    })
}

impl Gen {
    pub fn new(seed: u64, window: Window) -> Gen {
        let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        Gen {
            state: if z == 0 { 1 } else { z },
            window,
            last: None,
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A number below `n` (`n` of 0 gives 0).
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }

    fn pick<T: Copy>(&mut self, v: &[T]) -> Option<T> {
        if v.is_empty() {
            None
        } else {
            let i = self.below(v.len() as u64) as usize;
            v.get(i).copied()
        }
    }

    /// A pointer inside the window with room for `len` bytes, 8-aligned, in its lower half (the upper half is for `pokes` of a second buffer).
    fn ptr(&mut self, len: u64) -> u64 {
        let span = (self.window.len / 2).saturating_sub(len).max(8);
        self.window.base + (self.below(span) & !7)
    }

    /// A second pointer, in the upper half.
    fn ptr2(&mut self, len: u64) -> u64 {
        let half = self.window.len / 2;
        let span = half.saturating_sub(len).max(8);
        self.window.base + half + (self.below(span) & !7)
    }

    /// Which kind of call comes next: 70 valid, 25 near-valid, 5 garbage (of every hundred).
    fn class(&mut self) -> Class {
        match self.below(100) {
            0..=69 => Class::Valid,
            70..=94 => Class::Near,
            _ => Class::Garbage,
        }
    }

    /// The next call, given what the process is believed to hold.
    pub fn next(&mut self, m: &Model) -> Call {
        // (one call in twelve is the last one again: a write repeated until the queue is full, a read until it is empty)
        if self.below(12) == 0 {
            if let Some(c) = &self.last {
                return c.clone();
            }
        }
        let c = match self.class() {
            Class::Valid => self.valid(m),
            Class::Near => {
                let c = self.valid(m);
                self.mutate(m, c)
            }
            Class::Garbage => self.garbage(),
        };
        self.last = Some(c.clone());
        c
    }

    /// A size for a message or a socket transfer: mostly up to `small`, some up to a kilobyte, some at the edges (0, 1, a page, the largest message),
    /// a few anywhere below it, so that full queues, partial writes and the byte ceilings are reached (T176).
    fn size(&mut self, small: u64) -> u64 {
        match self.below(20) {
            0..=13 => self.below(small + 1),
            14..=16 => self.below(1025),
            17 => [0, 1, 4096, 32767][self.below(4) as usize],
            _ => self.below(32768),
        }
    }

    /// A call that should do something: the right handle kind and sane arguments (it may still fail: a full queue, a closed peer).
    fn valid(&mut self, m: &Model) -> Call {
        let n = self.pick(CALLS).unwrap_or(nr::EVENT_CREATE);
        // set-limits on a job and a permanent shutdown are rarer than the calls that make things happen
        let n = if matches!(
            n,
            nr::SOCKET_SHUTDOWN | nr::JOB_SET_LIMITS | nr::JOB_SET_IO_LIMITS
        ) && self.below(4) != 0
        {
            nr::SOCKET_WRITE
        } else {
            n
        };
        let handle = match wants(n) {
            Some(k) => {
                let mut hs = m.of(k);
                if matches!(n, nr::JOB_SET_LIMITS | nr::JOB_SET_IO_LIMITS) {
                    hs.retain(|&h| Some(h) != m.pinned); // never the root's limits
                }
                match self.pick(&hs) {
                    Some(h) => h,
                    None => return self.creator(n, m),
                }
            }
            None => {
                let hs = if matches!(n, nr::HANDLE_CLOSE | nr::HANDLE_REPLACE) {
                    m.movable()
                } else {
                    m.live().iter().map(|&(h, _)| h).collect()
                };
                match self.pick(&hs) {
                    Some(h) => h,
                    None => return self.creator(n, m),
                }
            }
        };
        let mut c = Call {
            nr: n,
            args: [handle, 0, 0, 0, 0, 0],
            pokes: Vec::new(),
        };
        match n {
            nr::HANDLE_DUPLICATE | nr::HANDLE_REPLACE => {
                // the same rights, or (four times in ten) a random subset of the assigned ones
                c.args[1] = if self.below(10) < 4 {
                    self.next_u64() & 0x3fff
                } else {
                    1 << 31
                };
            }
            nr::VMO_CREATE => {
                let pages = match self.below(10) {
                    0 => abi::user::TOP / 4096,
                    1 => abi::user::TOP / 4096 + 1,
                    _ => 1 + self.below(8),
                };
                // charged to a job the process holds (three times in ten), else to its own
                let jobs = m.of(Kind::Job);
                let job = if self.below(10) < 3 {
                    self.pick(&jobs).unwrap_or(0)
                } else {
                    0
                };
                // (sizes are bytes; one in four makes the VMO resizable)
                let flags = u64::from(self.below(4) == 0);
                c.args = [pages * 4096, job, flags, 0, 0, 0];
            }
            nr::CHANNEL_WRITE => {
                let len = self.size(64);
                let body = self.ptr(len.max(1));
                let list = self.ptr2(16);
                let nh = self.below(3).min(m.live().len() as u64);
                let mut raw = Vec::new();
                for _ in 0..nh {
                    if let Some(h) = self.pick(&m.movable()) {
                        raw.extend_from_slice(&(h as u32).to_le_bytes());
                    }
                }
                let nh = (raw.len() / 4) as u64;
                c.args = [handle, body, len, list, nh, 0];
                // (the first four bytes are a transaction id for `channel_read_txid` to match)
                let tx = self.below(8) as u32;
                let mut bytes: Vec<u8> = (0..len).map(|i| i as u8).collect();
                for (b, t) in bytes.iter_mut().zip(tx.to_le_bytes()) {
                    *b = t;
                }
                c.pokes.push((body, bytes));
                c.pokes.push((list, raw));
            }
            nr::CHANNEL_READ => {
                let max = self.size(128);
                c.args = [
                    handle,
                    self.ptr(max.max(1)),
                    max,
                    self.ptr2(16),
                    self.below(5),
                    0,
                ]
            }
            nr::CHANNEL_READ_TXID => {
                let max = self.size(128);
                c.args = [
                    handle,
                    self.below(8),
                    self.ptr(max.max(1)),
                    max,
                    self.ptr2(16),
                    self.below(5),
                ]
            }
            nr::SOCKET_WRITE => {
                let len = self.size(128);
                let at = self.ptr(len.max(1));
                c.args = [handle, at, len, 0, 0, 0];
                c.pokes
                    .push((at, (0..len).map(|i| (i * 3) as u8).collect()));
            }
            nr::SOCKET_READ => {
                let max = self.size(128);
                c.args = [handle, self.ptr(max.max(1)), max, 0, 0, 0]
            }
            nr::EVENT_SIGNAL => {
                c.args = [
                    handle,
                    (self.below(256) as u32 as u64) << 24,
                    (self.below(256) as u32 as u64) << 24,
                    0,
                    0,
                    0,
                ]
            }
            nr::PORT_BIND => {
                let obj = self.pick(m.live()).map_or(0, |(h, _)| h);
                c.args = [
                    handle,
                    obj,
                    (self.next_u64() & 0xff00_001f) | 1, // any non-empty subset of the signals a port can watch (and a few it cannot)
                    self.below(8),
                    self.below(2),
                    0,
                ];
            }
            nr::PORT_REARM | nr::PORT_UNBIND => c.args[1] = self.below(8),
            nr::VMO_READ | nr::VMO_WRITE => {
                let len = self.below(65);
                let at = self.ptr(len.max(1));
                c.args = [handle, self.below(8192), at, len, 0, 0];
                if n == nr::VMO_WRITE {
                    c.pokes
                        .push((at, (0..len).map(|i| (i ^ 0x5a) as u8).collect()));
                }
            }
            nr::VMO_COMMIT => {
                c.args = [
                    handle,
                    self.below(4) * 4096,
                    (1 + self.below(4)) * 4096,
                    0,
                    0,
                    0,
                ]
            }
            nr::JOB_CREATE | nr::JOB_SET_LIMITS => {
                let at = self.ptr(48);
                let words: [u64; 6] = [
                    self.limit(),
                    self.limit(),
                    self.limit(),
                    self.limit(),
                    self.limit(),
                    self.limit(),
                ];
                let raw: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
                c.args = [handle, at, 0, 0, 0, 0];
                c.pokes.push((at, raw));
            }
            // (12 and 13: the I/O-mapped pages and the vectors, IOMMU6)
            nr::JOB_INFO => c.args[1] = [0, 1, 2, 3, 4, 5, 6, 7, 12, 13][self.below(10) as usize],
            nr::JOB_SET_IO_LIMITS => {
                c.args[1] = self.limit();
                c.args[2] = self.limit();
            }
            _ => {}
        }
        c
    }

    /// A limit word: none, zero, or small.
    fn limit(&mut self) -> u64 {
        match self.below(4) {
            0 => u64::MAX,
            1 => 0,
            _ => 1 + self.below(64),
        }
    }

    /// A call that makes something, for when the model holds no handle of the kind a call wants.
    fn creator(&mut self, _wanted: usize, m: &Model) -> Call {
        const MAKERS: [usize; 5] = [
            nr::CHANNEL_CREATE,
            nr::SOCKET_CREATE,
            nr::EVENT_CREATE,
            nr::PORT_CREATE,
            nr::VMO_CREATE,
        ];
        let n = self.pick(&MAKERS).unwrap_or(nr::EVENT_CREATE);
        let _ = m;
        let mut args = [0u64; 6];
        if n == nr::VMO_CREATE {
            args[0] = (1 + self.below(8)) * 4096;
        }
        Call {
            nr: n,
            args,
            pokes: Vec::new(),
        }
    }

    /// Break a valid call a little.
    fn mutate(&mut self, m: &Model, mut c: Call) -> Call {
        match self.below(6) {
            // a handle of another kind (or any live one)
            0 => {
                if let Some((h, _)) = self.pick(m.live()) {
                    c.args[0] = h;
                }
            }
            // a closed handle
            1 => {
                if let Some(h) = self.pick(&m.closed) {
                    c.args[0] = h;
                } else {
                    c.args[0] ^= 1 << 20; // a generation that never existed
                }
            }
            // a size or length off by one, or just past a limit
            2 => {
                let i = 1 + self.below(4) as usize;
                if let Some(v) = c.args.get_mut(i) {
                    *v = match self.below(3) {
                        0 => v.wrapping_add(1),
                        1 => v.wrapping_sub(1),
                        _ => v.wrapping_add(4096),
                    };
                }
            }
            // a pointer out of the window, or in the read-only page
            3 => {
                let i = 1 + self.below(4) as usize;
                if let Some(v) = c.args.get_mut(i) {
                    *v = if self.below(2) == 0 {
                        self.window.read_only
                    } else {
                        self.below(0x1000)
                    };
                }
            }
            // a wide argument (more than 32 bits where a handle or a mask goes)
            4 => {
                let i = self.below(3) as usize;
                if let Some(v) = c.args.get_mut(i) {
                    *v |= 1 << 32;
                }
            }
            // rights widened or unassigned bits
            _ => {
                if let Some(v) = c.args.get_mut(1) {
                    *v = self.next_u64() as u32 as u64;
                }
            }
        }
        c
    }

    /// Any known call with random or extreme arguments.
    fn garbage(&mut self) -> Call {
        let n = self.pick(CALLS).unwrap_or(nr::EVENT_CREATE);
        let mut args = [0u64; 6];
        for a in &mut args {
            *a = match self.below(4) {
                0 => self.next_u64(),
                1 => u64::MAX - self.below(4),
                2 => self.below(16),
                _ => self.window.base + self.below(self.window.len),
            };
        }
        Call {
            nr: n,
            args,
            pokes: Vec::new(),
        }
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    const W: Window = Window {
        base: 0x1_0000,
        len: 0x10000,
        read_only: 0x20_0000,
    };

    fn run(seed: u64, n: usize) -> Vec<Call> {
        let mut g = Gen::new(seed, W);
        let m = Model::with(&[
            (7, Kind::Channel),
            (8, Kind::Event),
            (9, Kind::Vmo),
            (10, Kind::Job),
        ]);
        (0..n).map(|_| g.next(&m)).collect()
    }

    #[test]
    fn the_same_seed_gives_the_same_sequence() {
        assert_eq!(run(3, 500), run(3, 500));
        assert_ne!(run(3, 500), run(4, 500));
    }

    #[test]
    fn every_known_call_is_generated() {
        let calls = run(1, 20_000);
        for n in CALLS {
            assert!(calls.iter().any(|c| c.nr == *n), "call {n} never generated");
        }
    }

    #[test]
    fn valid_calls_use_pointers_inside_the_window_and_most_calls_are_valid() {
        // (the model holds one handle of some kinds only, so creators stand in for the rest)
        let mut g = Gen::new(9, W);
        let m = Model::with(&[
            (7, Kind::Channel),
            (8, Kind::Event),
            (9, Kind::Vmo),
            (10, Kind::Job),
            (11, Kind::Socket),
            (12, Kind::Port),
        ]);
        let mut inside = 0;
        let total = 5000;
        for _ in 0..total {
            let c = g.valid(&m);
            for (at, bytes) in &c.pokes {
                assert!(
                    *at >= W.base && at + bytes.len() as u64 <= W.base + W.len,
                    "a poke outside the window: {c:?}"
                );
            }
            if c.pokes
                .iter()
                .all(|(at, b)| *at >= W.base && at + b.len() as u64 <= W.base + W.len)
            {
                inside += 1;
            }
        }
        assert_eq!(inside, total);
    }

    #[test]
    fn the_model_follows_creates_closes_and_sends() {
        let mut m = Model::default();
        let create = Call {
            nr: nr::CHANNEL_CREATE,
            args: [0; 6],
            pokes: Vec::new(),
        };
        m.observe(&create, true, Some(1), Some(2));
        assert_eq!(m.live().len(), 2);
        let close = Call {
            nr: nr::HANDLE_CLOSE,
            args: [1, 0, 0, 0, 0, 0],
            pokes: Vec::new(),
        };
        m.observe(&close, true, None, None);
        assert_eq!(m.live().len(), 1);
        m.observe(&close, false, None, None);
        assert_eq!(m.live().len(), 1, "a failed call changes nothing");
        let send = Call {
            nr: nr::CHANNEL_WRITE,
            args: [2, 0x1_0000, 0, 0x1_8000, 1, 0],
            pokes: std::vec![(0x1_8000, 2u32.to_le_bytes().to_vec())],
        };
        m.observe(&send, true, None, None);
        assert!(m.live().is_empty(), "the sent handle left the table");
    }

    #[test]
    fn the_mix_is_seventy_twenty_five_five() {
        let mut g = Gen::new(11, W);
        let (mut v, mut n, mut x) = (0, 0, 0);
        for _ in 0..100_000 {
            match g.class() {
                Class::Valid => v += 1,
                Class::Near => n += 1,
                Class::Garbage => x += 1,
            }
        }
        assert!(
            (69_000..=71_000).contains(&v)
                && (24_000..=26_000).contains(&n)
                && (4_300..=5_700).contains(&x),
            "{v} {n} {x}"
        );
    }
}
