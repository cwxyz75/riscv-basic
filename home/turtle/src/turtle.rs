//! The turtle: position, heading, pen and sprite, drawing onto a [`Canvas`].
//! See "Turtle graphics in detail" in `LOGO.md` for the rules.

use crate::canvas::Canvas;
use crate::draw;
use libm::{cos, floor, sin};

/// A single move is cut off after this many crossings of the edge (wrap).
const MAX_CROSSINGS: usize = 4096;
/// Sprite geometry: tip distance, corner distance, corner angle from the heading.
const SPRITE_TIP: f64 = 8.0;
const SPRITE_SIDE: f64 = 7.0;
const SPRITE_ANGLE: f64 = 150.0;
const DEGREES: f64 = core::f64::consts::PI / 180.0;

#[derive(Debug, Clone)]
pub struct Turtle {
    x: f64,
    y: f64,
    /// Degrees clockwise from straight up, in `[0, 360)`.
    heading: f64,
    pen_down: bool,
    pen_color: u8,
    visible: bool,
    /// Whether the sprite is currently toggled onto the canvas.
    sprite_drawn: bool,
}

impl Default for Turtle {
    fn default() -> Self {
        Self::new()
    }
}

/// `round`: halves go up.
fn round(v: f64) -> i32 {
    floor(v + 0.5) as i32
}

impl Turtle {
    /// At home, heading up, pen down, white, sprite shown (it appears on the
    /// canvas at the first drawing operation, or on [`Turtle::refresh`]).
    pub fn new() -> Self {
        Turtle {
            x: 0.0,
            y: 0.0,
            heading: 0.0,
            pen_down: true,
            pen_color: 7,
            visible: true,
            sprite_drawn: false,
        }
    }

    pub fn x(&self) -> f64 {
        self.x
    }
    pub fn y(&self) -> f64 {
        self.y
    }
    pub fn heading(&self) -> f64 {
        self.heading
    }
    pub fn is_pen_down(&self) -> bool {
        self.pen_down
    }
    pub fn pen_color(&self) -> u8 {
        self.pen_color
    }
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    // --- pixel mapping --------------------------------------------------

    fn column(canvas: &impl Canvas, x: f64) -> i32 {
        canvas.width() / 2 + round(x)
    }

    fn row(canvas: &impl Canvas, y: f64) -> i32 {
        canvas.height() / 2 - round(y)
    }

    // --- the sprite -----------------------------------------------------

    fn toggle_sprite(&self, canvas: &mut impl Canvas) {
        let point = |distance: f64, angle: f64| {
            let h = (self.heading + angle) * DEGREES;
            (
                Self::column(canvas, self.x + distance * sin(h)),
                Self::row(canvas, self.y + distance * cos(h)),
            )
        };
        let tip = point(SPRITE_TIP, 0.0);
        let right = point(SPRITE_SIDE, SPRITE_ANGLE);
        let left = point(SPRITE_SIDE, -SPRITE_ANGLE);
        draw::toggle_line_open(canvas, tip.0, tip.1, right.0, right.1);
        draw::toggle_line_open(canvas, right.0, right.1, left.0, left.1);
        draw::toggle_line_open(canvas, left.0, left.1, tip.0, tip.1);
    }

    fn erase_sprite(&mut self, canvas: &mut impl Canvas) {
        if self.sprite_drawn {
            self.toggle_sprite(canvas);
            self.sprite_drawn = false;
        }
    }

    fn draw_sprite(&mut self, canvas: &mut impl Canvas) {
        if self.visible && !self.sprite_drawn {
            self.toggle_sprite(canvas);
            self.sprite_drawn = true;
        }
    }

    /// Make the sprite match the turtle's state (drawn if shown). Safe to call
    /// at any time.
    pub fn refresh(&mut self, canvas: &mut impl Canvas) {
        self.erase_sprite(canvas);
        self.draw_sprite(canvas);
    }

    pub fn hide(&mut self, canvas: &mut impl Canvas) {
        self.erase_sprite(canvas);
        self.visible = false;
    }

    pub fn show(&mut self, canvas: &mut impl Canvas) {
        self.visible = true;
        self.draw_sprite(canvas);
    }

    // --- moving ---------------------------------------------------------

