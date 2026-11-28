//! The mount table (FS-SPEC section 6). A volume is **identified by its uuid**; this table maps a uuid to the local name it is
//! published under (`/svc/vol/<name>`); a disk can never claim a name by what it calls itself. The table says nothing about
//! who may use a volume (that is the grant list, `/etc/trusted` today).
//!
//! Format: one entry per line, `uuid name [options...]`; blank lines and everything after `#` are ignored; fields are split on
//! spaces and tabs. The uuid is 32 hex digits, dashes allowed anywhere between them. A name is 1 to 32 bytes of `a-z A-Z 0-9 . _ -`
//! not starting with `.`. Options: `ro` (publish read-only), `sub=<dir>` (publish this subdirectory of the volume instead of its
//! root; the path is relative, no `..`).
//!
//! [`plan`] is the other half: given the volumes `volmgr` found (a uuid and a label each) and the table, what is published and
//! under which names, with duplicates refused.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub uuid: [u8; 16],
    pub name: String,
    pub readonly: bool,
    pub sub: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Fewer than two fields.
    MissingField,
    /// The uuid is not 32 hex digits.
    BadUuid,
    /// The name is empty, too long, or has a character outside the set.
    BadName,
    /// An option that is not defined, or a bad `sub=` path.
    BadOption,
    /// The same uuid or the same name twice.
    Duplicate,
    /// The text is not UTF-8.
    NotText,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based line number (0 when the whole text is bad).
    pub line: usize,
    pub kind: ErrorKind,
}

pub const MAX_NAME: usize = 32;
pub const MAX_ENTRIES: usize = 64;

/// Parse a uuid: 32 hex digits, dashes ignored.
pub fn parse_uuid(s: &str) -> Option<[u8; 16]> {
    let mut out = [0u8; 16];
    let mut n = 0;
    let mut hi: Option<u8> = None;
    for c in s.chars() {
        if c == '-' {
            continue;
        }
        let d = c.to_digit(16)? as u8;
        match hi.take() {
            None => hi = Some(d),
            Some(h) => {
                if n >= 16 {
                    return None;
                }
                out[n] = (h << 4) | d;
                n += 1;
            }
        }
    }
    (n == 16 && hi.is_none()).then_some(out)
}

/// The uuid as 32 lower-case hex digits (the name under `/svc/vol/by-uuid/`).
pub fn uuid_hex(u: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in u {
        for d in [b >> 4, b & 15] {
            s.push(char::from_digit(u32::from(d), 16).unwrap_or('0'));
        }
    }
    s
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= MAX_NAME
        && !n.starts_with('.')
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn valid_sub(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 255
        && !p.starts_with('/')
        && p.split('/').all(|c| {
            !c.is_empty() && c != "." && c != ".." && !c.bytes().any(|b| b == 0 || b < 0x20)
        })
}

/// Parse the table.
pub fn parse(text: &[u8]) -> Result<Vec<Entry>, ParseError> {
    let text = core::str::from_utf8(text).map_err(|_| ParseError {
        line: 0,
        kind: ErrorKind::NotText,
    })?;
    let mut out: Vec<Entry> = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let err = |kind| ParseError { line: i + 1, kind };
        let line = raw.split('#').next().unwrap_or("");
        let mut f = line.split([' ', '\t']).filter(|s| !s.is_empty());
        let Some(u) = f.next() else { continue };
        let name = f.next().ok_or(err(ErrorKind::MissingField))?;
        let uuid = parse_uuid(u).ok_or(err(ErrorKind::BadUuid))?;
        if !valid_name(name) {
            return Err(err(ErrorKind::BadName));
        }
        let (mut readonly, mut sub) = (false, None);
        for o in f {
            if o == "ro" {
                readonly = true;
            } else if let Some(p) = o.strip_prefix("sub=") {
                if !valid_sub(p) || sub.is_some() {
                    return Err(err(ErrorKind::BadOption));
                }
                sub = Some(String::from(p));
            } else {
                return Err(err(ErrorKind::BadOption));
            }
        }
        if out.iter().any(|e| e.uuid == uuid || e.name == name) || out.len() >= MAX_ENTRIES {
            return Err(err(ErrorKind::Duplicate));
        }
        out.push(Entry {
            uuid,
            name: String::from(name),
            readonly,
            sub,
        });
    }
    Ok(out)
}

