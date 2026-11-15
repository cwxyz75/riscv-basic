//! The TCP state machine (S9-SPEC 5): sans-IO, `no_std` + `alloc`, no `unsafe`, no panic on any input, nothing allocated in
//! proportion to anything a packet claims (the buffers are sized when the connection is made). Time is an opaque monotonic
//! nanosecond count the caller passes in.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod conn;
pub mod reasm;
pub mod rto;
pub mod sendbuf;
pub mod seq;
pub mod siphash;