    /// Move by `(dx, dy)`, drawing if the pen is down, wrapping at the edges.
    fn travel(&mut self, canvas: &mut impl Canvas, dx: f64, dy: f64) {
        let half_w = f64::from(canvas.width()) / 2.0;
        let half_h = f64::from(canvas.height()) / 2.0;
        let (mut x0, mut y0) = (self.x, self.y);
        let (mut rx, mut ry) = (dx, dy);
        for _ in 0..=MAX_CROSSINGS {
            let (x1, y1) = (x0 + rx, y0 + ry);
            // The first edge the move would cross, as a fraction `t` of what is left.
            let mut t = 1.0;
            let mut exit: Option<(bool, bool)> = None; // (horizontal edge, positive side)
            let mut consider =
                |limit: f64, from: f64, delta: f64, horizontal: bool, positive: bool| {
                    let crossing = (limit - from) / delta;
                    if crossing < t {
                        t = crossing;
                        exit = Some((horizontal, positive));
                    }
                };
            if x1 > half_w {
                consider(half_w, x0, rx, true, true);
            } else if x1 < -half_w {
                consider(-half_w, x0, rx, true, false);
            }
            if y1 > half_h {
                consider(half_h, y0, ry, false, true);
            } else if y1 < -half_h {
                consider(-half_h, y0, ry, false, false);
            }
            let Some((horizontal, positive)) = exit else {
                self.draw_segment(canvas, x0, y0, x1, y1);
                self.x = x1;
                self.y = y1;
                return;
            };
            let t = t.max(0.0);
            let (mut ex, mut ey) = (x0 + rx * t, y0 + ry * t);
            // Land exactly on the edge, then reappear on the opposite one.
            let (nx, ny);
            if horizontal {
                ex = if positive { half_w } else { -half_w };
                nx = -ex;
                ny = ey;
            } else {
                ey = if positive { half_h } else { -half_h };
                nx = ex;
                ny = -ey;
            }
            self.draw_segment(canvas, x0, y0, ex, ey);
            rx *= 1.0 - t;
            ry *= 1.0 - t;
            x0 = nx;
            y0 = ny;
        }
        // Too many crossings: stop where we are.
        self.x = x0;
        self.y = y0;
    }

    fn draw_segment(&self, canvas: &mut impl Canvas, x0: f64, y0: f64, x1: f64, y1: f64) {
        if self.pen_down {
            draw::line(
                canvas,
                Self::column(canvas, x0),
                Self::row(canvas, y0),
                Self::column(canvas, x1),
                Self::row(canvas, y1),
                self.pen_color,
            );
        }
    }

    /// Run `action` with the sprite taken off the canvas, and put it back after.
    fn without_sprite<C: Canvas>(
        &mut self,
        canvas: &mut C,
        action: impl FnOnce(&mut Self, &mut C),
    ) {
        self.erase_sprite(canvas);
        action(self, canvas);
        self.draw_sprite(canvas);
    }

    pub fn forward(&mut self, canvas: &mut impl Canvas, distance: f64) {
        self.without_sprite(canvas, |t, c| {
            let h = t.heading * DEGREES;
            t.travel(c, distance * sin(h), distance * cos(h));
        });
    }

    pub fn back(&mut self, canvas: &mut impl Canvas, distance: f64) {
        self.forward(canvas, -distance);
    }

    pub fn left(&mut self, canvas: &mut impl Canvas, degrees: f64) {
        self.set_heading(canvas, self.heading - degrees);
    }

    pub fn right(&mut self, canvas: &mut impl Canvas, degrees: f64) {
        self.set_heading(canvas, self.heading + degrees);
    }

    pub fn set_heading(&mut self, canvas: &mut impl Canvas, degrees: f64) {
        self.without_sprite(canvas, |t, _| {
            let mut h = degrees % 360.0;
            if h < 0.0 {
                h += 360.0;
            }
            t.heading = h;
        });
    }

    pub fn set_xy(&mut self, canvas: &mut impl Canvas, x: f64, y: f64) {
        self.without_sprite(canvas, |t, c| {
            let (dx, dy) = (x - t.x, y - t.y);
            t.travel(c, dx, dy);
        });
    }

    pub fn home(&mut self, canvas: &mut impl Canvas) {
        self.without_sprite(canvas, |t, _| {
            t.x = 0.0;
            t.y = 0.0;
            t.heading = 0.0;
        });
    }

    /// Clear the canvas and go home.
    pub fn clear_screen(&mut self, canvas: &mut impl Canvas) {
        self.without_sprite(canvas, |t, c| {
            c.clear(0);
            t.x = 0.0;
            t.y = 0.0;
            t.heading = 0.0;
        });
    }

