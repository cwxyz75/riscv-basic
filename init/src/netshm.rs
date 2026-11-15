//! Shared memory for the network data path (NET-SPEC 5.3): `netring::Mem` over a mapped region, with the ordering the
//! ring protocol needs. Included (`#[path]`) by `netdrv`, `netd` and the suite, like `prog.rs`.
#![allow(dead_code)]

use core::sync::atomic::{fence, AtomicU32, Ordering};

/// The ring VMO as mapped in this process at `base`.
#[derive(Clone, Copy)]
pub struct Shm {
    pub base: usize,
}

impl Shm {
    fn word(&self, off: usize) -> &AtomicU32 {
        // SAFETY: `base` maps the two-page ring VMO read-write for the life of the process; `off` is a multiple of 4
        // inside it (the ring code only passes layout offsets), so this is an aligned, valid `AtomicU32`.
        unsafe { &*((self.base + off) as *const AtomicU32) }
    }

    /// Read a 64-bit counter (two words, low first) from the control header.
    pub fn load64(&self, off: usize) -> u64 {
        u64::from(self.word(off).load(Ordering::Acquire))
            | u64::from(self.word(off + 4).load(Ordering::Acquire)) << 32
    }

    /// Add to a 64-bit counter written by one process only (no atomic add across two words is needed).
    pub fn add64(&self, off: usize, n: u64) {
        let v = self.load64(off).wrapping_add(n);
        self.word(off).store(v as u32, Ordering::Relaxed);
        self.word(off + 4)
            .store((v >> 32) as u32, Ordering::Release);
    }

    pub fn store64(&self, off: usize, v: u64) {
        self.word(off).store(v as u32, Ordering::Relaxed);
        self.word(off + 4)
            .store((v >> 32) as u32, Ordering::Release);
    }

    pub fn read_bytes(&self, off: usize, out: &mut [u8]) {
        for (i, b) in out.iter_mut().enumerate() {
            // SAFETY: inside the mapped ring VMO (callers pass header offsets).
            *b = unsafe { core::ptr::read_volatile((self.base + off + i) as *const u8) };
        }
    }

    pub fn write_bytes(&self, off: usize, data: &[u8]) {
        for (i, b) in data.iter().enumerate() {
            // SAFETY: as in `read_bytes`.
            unsafe { core::ptr::write_volatile((self.base + off + i) as *mut u8, *b) };
        }
    }

    pub fn count(&self, stat: usize) {
        self.add64(netring::ctl::STATS + 8 * stat, 1);
    }
}

impl netring::Mem for Shm {
    fn load(&self, off: usize) -> u32 {
        self.word(off).load(Ordering::Acquire)
    }
    fn store(&self, off: usize, v: u32) {
        self.word(off).store(v, Ordering::Release)
    }
    fn fence(&self) {
        fence(Ordering::SeqCst)
    }
}
