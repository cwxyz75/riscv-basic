//! S, the send buffer (S9-SPEC 5.4): a ring holding everything taken from the application and not yet acknowledged.
use alloc::vec;
use alloc::vec::Vec;

pub struct SendBuf {
    buf: Vec<u8>,
    head: usize,
    len: usize,
}

impl SendBuf {
    pub fn new(cap: usize) -> SendBuf {
        SendBuf {
            buf: vec![0u8; cap],
            head: 0,
            len: 0,
        }
    }
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn free(&self) -> usize {
        self.buf.len() - self.len
    }
    /// Append as much of `data` as fits; returns how much.
    pub fn push(&mut self, data: &[u8]) -> usize {
        let n = data.len().min(self.free());
        for (i, &b) in data[..n].iter().enumerate() {
            let p = (self.head + self.len + i) % self.buf.len();
            self.buf[p] = b;
        }
        self.len += n;
        n
    }
    /// Copy `out.len()` bytes starting `off` bytes from the oldest unacknowledged one; returns how many were available.
    pub fn peek(&self, off: usize, out: &mut [u8]) -> usize {
        if off >= self.len {
            return 0;
        }
        let n = out.len().min(self.len - off);
        for (i, o) in out[..n].iter_mut().enumerate() {
            *o = self.buf[(self.head + off + i) % self.buf.len()];
        }
        n
    }
    /// Drop `n` acknowledged bytes from the front.
    pub fn ack(&mut self, n: usize) {
        let n = n.min(self.len);
        self.head = (self.head + n) % self.buf.len().max(1);
        self.len -= n;
    }
}