    /// Clear the canvas; the turtle stays where it is.
    pub fn clean(&mut self, canvas: &mut impl Canvas) {
        self.without_sprite(canvas, |_, c| c.clear(0));
    }

    pub fn pen_up(&mut self) {
        self.pen_down = false;
    }

    pub fn pen_down(&mut self) {
        self.pen_down = true;
    }

    /// Set the pen colour (the low four bits count).
    pub fn set_pen_color(&mut self, color: u8) {
        self.pen_color = color & 0x0F;
    }

    /// Flood-fill the region under the turtle with the pen colour.
    pub fn fill(&mut self, canvas: &mut impl Canvas) {
        self.without_sprite(canvas, |t, c| {
            let (x, y) = (Self::column(c, t.x), Self::row(c, t.y));
            draw::fill(c, x, y, t.pen_color);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::Bitmap;

    #[test]
    fn heading_is_kept_in_range() {
        let mut canvas = Bitmap::new(21, 21);
        let mut t = Turtle::new();
        t.right(&mut canvas, 450.0);
        assert_eq!(t.heading(), 90.0);
        t.left(&mut canvas, 100.0);
        assert_eq!(t.heading(), 350.0);
        t.set_heading(&mut canvas, -10.0);
        assert_eq!(t.heading(), 350.0);
    }

    #[test]
    fn queries_follow_moves() {
        let mut canvas = Bitmap::new(41, 41);
        let mut t = Turtle::new();
        t.hide(&mut canvas);
        t.right(&mut canvas, 90.0);
        t.forward(&mut canvas, 10.0);
        assert!((t.x() - 10.0).abs() < 1e-9 && t.y().abs() < 1e-9);
        t.back(&mut canvas, 4.0);
        assert!((t.x() - 6.0).abs() < 1e-9);
        t.home(&mut canvas);
        assert_eq!((t.x(), t.y(), t.heading()), (0.0, 0.0, 0.0));
        t.set_pen_color(0x1f);
        assert_eq!(t.pen_color(), 15);
    }

    #[test]
    fn the_sprite_is_drawn_and_erased_exactly() {
        let mut canvas = Bitmap::new(31, 31);
        let blank = canvas.clone();
        let mut t = Turtle::new();
        t.refresh(&mut canvas);
        assert_ne!(canvas, blank, "a shown turtle leaves a sprite");
        t.refresh(&mut canvas); // idempotent
        let shown = canvas.clone();
        t.hide(&mut canvas);
        assert_eq!(canvas, blank, "hiding erases it without a trace");
        t.show(&mut canvas);
        assert_eq!(canvas, shown);
    }

    #[test]
    fn the_sprite_never_ends_up_in_the_picture() {
        // Same drawing with the sprite shown and hidden: removing the sprite
        // from the shown one must give exactly the hidden one.
        fn square(t: &mut Turtle, canvas: &mut Bitmap) {
            for _ in 0..4 {
                t.forward(canvas, 12.0);
                t.right(canvas, 90.0);
            }
        }
        let mut hidden_canvas = Bitmap::new(41, 41);
        let mut hidden = Turtle::new();
        hidden.hide(&mut hidden_canvas);
        square(&mut hidden, &mut hidden_canvas);
        let mut shown_canvas = Bitmap::new(41, 41);
        let mut shown = Turtle::new();
        square(&mut shown, &mut shown_canvas);
        shown.hide(&mut shown_canvas);
        assert_eq!(shown_canvas, hidden_canvas);
    }

    #[test]
    fn fill_ignores_the_sprite_and_a_same_colour_target() {
        let mut canvas = Bitmap::new(21, 21);
        let mut t = Turtle::new();
        t.set_pen_color(3);
        t.fill(&mut canvas); // whole blank canvas, sprite on top of it
        t.hide(&mut canvas);
        assert!((0..21).all(|y| (0..21).all(|x| canvas.pixel(x, y) == 3)));
        let before = canvas.clone();
        t.fill(&mut canvas); // already that colour: nothing to do
        assert_eq!(canvas, before);
    }

    #[test]
    fn a_huge_move_terminates() {
        let mut canvas = Bitmap::new(8, 8);
        let mut t = Turtle::new();
        t.hide(&mut canvas);
        t.forward(&mut canvas, 1e12);
        assert!(t.x().is_finite() && t.y().is_finite());
    }
}
