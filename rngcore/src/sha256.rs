//! SHA-256, HMAC-SHA256 and HKDF-SHA256.

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

pub const DIGEST: usize = 32;
const BLOCK: usize = 64;

/// An incremental SHA-256.
#[derive(Clone)]
pub struct Sha256 {
    h: [u32; 8],
    buf: [u8; BLOCK],
    used: usize,
    total: u64,
}

impl Default for Sha256 {
    fn default() -> Sha256 {
        Sha256::new()
    }
}

fn compress(h: &mut [u32; 8], block: &[u8; BLOCK]) {
    let mut w = [0u32; 64];
    for (i, c) in block.as_chunks::<4>().0.iter().enumerate() {
        w[i] = u32::from_be_bytes(*c);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = hh
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
        *x = x.wrapping_add(y);
    }
}

impl Sha256 {
    pub fn new() -> Sha256 {
        Sha256 {
            h: H0,
            buf: [0; BLOCK],
            used: 0,
            total: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.used > 0 {
            let n = (BLOCK - self.used).min(data.len());
            self.buf[self.used..self.used + n].copy_from_slice(&data[..n]);
            self.used += n;
            data = &data[n..];
            if self.used == BLOCK {
                let b = self.buf;
                compress(&mut self.h, &b);
                self.used = 0;
            }
        }
        let (blocks, rest) = data.as_chunks::<BLOCK>();
        for b in blocks {
            compress(&mut self.h, b);
        }
        if !rest.is_empty() {
            self.buf[..rest.len()].copy_from_slice(rest);
            self.used = rest.len();
        }
    }

    pub fn finish(mut self) -> [u8; DIGEST] {
        let bits = self.total.wrapping_mul(8);
        let mut pad = [0u8; BLOCK * 2];
        pad[0] = 0x80;
        let padlen = if self.used < 56 {
            56 - self.used
        } else {
            120 - self.used
        };
        self.update(&pad[..padlen]);
        self.update(&bits.to_be_bytes());
        debug_assert_eq!(self.used, 0);
        let mut out = [0u8; DIGEST];
        for (o, x) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.h) {
            *o = x.to_be_bytes();
        }
        out
    }
}

pub fn sha256(data: &[u8]) -> [u8; DIGEST] {
    let mut s = Sha256::new();
    s.update(data);
    s.finish()
}

/// HMAC-SHA256 (RFC 2104) over the concatenation of `parts`.
pub fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; DIGEST] {
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..DIGEST].copy_from_slice(&sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(&ipad);
    for p in parts {
        inner.update(p);
    }
    let inner = inner.finish();
    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(&inner);
    outer.finish()
}

/// HKDF-SHA256 (RFC 5869): extract with `salt`, expand with `info`, fill `out`. `false` (and nothing written) if `out` is
/// longer than the 255 * 32 bytes the construction allows.
pub fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8], out: &mut [u8]) -> bool {
    if out.len() > 255 * DIGEST {
        return false;
    }
    let prk = hmac_sha256(salt, &[ikm]);
    let mut t = [0u8; DIGEST];
    let mut have = 0usize;
    for (i, chunk) in out.chunks_mut(DIGEST).take(255).enumerate() {
        let ctr = [(i + 1) as u8];
        t = if have == 0 {
            hmac_sha256(&prk, &[info, &ctr])
        } else {
            hmac_sha256(&prk, &[&t, info, &ctr])
        };
        have = DIGEST;
        chunk.copy_from_slice(&t[..chunk.len()]);
    }
    true
}
