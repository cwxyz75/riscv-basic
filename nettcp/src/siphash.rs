//! SipHash-2-4 (Aumasson and Bernstein), the keyed hash behind initial sequence numbers (RFC 6528) and ephemeral ports
//! (RFC 6056) (S9-SPEC 5.10). Written here from the paper; checked against its published vectors in `tests/isn.rs`.

fn round(v: &mut [u64; 4]) {
    v[0] = v[0].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(13);
    v[1] ^= v[0];
    v[0] = v[0].rotate_left(32);
    v[2] = v[2].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(16);
    v[3] ^= v[2];
    v[0] = v[0].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(21);
    v[3] ^= v[0];
    v[2] = v[2].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(17);
    v[1] ^= v[2];
    v[2] = v[2].rotate_left(32);
}

/// SipHash-2-4 of `data` under the 16-byte `key`.
pub fn siphash24(key: &[u8; 16], data: &[u8]) -> u64 {
    let k0 = u64::from_le_bytes([
        key[0], key[1], key[2], key[3], key[4], key[5], key[6], key[7],
    ]);
    let k1 = u64::from_le_bytes([
        key[8], key[9], key[10], key[11], key[12], key[13], key[14], key[15],
    ]);
    let mut v = [
        k0 ^ 0x736f_6d65_7073_6575,
        k1 ^ 0x646f_7261_6e64_6f6d,
        k0 ^ 0x6c79_6765_6e65_7261,
        k1 ^ 0x7465_6462_7974_6573,
    ];
    let (blocks, rest) = data.as_chunks::<8>();
    for c in blocks {
        let m = u64::from_le_bytes(*c);
        v[3] ^= m;
        round(&mut v);
        round(&mut v);
        v[0] ^= m;
    }
    let mut last = (data.len() as u64) << 56;
    for (i, &b) in rest.iter().enumerate() {
        last |= u64::from(b) << (8 * i);
    }
    v[3] ^= last;
    round(&mut v);
    round(&mut v);
    v[0] ^= last;
    v[2] ^= 0xff;
    for _ in 0..4 {
        round(&mut v);
    }
    v[0] ^ v[1] ^ v[2] ^ v[3]
}

/// The four-tuple as bytes, hashed under the secret: the `F` of RFC 6528.
pub fn tuple_hash(key: &[u8; 16], local: [u8; 4], lport: u16, remote: [u8; 4], rport: u16) -> u64 {
    let mut b = [0u8; 12];
    b[0..4].copy_from_slice(&local);
    b[4..6].copy_from_slice(&lport.to_be_bytes());
    b[6..10].copy_from_slice(&remote);
    b[10..12].copy_from_slice(&rport.to_be_bytes());
    siphash24(key, &b)
}

/// The initial sequence number for a connection (RFC 6528): the clock in 4-microsecond units plus the keyed hash of the
/// four-tuple. The clock is the kernel's, the secret outlives a stack restart, so for one four-tuple the ISN only moves
/// forward within a boot.
pub fn isn(
    key: &[u8; 16],
    local: [u8; 4],
    lport: u16,
    remote: [u8; 4],
    rport: u16,
    now_ns: u64,
) -> u32 {
    let m = (now_ns / 4_000) as u32;
    m.wrapping_add(tuple_hash(key, local, lport, remote, rport) as u32)
}

/// Pick a local port for an outgoing connection (RFC 6056's hash-based algorithm, simplified: the starting offset depends
/// on the destination and the secret, and the first free port at or after it, within `lo..=hi`, is taken). `used` says
/// whether a port is taken for this destination. `None` when the whole range is taken.
#[allow(clippy::too_many_arguments)]
pub fn ephemeral_port(
    key: &[u8; 16],
    local: [u8; 4],
    remote: [u8; 4],
    rport: u16,
    lo: u16,
    hi: u16,
    counter: u32,
    used: impl Fn(u16) -> bool,
) -> Option<u16> {
    let range = u32::from(hi) - u32::from(lo) + 1;
    let offset = (tuple_hash(key, local, 0, remote, rport) as u32).wrapping_add(counter);
    for i in 0..range {
        let p = lo + ((offset.wrapping_add(i)) % range) as u16;
        if !used(p) {
            return Some(p);
        }
    }
    None
}
