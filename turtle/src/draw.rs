//! Rasterising: lines, toggled lines, and flood fill.

use crate::canvas::Canvas;
use alloc::vec;

/// Visit every pixel of the Bresenham line from `(x0, y0)` to `(x1, y1)`, both
/// end points included (the variant specified in `LOGO.md`).
fn walk(x0: i32, y0: i32, x1: i32, y1: i32, mut visit: impl FnMut(i32, i32)) {
    let dx = (x1 - x0).abs();
    let dy = (y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let (mut x, mut y) = (x0, y0);
    let mut err = dx - dy;
    loop {
        visit(x, y);
        if x == x1 && y == y1 {
            return;
        }
        let e2 = 2 * err;
        if e2 > -dy {
            err -= dy;
            x += sx;
        }
        if e2 < dx {
            err += dx;
            y += sy;
        }
    }
}

/// Draw a line in `color`; pixels outside the canvas are skipped.
pub fn line(canvas: &mut impl Canvas, x0: i32, y0: i32, x1: i32, y1: i32, color: u8) {
    walk(x0, y0, x1, y1, |x, y| canvas.set_pixel(x, y, color));
}

/// Toggle every pixel of the line except its last, so that consecutive edges of
/// a polygon toggle their shared corner exactly once.
pub fn toggle_line_open(canvas: &mut impl Canvas, x0: i32, y0: i32, x1: i32, y1: i32) {
    walk(x0, y0, x1, y1, |x, y| {
        if (x, y) != (x1, y1) {
            canvas.toggle_pixel(x, y);
        }
    });
}

/// Flood-fill, with `color`, the 4-connected region around `(x, y)` that has the
/// same colour as that pixel. Does nothing if `(x, y)` is off the canvas or
/// already has `color`.
pub fn fill(canvas: &mut impl Canvas, x: i32, y: i32, color: u8) {
    let (width, height) = (canvas.width(), canvas.height());
    if x < 0 || y < 0 || x >= width || y >= height {
        return;
    }
    let target = canvas.pixel(x, y);
    if target == color & 0x0F {
        return;
    }
    // Span fill: take a seed, fill its whole horizontal run, and seed the runs
    // directly above and below. Keeps the stack small on big regions.
    let mut seeds = vec![(x, y)];
    while let Some((sx, sy)) = seeds.pop() {
        if canvas.pixel(sx, sy) != target {
            continue;
        }
        let mut left = sx;
        while left > 0 && canvas.pixel(left - 1, sy) == target {
            left -= 1;
        }
        let mut right = sx;
        while right < width - 1 && canvas.pixel(right + 1, sy) == target {
            right += 1;
        }
        for i in left..=right {
            canvas.set_pixel(i, sy, color);
        }
        for ny in [sy - 1, sy + 1] {
            if ny < 0 || ny >= height {
                continue;
            }
            let mut i = left;
            while i <= right {
                if canvas.pixel(i, ny) == target {
                    seeds.push((i, ny));
                    while i <= right && canvas.pixel(i, ny) == target {
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
        }
    }
}
