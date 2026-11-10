#!/usr/bin/env python3
"""An independent check of the LOGO demo pictures.

Usage:
    verify_demo_images.py           compare logo/tests/golden/demo_*.img with this renderer
    verify_demo_images.py --write   (re)write them from this renderer

The golden images of the showpiece programs (spiral, tree, Koch snowflake,
rotated squares) are too big to work out by hand and must not simply be copied
from the Rust implementation's own output. So this file is a second, separate
implementation of the turtle rules in "Turtle graphics in detail" in LOGO.md
(rounding, Bresenham, wrap, heading arithmetic), and it draws each demo by
calling the turtle moves directly, never through a LOGO interpreter. The
expected images come from here; the Rust turtle and LOGO engines must reproduce
them pixel for pixel, and CI re-checks that they still do.
"""
import math
import pathlib
import sys

GOLDEN = pathlib.Path(__file__).resolve().parent.parent / "logo" / "tests" / "golden"
DEG = math.pi / 180.0


def round_half_up(v: float) -> int:
    return math.floor(v + 0.5)


class Turtle:
    """The turtle of LOGO.md on a width x height canvas (sprite never shown)."""

    def __init__(self, width: int, height: int):
        self.w, self.h = width, height
        self.pixels = [[0] * width for _ in range(height)]
        self.x = self.y = 0.0
        self.heading = 0.0
        self.pen_down = True
        self.color = 7

    # --- drawing -------------------------------------------------------------
    def plot(self, col: int, row: int) -> None:
        if 0 <= col < self.w and 0 <= row < self.h:
            self.pixels[row][col] = self.color

    def line(self, x0: int, y0: int, x1: int, y1: int) -> None:
        """Bresenham, both end points included (the variant written in LOGO.md)."""
        dx, dy = abs(x1 - x0), abs(y1 - y0)
        sx = 1 if x0 < x1 else -1
        sy = 1 if y0 < y1 else -1
        err = dx - dy
        x, y = x0, y0
        while True:
            self.plot(x, y)
            if (x, y) == (x1, y1):
                return
            e2 = 2 * err
            if e2 > -dy:
                err -= dy
                x += sx
            if e2 < dx:
                err += dx
                y += sy

    def column(self, x: float) -> int:
        return self.w // 2 + round_half_up(x)

    def row(self, y: float) -> int:
        return self.h // 2 - round_half_up(y)

    def segment(self, x0, y0, x1, y1) -> None:
        if self.pen_down:
            self.line(self.column(x0), self.row(y0), self.column(x1), self.row(y1))

    # --- moving (with wrap) -----------------------------------------------------
    def travel(self, dx: float, dy: float) -> None:
        half_w, half_h = self.w / 2.0, self.h / 2.0
        x0, y0 = self.x, self.y
        rx, ry = dx, dy
        for _ in range(4097):
            x1, y1 = x0 + rx, y0 + ry
            t, exit_edge = 1.0, None
            if x1 > half_w:
                c = (half_w - x0) / rx
                if c < t:
                    t, exit_edge = c, ("x", +1)
            elif x1 < -half_w:
                c = (-half_w - x0) / rx
                if c < t:
                    t, exit_edge = c, ("x", -1)
            if y1 > half_h:
                c = (half_h - y0) / ry
                if c < t:
                    t, exit_edge = c, ("y", +1)
            elif y1 < -half_h:
                c = (-half_h - y0) / ry
                if c < t:
                    t, exit_edge = c, ("y", -1)
            if exit_edge is None:
                self.segment(x0, y0, x1, y1)
                self.x, self.y = x1, y1
                return
            t = max(t, 0.0)
            ex, ey = x0 + rx * t, y0 + ry * t
            axis, sign = exit_edge
            if axis == "x":
                ex = sign * half_w
                nx, ny = -ex, ey
            else:
                ey = sign * half_h
                nx, ny = ex, -ey
            self.segment(x0, y0, ex, ey)
            rx *= 1.0 - t
            ry *= 1.0 - t
            x0, y0 = nx, ny
        self.x, self.y = x0, y0

    def forward(self, d: float) -> None:
        h = self.heading * DEG
        self.travel(d * math.sin(h), d * math.cos(h))

    def back(self, d: float) -> None:
        self.forward(-d)

    def set_heading(self, degrees: float) -> None:
        h = math.fmod(degrees, 360.0)
        if h < 0.0:
            h += 360.0
        self.heading = h

    def left(self, a: float) -> None:
        self.set_heading(self.heading - a)

    def right(self, a: float) -> None:
        self.set_heading(self.heading + a)

    def set_xy(self, x: float, y: float) -> None:
        self.travel(x - self.x, y - self.y)

    def text(self) -> str:
        rows = ("".join("." if p == 0 else f"{p:x}" for p in row) for row in self.pixels)
        return "\n".join(rows) + "\n"


# --- the demos: the same turtle geometry as examples/logo/*.logo -------------------

def spiral(t: Turtle, size: float, angle: float) -> None:
    while not size > 30:
        t.forward(size)
        t.right(angle)
        size = size + 2


def tree(t: Turtle, size: float) -> None:
    if size < 4:
        return
    t.forward(size)
    t.left(25)
    tree(t, size * 0.7)
    t.right(50)
    tree(t, size * 0.7)
    t.left(25)
    t.back(size)


def koch(t: Turtle, size: float, depth: float) -> None:
    if depth == 0:
        t.forward(size)
        return
    koch(t, size / 3, depth - 1)
    t.left(60)
    koch(t, size / 3, depth - 1)
    t.right(120)
    koch(t, size / 3, depth - 1)
    t.left(60)
    koch(t, size / 3, depth - 1)


def snowflake(t: Turtle, size: float, depth: float) -> None:
    for _ in range(3):
        koch(t, size, depth)
        t.right(120)


def square(t: Turtle, size: float) -> None:
    for _ in range(4):
        t.forward(size)
        t.right(90)


def flower(t: Turtle, size: float) -> None:
    for _ in range(36):
        square(t, size)
        t.right(10)


def demo_spiral() -> Turtle:
    t = Turtle(100, 80)
    spiral(t, 2, 91)
    return t


def demo_tree() -> Turtle:
    t = Turtle(100, 100)
    t.pen_down = False
    t.set_xy(0, -45)
    t.pen_down = True
    tree(t, 30)
    return t


def demo_koch() -> Turtle:
    t = Turtle(100, 100)
    t.pen_down = False
    t.set_xy(-27, -16)
    t.pen_down = True
    snowflake(t, 54, 3)
    return t


def demo_flower() -> Turtle:
    t = Turtle(80, 80)
    flower(t, 20)
    return t


DEMOS = {"demo_spiral": demo_spiral, "demo_tree": demo_tree, "demo_koch": demo_koch, "demo_flower": demo_flower}


def main() -> int:
    write = "--write" in sys.argv
    status = 0
    for name, make in DEMOS.items():
        image = make().text()
        path = GOLDEN / f"{name}.img"
        if write:
            path.write_text(image)
            print(f"wrote {path.name} ({image.count(chr(10))} rows)")
        elif not path.exists() or path.read_text() != image:
            print(f"MISMATCH: {path.name} differs from the independent renderer", file=sys.stderr)
            status = 1
        else:
            print(f"ok: {path.name}")
    return status


if __name__ == "__main__":
    sys.exit(main())
