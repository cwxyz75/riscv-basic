//! The configuration blob (S8, NET-SPEC 8.4): one page held by `netsup`, two halves, each written by one process with a
//! sequence counter (odd while writing, even when stable; a reader retries until it reads the same even value twice). It lets
//! a restarted `netd` re-apply the interface configuration at once and a restarted `dhcpc` resume (renew) instead of starting
//! over. Included (`#[path]`) by `netd` and `dhcpc`.
#![allow(dead_code)]

use core::sync::atomic::{AtomicU32, Ordering};

/// The interface half (written by `netd`): `seq, valid, addr, mask, gateway, dns` (words 0 to 5).
const IFACE: usize = 0;
/// The lease half (written by `dhcpc`): `seq, valid, server, addr, mask, router, dns, lease_s, t1_s, t2_s, acquired_lo, acquired_hi`.
const LEASE: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Iface {
    pub addr: [u8; 4],
    pub mask: [u8; 4],
    pub gateway: [u8; 4],
    pub dns: [u8; 4],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Lease {
    pub server: [u8; 4],
    pub addr: [u8; 4],
    pub mask: [u8; 4],
    pub router: [u8; 4],
    pub dns: [u8; 4],
    pub lease_s: u32,
    pub t1_s: u32,
    pub t2_s: u32,
    /// When it was acquired, in the kernel's monotonic nanoseconds (valid across restarts within one boot).
    pub acquired_ns: u64,
}

#[derive(Clone, Copy)]
pub struct Blob {
    pub base: usize,
}

fn w(v: [u8; 4]) -> u32 {
    u32::from_le_bytes(v)
}
fn b(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}

impl Blob {
    fn word(&self, off: usize, i: usize) -> &AtomicU32 {
        // SAFETY: `base` maps the one-page blob read-write for the life of the process; `off + 4 * i` is inside the page and
        // 4-aligned (the offsets here are constants below 512).
        unsafe { &*((self.base + off + 4 * i) as *const AtomicU32) }
    }

    /// Run `f` as the single writer of the half at `off`: seq odd during, even after.
    fn write(&self, off: usize, f: impl FnOnce(&dyn Fn(usize, u32))) {
        let seq = self.word(off, 0).load(Ordering::Relaxed);
        self.word(off, 0).store(seq.wrapping_add(1) | 1, Ordering::Release);
        f(&|i, v| self.word(off, i).store(v, Ordering::Relaxed));
        self.word(off, 0).store((seq.wrapping_add(1) | 1).wrapping_add(1), Ordering::Release);
    }

    /// Read the half at `off` as words, retrying until the sequence number is even and unchanged.
    fn read<const N: usize>(&self, off: usize) -> Option<[u32; N]> {
        for _ in 0..1000 {
            let s1 = self.word(off, 0).load(Ordering::Acquire);
            if s1 & 1 != 0 {
                continue;
            }
            let mut out = [0u32; N];
            for (i, o) in out.iter_mut().enumerate() {
                *o = self.word(off, i).load(Ordering::Relaxed);
            }
            core::sync::atomic::fence(Ordering::Acquire);
            if self.word(off, 0).load(Ordering::Relaxed) == s1 {
                return Some(out);
            }
        }
        None
    }

    pub fn write_iface(&self, i: Option<Iface>) {
        self.write(IFACE, |set| match i {
            Some(i) => {
                set(2, w(i.addr));
                set(3, w(i.mask));
                set(4, w(i.gateway));
                set(5, w(i.dns));
                set(1, 1);
            }
            None => set(1, 0),
        });
    }

    pub fn read_iface(&self) -> Option<Iface> {
        let r = self.read::<6>(IFACE)?;
        (r[1] == 1).then(|| Iface { addr: b(r[2]), mask: b(r[3]), gateway: b(r[4]), dns: b(r[5]) })
    }

    pub fn write_lease(&self, l: Option<Lease>) {
        self.write(LEASE, |set| match l {
            Some(l) => {
                for (i, v) in [w(l.server), w(l.addr), w(l.mask), w(l.router), w(l.dns), l.lease_s, l.t1_s, l.t2_s, l.acquired_ns as u32, (l.acquired_ns >> 32) as u32].into_iter().enumerate() {
                    set(2 + i, v);
                }
                set(1, 1);
            }
            None => set(1, 0),
        });
    }

    pub fn read_lease(&self) -> Option<Lease> {
        let r = self.read::<12>(LEASE)?;
        (r[1] == 1).then(|| Lease {
            server: b(r[2]),
            addr: b(r[3]),
            mask: b(r[4]),
            router: b(r[5]),
            dns: b(r[6]),
            lease_s: r[7],
            t1_s: r[8],
            t2_s: r[9],
            acquired_ns: u64::from(r[10]) | u64::from(r[11]) << 32,
        })
    }
}
