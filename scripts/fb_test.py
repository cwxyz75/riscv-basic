#!/usr/bin/env python3
"""Milestone L0 boot test: the framebuffer.

Usage: fb_test.py KERNEL_ELF

Boots with `-device ramfb`, picks LOGO at the menu (the kernel then configures
the framebuffer and draws a test pattern), and takes a screenshot through the
QEMU monitor (`screendump`, which works with no window at all). The screenshot
must be 640x480 and show the pattern: eight colour bars over a grey ramp, with a white turtle square (the first
drawing by the turtle crate through the kernel's framebuffer canvas) drawn on
top of the lower left part of the ramp.
"""
import os
import socket
import sys
import tempfile
import time

from boot_test import boot, check

WIDTH, HEIGHT = 640, 480
BARS = [(0, 0, 0), (255, 0, 0), (0, 255, 0), (255, 255, 0), (0, 0, 255), (255, 0, 255), (0, 255, 255), (255, 255, 255)]


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def monitor_command(port: int, command: str) -> None:
    """Send one command to the QEMU monitor and wait until it has been run."""
    with socket.create_connection(("127.0.0.1", port), timeout=5) as m:
        m.settimeout(5)
        buffer = b""
        while b"(qemu)" not in buffer:  # the banner and first prompt
            buffer += m.recv(4096)
        m.sendall(command.encode() + b"\n")
        time.sleep(1.0)  # screendump writes the file before returning


def read_ppm(path: str):
    data = open(path, "rb").read()
    magic, size, maxval, pixels = data.split(b"\n", 3)
    assert magic == b"P6" and maxval == b"255", (magic, maxval)
    width, height = (int(v) for v in size.split())
    return width, height, pixels


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    port = free_port()
    shot = os.path.join(tempfile.mkdtemp(), "screen.ppm")
    state = {"grabbed": False}

    def grab(text: str) -> bool:
        if "test pattern drawn" not in text:
            return False
        if not state["grabbed"]:
            time.sleep(0.3)  # let QEMU's display refresh pick up the pixels
            monitor_command(port, f"screendump {shot}")
            state["grabbed"] = True
        return True

    text, timed_out, returncode = boot(
        sys.argv[1],
        [(grab, 0.0)],
        menu_choice=b"2",
        timeout=30,
        extra_args=["-device", "ramfb"],
        monitor=f"tcp:127.0.0.1:{port},server,nowait",
        stop_when=[lambda t: state["grabbed"]],
    )
    status = check(text, ["framebuffer: 640x480 test pattern drawn", "LOGO is not installed yet."], timed_out, returncode)
    if status:
        return status
    if not os.path.exists(shot):
        print("no screenshot was written", file=sys.stderr)
        return 1

    width, height, pixels = read_ppm(shot)
    problems = []
    if (width, height) != (WIDTH, HEIGHT):
        problems.append(f"screenshot is {width}x{height}, expected {WIDTH}x{HEIGHT}")
    else:
        def at(x, y):
            i = (y * width + x) * 3
            return tuple(pixels[i : i + 3])

        for bar, colour in enumerate(BARS):
            for x in (bar * 80 + 1, bar * 80 + 40, bar * 80 + 78):
                for y in (0, 120, 239):
                    if at(x, y) != colour:
                        problems.append(f"bar {bar} at ({x},{y}): {at(x, y)}, expected {colour}")
        # The turtle square covers columns 170..250, rows 340..420: white outline,
        # and the ramp still showing through inside it.
        for x, y in [(170, 340), (170, 380), (170, 420), (210, 340), (210, 420), (250, 340), (250, 380), (250, 420)]:
            if at(x, y) != (255, 255, 255):
                problems.append(f"turtle square edge at ({x},{y}): {at(x, y)}, expected white")
        for x, y in [(171, 341), (210, 380), (249, 419), (169, 380), (251, 380), (210, 339), (210, 421)]:
            grey = x * 255 // (WIDTH - 1)
            if at(x, y) != (grey, grey, grey):
                problems.append(f"next to the square at ({x},{y}): {at(x, y)}, expected ramp {(grey,) * 3}")
        for x in (0, 1, 100, 319, 320, 500, 638, 639):
            grey = x * 255 // (WIDTH - 1)
            for y in (240, 360, 479):
                if at(x, y) != (grey, grey, grey):
                    problems.append(f"ramp at ({x},{y}): {at(x, y)}, expected {(grey,) * 3}")
    if problems:
        print("framebuffer test FAILED:", *problems[:12], sep="\n  ", file=sys.stderr)
        return 1
    print("framebuffer test passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
