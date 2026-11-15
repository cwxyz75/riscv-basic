//! The Internet checksum (RFC 1071).

/// Add `data` to a running one's-complement sum, as big-endian 16-bit words (an odd last byte is the high byte of a
/// word padded with zero). The running sum is a `u32` and never overflows for inputs below 128 KiB; `fold` reduces it.
pub fn sum16(data: &[u8], init: u32) -> u32 {
    let mut s = init;
    for c in data.chunks(2) {
        let hi = u32::from(c[0]) << 8;
        let lo = c.get(1).map_or(0, |&b| u32::from(b));
        s = s.wrapping_add(hi | lo);
    }
    s
}

/// Fold the carries of a running sum into 16 bits.
pub fn fold(mut s: u32) -> u16 {
    while s > 0xffff {
        s = (s & 0xffff) + (s >> 16);
    }
    s as u16
}

/// The checksum of `data` (the complement of the folded sum).
pub fn checksum(data: &[u8]) -> u16 {
    !fold(sum16(data, 0))
}

/// The pseudo-header sum for UDP (and TCP) over IPv4.
pub fn pseudo(src: [u8; 4], dst: [u8; 4], proto: u8, len: u16) -> u32 {
    let mut s = sum16(&src, 0);
    s = sum16(&dst, s);
    s = s.wrapping_add(u32::from(proto));
    s.wrapping_add(u32::from(len))
}

/// Does `data` (which includes its own checksum field) verify, starting from `init`?
pub fn verifies(data: &[u8], init: u32) -> bool {
    fold(sum16(data, init)) == 0xffff
}
