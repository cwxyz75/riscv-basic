#!/usr/bin/env python3
"""LOGO on the real machine (milestone L2): terminal in, framebuffer out.

Usage: logo_test.py KERNEL_ELF

Boots with `-device ramfb`, chooses LOGO at the menu and types a few lines:
a 100-pixel turtle square, a `print`, and a flood fill. The console must show
the LOGO prompt and the printed answer, and a screenshot taken through the QEMU
monitor (`screendump`, no window needed) must show the picture pixel by pixel:
a white outline, a red interior, a black background.
"""
import os
import socket
import sys
import tempfile
import time

from boot_test import boot, check

WIDTH, HEIGHT = 640, 480
WHITE, RED, BLACK = (255, 255, 255), (255, 0, 0), (0, 0, 0)

LINES = [
    b"ht\r",                       # hide the turtle: no sprite in the picture
    b"repeat 4 [fd 100 rt 90]\r",  # a square from the centre up and to the right
    b"print 6 * 7\r",
    b"pu\r",
    b"setxy 50 50\r",              # inside the square
    b"setpc 4\r",                  # red
    b"fill\r",
]


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def monitor_command(port: int, command: str) -> None:
    with socket.create_connection(("127.0.0.1", port), timeout=5) as m:
        m.settimeout(5)
        buffer = b""
        while b"(qemu)" not in buffer:
            buffer += m.recv(4096)
        m.sendall(command.encode() + b"\n")
        time.sleep(1.0)  # screendump writes the file before returning


def read_ppm(path: str):
    magic, size, maxval, pixels = open(path, "rb").read().split(b"\n", 3)
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
        if "fill\n? " not in text:
            return False
        if not state["grabbed"]:
            time.sleep(0.5)  # let QEMU's display refresh pick up the pixels
            monitor_command(port, f"screendump {shot}")
            state["grabbed"] = True
        return True

    steps = ["RISCV LOGO V0.2"] + [(line, 0.1) for line in LINES] + [grab]
    steps = [s if isinstance(s, tuple) else (s, 0.0) for s in steps]
    text, timed_out, returncode = boot(
        sys.argv[1],
        steps,
        menu_choice=b"2",
        timeout=40,
        extra_args=["-device", "ramfb"],
        monitor=f"tcp:127.0.0.1:{port},server,nowait",
        stop_when=[lambda t: state["grabbed"]],
    )
    status = check(
        text,
        [
            "Your choice: 2\nRISCV LOGO V0.2\n\n? ",
            "repeat 4 [fd 100 rt 90]\n? ",
            "print 6 * 7\n42\n? ",
        ],
        timed_out,
        returncode,
    )
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

        # The square: logo (0,0) is pixel (320,240); it spans columns 320..420 and
        # rows 140..240, its outline white and its inside filled red.
        outline = [(320, 240), (320, 200), (320, 140), (370, 140), (420, 140), (420, 190), (420, 240), (370, 240)]
        inside = [(321, 141), (370, 190), (419, 239), (330, 230), (410, 150)]
        outside = [(319, 190), (421, 190), (370, 139), (370, 241), (100, 100), (600, 400), (0, 0), (639, 479)]
        for x, y in outline:
            if at(x, y) != WHITE:
                problems.append(f"outline ({x},{y}): {at(x, y)}, expected white")
        for x, y in inside:
            if at(x, y) != RED:
                problems.append(f"inside ({x},{y}): {at(x, y)}, expected red")
        for x, y in outside:
            if at(x, y) != BLACK:
                problems.append(f"outside ({x},{y}): {at(x, y)}, expected black")
    if problems:
        print("LOGO screen test FAILED:", *problems[:12], sep="\n  ", file=sys.stderr)
        return 1
    print("logo test passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
