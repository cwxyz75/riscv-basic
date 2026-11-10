#!/usr/bin/env python3
"""Take the README screenshot: boot the kernel, choose LOGO, type the example
programs and a few drawing commands, and save what the screen shows as a PNG.

Usage: screenshot_logo.py KERNEL_ELF OUTPUT.png

The picture is a real screen capture from the running machine (QEMU's `screendump`
through the monitor), nothing is rendered by this script apart from the PNG
encoding.
"""
import os
import pathlib
import socket
import struct
import sys
import tempfile
import time
import zlib

from boot_test import boot

ROOT = pathlib.Path(__file__).resolve().parent.parent

COMMANDS = [
    "ht",
    "setpc 6",
    "flower 80",              # yellow rosette of 36 rotated squares, in the middle
    "pu",
    "setxy -235 -150",
    "pd",
    "setpc 3",
    "snowflake 150 4",        # cyan Koch snowflake, left
    "pu",
    "setxy 190 -225",
    "pd",
    "setpc 2",
    "tree 70",                # green tree, right
]
PROGRAMS = ["squares.logo", "koch.logo", "tree.logo"]


def write_png(path: str, width: int, height: int, rgb: bytes) -> None:
    def chunk(kind: bytes, data: bytes) -> bytes:
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    rows = b"".join(b"\x00" + rgb[y * width * 3 : (y + 1) * width * 3] for y in range(height))
    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(rows, 9))
    png += chunk(b"IEND", b"")
    pathlib.Path(path).write_bytes(png)


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    kernel, output = sys.argv[1], sys.argv[2]
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    shot = os.path.join(tempfile.mkdtemp(), "screen.ppm")
    state = {"grabbed": False}

    lines = []
    for name in PROGRAMS:
        lines += (ROOT / "examples" / "logo" / name).read_text().splitlines()
    lines += COMMANDS
    typed = b"".join(line.encode() + b"\r" for line in lines)

    def grab(text: str) -> bool:
        # The last drawing command has been echoed and its prompt is back.
        if "tree 70\n? " not in text:
            return False
        if not state["grabbed"]:
            time.sleep(1.0)
            with socket.create_connection(("127.0.0.1", port), timeout=5) as m:
                m.settimeout(5)
                buffer = b""
                while b"(qemu)" not in buffer:
                    buffer += m.recv(4096)
                m.sendall(f"screendump {shot}\n".encode())
                time.sleep(1.5)
            state["grabbed"] = True
        return True

    boot(
        kernel,
        [("RISCV LOGO V0.2", 0.0), (typed, 0.0), (grab, 0.0)],
        menu_choice=b"2",
        timeout=120,
        extra_args=["-device", "ramfb"],
        monitor=f"tcp:127.0.0.1:{port},server,nowait",
        stop_when=[lambda t: state["grabbed"]],
    )
    magic, size, _, pixels = open(shot, "rb").read().split(b"\n", 3)
    width, height = (int(v) for v in size.split())
    write_png(output, width, height, pixels)
    print(f"wrote {output} ({width}x{height}, {os.path.getsize(output)} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