/// A volume `volmgr` recognised on some device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The device number (in `devmgr`'s order).
    pub device: u16,
    pub uuid: [u8; 16],
    /// Display text from the superblock; never used for naming.
    pub label: String,
}

/// What to do with one found volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Serve it: always under `by-uuid/<hex>`, and under `name` too if the table maps its uuid.
    Publish {
        by_uuid: String,
        name: Option<String>,
        readonly: bool,
        sub: Option<String>,
    },
    /// A second device with the same uuid as an earlier one (a cloned disk): not served, the first keeps its names.
    RefuseDuplicate,
}

/// Decide for every found volume, in order. A uuid seen before is refused; names come only from the table.
pub fn plan(table: &[Entry], found: &[Found]) -> Vec<(Found, Decision)> {
    let mut seen: Vec<[u8; 16]> = Vec::new();
    let mut out = Vec::new();
    for f in found {
        if seen.contains(&f.uuid) {
            out.push((f.clone(), Decision::RefuseDuplicate));
            continue;
        }
        seen.push(f.uuid);
        let e = table.iter().find(|e| e.uuid == f.uuid);
        out.push((
            f.clone(),
            Decision::Publish {
                by_uuid: uuid_hex(&f.uuid),
                name: e.map(|e| e.name.clone()),
                readonly: e.is_some_and(|e| e.readonly),
                sub: e.and_then(|e| e.sub.clone()),
            },
        ));
    }
    out
}

/// How `volmgr` opens and probes one device (FS-SPEC section 6, step 5d; T286, T209): a pure state machine over the device
/// manager's answers, the probe's verdicts and the clock, so that its budgets are host-tested and proved, and the callers that wait
/// for `volmgr` (a test, a supervisor) can add them up.
///
/// - The device manager answers `Busy` while the device's previous driver winds down: open again, for up to [`discovery::BUSY_MS`]
///   from the first open.
/// - A probe that could not read the device ([`discovery::Probed::Unreadable`]) after a `Busy` is the old driver's last moments,
///   not a verdict on the disk: open and probe again, for up to [`discovery::PROBE_RETRY_MS`] from that first failure (a `Busy`
///   meanwhile is waited for inside the same window). An unreadable device that was never `Busy` is a failing device and is
///   ignored, as is one still unreadable when the window closes. A readable device with no volume
///   ([`discovery::Probed::NotVolume`]) is never probed again: a hostile device costs one probe.
/// - Nothing is asked of the device manager later than [`discovery::DEVICE_BOUND_MS`] after the first open: one device costs at
///   most that, plus the open or probe under way when it passes.
pub mod discovery {
    /// How long a device that answers `Busy` is waited for, from its first open (ms).
    pub const BUSY_MS: u64 = 5000;
    /// How long an unreadable device that was `Busy` before is probed again, from its first failed probe (ms).
    pub const PROBE_RETRY_MS: u64 = 5000;
    /// The latest a device's discovery opens it again, after its first open (ms).
    pub const DEVICE_BOUND_MS: u64 = BUSY_MS + PROBE_RETRY_MS;
    /// How long a volume server may take to mount its volume and say it is ready (ms): `volmgr` publishes its names only then.
    pub const READY_MS: u64 = 15_000;

