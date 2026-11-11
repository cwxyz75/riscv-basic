//! Turtle graphics for LOGO: a [`Canvas`] trait, a line rasteriser and flood
//! fill, a host-testable [`Bitmap`] backend, and the [`Turtle`] itself. The
//! rules (pixel mapping, Bresenham variant, wrap, sprite) are written down in
//! the "Turtle graphics in detail" section of `LOGO.md`.
//!
//! `no_std` and free of `unsafe`: the kernel supplies a `Canvas` over the real
//! framebuffer, tests use `Bitmap`.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod canvas;
mod draw;
mod palette;
mod turtle;

pub use canvas::{Bitmap, Canvas};
pub use palette::rgb;
pub use turtle::Turtle;
