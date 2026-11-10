//! The drawing surface the turtle draws on.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// A grid of pixels, each a palette index from 0 to 15.
pub trait Canvas {
    fn width(&self) -> i32;
    fn height(&self) -> i32;

    /// The colour at `(x, y)`; `0` outside the canvas.
    fn pixel(&self, x: i32, y: i32) -> u8;

    /// Set a pixel; positions outside the canvas are ignored.
    fn set_pixel(&mut self, x: i32, y: i32, color: u8);

    /// Fill the whole canvas with one colour.
    fn clear(&mut self, color: u8);

    /// Invert a pixel reversibly (XOR the palette index with 15): doing it
    /// twice restores the original. Used to draw and erase the turtle sprite.
    fn toggle_pixel(&mut self, x: i32, y: i32) {
        if x >= 0 && y >= 0 && x < self.width() && y < self.height() {
            let color = self.pixel(x, y);
            self.set_pixel(x, y, color ^ 0x0F);
        }
    }
}

/// An in-memory canvas: the host-side backend, used by the golden-image tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    width: i32,
    height: i32,
    pixels: Vec<u8>,
}

impl Bitmap {
    /// A blank (colour 0) bitmap.
    pub fn new(width: i32, height: i32) -> Self {
        Bitmap {
            width,
            height,
            pixels: vec![0; (width.max(0) * height.max(0)) as usize],
        }
    }

    /// The picture as text, one line per row: `.` for colour 0 and a lower-case
    /// hex digit for any other colour.
    pub fn to_ascii(&self) -> String {
        let mut out = String::new();
        for y in 0..self.height {
            for x in 0..self.width {
                out.push(match self.pixel(x, y) {
                    0 => '.',
                    n => char::from_digit(u32::from(n), 16).unwrap_or('?'),
                });
            }
            out.push('\n');
        }
        out
    }
}

impl Canvas for Bitmap {
    fn width(&self) -> i32 {
        self.width
    }

    fn height(&self) -> i32 {
        self.height
    }

    fn pixel(&self, x: i32, y: i32) -> u8 {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return 0;
        }
        self.pixels[(y * self.width + x) as usize]
    }

    fn set_pixel(&mut self, x: i32, y: i32, color: u8) {
        if x >= 0 && y >= 0 && x < self.width && y < self.height {
            self.pixels[(y * self.width + x) as usize] = color & 0x0F;
        }
    }

    fn clear(&mut self, color: u8) {
        self.pixels.fill(color & 0x0F);
    }
}