    /// The device manager's answer to an open.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Opened {
        /// A channel to a new driver.
        Ok,
        /// The previous driver is still winding down.
        Busy,
        /// No device with this number: the end of the list.
        NoDevice,
        /// Anything else (refused, failed, no answer).
        Failed,
    }

    /// What a probe found (`fsfmt::Probe`, without the superblock).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Probed {
        Volume,
        NotVolume,
        Unreadable,
    }

    /// What to do next.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Next {
        /// Open the device again (after a short pause).
        OpenAgain,
        /// Probe the channel the open gave.
        Probe,
        /// A volume: serve it.
        Use,
        /// Not served: not a volume, failing, or still busy or unreadable when its bound passed.
        Ignore,
        /// There is no such device: discovery is over.
        End,
    }

    /// One device's discovery.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Device {
        started: u64,
        busy: bool,
        first_failure: Option<u64>,
    }

    impl Device {
        /// Discovery of a device starts with its first open, at `now` (ms).
        pub fn new(now: u64) -> Device {
            Device {
                started: now,
                busy: false,
                first_failure: None,
            }
        }

        /// The latest time anything is asked again.
        fn cap(&self) -> u64 {
            self.started.saturating_add(DEVICE_BOUND_MS)
        }

        /// Until when a `Busy` answer is waited for.
        fn busy_until(&self) -> u64 {
            let first = self.started.saturating_add(BUSY_MS);
            match self.first_failure {
                Some(f) => first.max(f.saturating_add(PROBE_RETRY_MS)).min(self.cap()),
                None => first,
            }
        }

        /// Whether the device answered `Busy` at some point of this discovery.
        pub fn was_busy(&self) -> bool {
            self.busy
        }

        /// The device manager answered an open with `r` at `now`.
        pub fn opened(&mut self, r: Opened, now: u64) -> Next {
            match r {
                Opened::Ok => Next::Probe,
                Opened::NoDevice => Next::End,
                Opened::Failed => Next::Ignore,
                Opened::Busy => {
                    self.busy = true;
                    if now < self.busy_until() {
                        Next::OpenAgain
                    } else {
                        Next::Ignore
                    }
                }
            }
        }

        /// The probe of the channel found `p` at `now`.
        pub fn probed(&mut self, p: Probed, now: u64) -> Next {
            match p {
                Probed::Volume => Next::Use,
                Probed::NotVolume => Next::Ignore,
                Probed::Unreadable => {
                    if !self.busy {
                        return Next::Ignore;
                    }
                    let first = *self.first_failure.get_or_insert(now);
                    if now < first.saturating_add(PROBE_RETRY_MS).min(self.cap()) {
                        Next::OpenAgain
                    } else {
                        Next::Ignore
                    }
                }
            }
        }
    }

    #[cfg(kani)]
    mod proofs {
        use super::*;

        fn any_opened() -> Opened {
            match kani::any::<u8>() % 4 {
                0 => Opened::Ok,
                1 => Opened::Busy,
                2 => Opened::NoDevice,
                _ => Opened::Failed,
            }
        }

        fn any_probed() -> Probed {
            match kani::any::<u8>() % 3 {
                0 => Probed::Volume,
                1 => Probed::NotVolume,
                _ => Probed::Unreadable,
            }
        }

        /// Every run of up to eight answers, at any non-decreasing times from any start: a volume is always used, a readable
        /// non-volume never probed again, an unreadable device never retried unless it was Busy before, a failed or missing device
        /// never opened again, and nothing is asked of the device manager at or past `DEVICE_BOUND_MS` after the first open
        /// (`cargo kani -p mounts`, by hand: OD177, OD182).
        #[kani::proof]
        #[kani::unwind(9)]
        fn the_decisions_keep_their_bounds() {
            let start: u64 = kani::any();
            kani::assume(start <= u64::MAX / 2);
            let mut d = Device::new(start);
            let mut now = start;
            let mut busy_seen = false;
            let mut probing = false;
            for _ in 0..8 {
                let step: u64 = kani::any();
                kani::assume(step <= u64::MAX / 32);
                now += step;
                let next = if probing {
                    let p = any_probed();
                    let n = d.probed(p, now);
                    match p {
                        Probed::Volume => assert!(n == Next::Use),
                        Probed::NotVolume => assert!(n == Next::Ignore),
                        Probed::Unreadable => {
                            assert!(n == Next::OpenAgain || n == Next::Ignore);
                            if !busy_seen {
                                assert!(n == Next::Ignore);
                            }
                        }
                    }
                    n
                } else {
                    let o = any_opened();
                    busy_seen |= o == Opened::Busy;
                    let n = d.opened(o, now);
                    match o {
                        Opened::Ok => assert!(n == Next::Probe),
                        Opened::NoDevice => assert!(n == Next::End),
                        Opened::Failed => assert!(n == Next::Ignore),
                        Opened::Busy => {
                            assert!(n == Next::OpenAgain || n == Next::Ignore);
                            // the first wait for a Busy device is never cut short of BUSY_MS
                            if now < start + BUSY_MS {
                                assert!(n == Next::OpenAgain);
                            }
                        }
                    }
                    n
                };
                if next == Next::OpenAgain {
                    assert!(now < start + DEVICE_BOUND_MS);
                }
                match next {
                    Next::OpenAgain => probing = false,
                    Next::Probe => probing = true,
                    Next::Use | Next::Ignore | Next::End => return,
                }
            }
        }
    }
}
