//! Sequence-number arithmetic modulo 2^32 (RFC 9293 3.4): one comparison helper, tested at the wrap.

/// `a` is before `b` (the signed distance from `a` to `b` is positive).
pub fn lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}
pub fn le(a: u32, b: u32) -> bool {
    a == b || lt(a, b)
}
pub fn gt(a: u32, b: u32) -> bool {
    lt(b, a)
}
pub fn ge(a: u32, b: u32) -> bool {
    le(b, a)
}
/// `lo <= x < hi` (all modulo 2^32, with `hi - lo` below 2^31).
pub fn in_range(x: u32, lo: u32, hi: u32) -> bool {
    x.wrapping_sub(lo) < hi.wrapping_sub(lo)
}
/// The distance from `a` forward to `b`, as an unsigned count (meaningful when `a <= b`).
pub fn dist(a: u32, b: u32) -> u32 {
    b.wrapping_sub(a)
}
