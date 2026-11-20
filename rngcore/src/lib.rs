//! Randomness primitives for the OS (S9 step 5): SHA-256 (FIPS 180-4), HMAC-SHA256 (RFC 2104), HKDF-SHA256 (RFC 5869), the
//! ChaCha20 block function (RFC 8439) and a generator with fast key erasure (Bernstein's construction, as OpenBSD and Linux
//! use it) that is seeded from device entropy and reseeded from time to time. `no_std`, no `unsafe`, no allocation, nothing
//! that depends on a clock or a device: the driver and the services feed it bytes.
#![no_std]
#![forbid(unsafe_code)]

pub mod chacha;
pub mod drbg;
pub mod pool;
pub mod sha256;

pub use drbg::Drbg;
pub use sha256::{hkdf, hmac_sha256, sha256};
