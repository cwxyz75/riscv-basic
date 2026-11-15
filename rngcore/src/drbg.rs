//! A random generator with fast key erasure. The state is one 32-byte key. Every request runs ChaCha20 under that key: the first
//! 32 bytes of the first block become the *next* key (so the key that produced any output is already gone, and a later
//! compromise of the state does not reveal earlier output), the rest is the output. Reseeding mixes new entropy into the key
//! with HMAC-SHA256, so no input can make the state weaker than it was.
use crate::chacha;
use crate::sha256::{hmac_sha256, sha256};

/// The most one request may ask for (the key is replaced after each).
pub const MAX_REQUEST: usize = 64 * 1024;

pub struct Drbg {
    key: [u8; 32],
    /// Total entropy bytes mixed in (a diagnostic; `is_seeded` needs at least 32).
    mixed: u64,
}

impl Drbg {
    /// An unseeded generator: it refuses to produce output until at least 32 bytes of entropy were mixed in.
    pub fn new() -> Drbg {
        Drbg {
            key: [0; 32],
            mixed: 0,
        }
    }

    pub fn is_seeded(&self) -> bool {
        self.mixed >= 32
    }

    /// Mix `entropy` into the state: `key = HMAC(key, entropy)`. Any amount, any quality: it can only add.
    pub fn reseed(&mut self, entropy: &[u8]) {
        self.key = hmac_sha256(&self.key, &[b"rngcore reseed", entropy]);
        self.mixed = self.mixed.saturating_add(entropy.len() as u64);
    }

    /// Fill `out` (at most `MAX_REQUEST` bytes; longer is refused). `false` if not yet seeded: nothing is written.
    pub fn fill(&mut self, out: &mut [u8]) -> bool {
        if !self.is_seeded() || out.len() > MAX_REQUEST {
            return false;
        }
        let nonce = [0u8; 12];
        let mut old = self.key;
        let first = chacha::block(&old, 0, &nonce);
        // the output: the second half of block 0, then blocks 1, 2, ... under the same (old) key
        let mut off = out.len().min(32);
        out[..off].copy_from_slice(&first[32..32 + off]);
        let mut ctr = 1u32;
        while off < out.len() {
            let b = chacha::block(&old, ctr, &nonce);
            let n = (out.len() - off).min(64);
            out[off..off + n].copy_from_slice(&b[..n]);
            off += n;
            ctr += 1;
        }
        // erase: the next key is the first half of block 0; the key that made this output is overwritten
        self.key.copy_from_slice(&first[..32]);
        old.fill(0);
        true
    }

    /// A derived 32-byte key for `label` (domain separation): HMAC of the label under a fresh generator output.
    pub fn derive(&mut self, label: &[u8]) -> Option<[u8; 32]> {
        let mut base = [0u8; 32];
        if !self.fill(&mut base) {
            return None;
        }
        Some(hmac_sha256(&base, &[label]))
    }
}

impl Default for Drbg {
    fn default() -> Drbg {
        Drbg::new()
    }
}

/// A digest of the state, for tests that need to see that two generators diverge without exposing the key.
pub fn fingerprint(d: &Drbg) -> [u8; 32] {
    sha256(&d.key)
}
