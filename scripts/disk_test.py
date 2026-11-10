#!/usr/bin/env python3
"""SAVE and LOAD on the real machine (milestone M7d).

Usage: disk_test.py KERNEL_ELF

Every step boots QEMU with a virtio block device backed by a temporary image,
made and inspected by `disk.py`, an implementation of the format that shares no
code with the kernel's:

  1. BASIC saves a program, then `BYE` reboots; after the reboot BASIC lists the
     directory and loads and runs it (persistence across a reboot).
  2. A fresh QEMU process on the same image loads and runs it (persistence across
     power-off). The host then reads the image: the file is there, as text.
  3. LOGO saves two procedures; a fresh QEMU process loads them and uses them.
  4. A file the host put on the image (`disk.py put`) is loaded by the kernel;
     BASIC sees the LOGO files in DIR and refuses to LOAD one.
  5. Without a disk attached, the commands say so.
  6. A blank (never formatted) image gives DISK NOT FORMATTED.
"""
import os
import sys
import tempfile

sys.path.insert(0, os.path.dirname(__file__))
import disk
from boot_test import boot, check

DISK_ARGS = lambda image: [
    "-global", "virtio-mmio.force-legacy=false",
    "-drive", f"file={image},if=none,format=raw,id=hd0",
    "-device", "virtio-blk-device,drive=hd0",
]


def session(kernel, image, choice, lines, until, extra=()):
    """Boot with the disk, pick a language, type `lines` (each waits for the
    previous output to settle), and return the transcript."""
    steps = []
    for line in lines:
        steps.append((line.encode() + b"\r", 0.4))
    text, timed_out, rc = boot(
        kernel, steps, menu_choice=choice, timeout=60,
        extra_args=DISK_ARGS(image) if image else [], stop_when=[until],
    )
    return text, timed_out, rc


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    kernel = sys.argv[1]
    failed = 0
    with tempfile.TemporaryDirectory() as tmp:
        image = os.path.join(tmp, "disk.img")
        disk.new_image(image)

        # 1. BASIC: save, reboot, load, run
        steps = [
            (b'10 PRINT "PERSIST"\r', 0.3),
            (b'SAVE "Test"\r', 0.5),
            (b"BYE\r", 0.0),
            lambda t: t.count("2) LOGO") >= 2,
            (b"1", 0.5),
            (b"DIR\r", 0.5),
            (b"NEW\r", 0.3),
            (b'LOAD "TEST"\r', 0.5),
            (b"RUN\r", 0.5),
        ]
        steps = [s if isinstance(s, tuple) else (s, 0.0) for s in steps]
        text, timed_out, rc = boot(
            kernel, steps, menu_choice=b"1", timeout=60,
            extra_args=DISK_ARGS(image), stop_when=["PERSIST\nREADY."],
        )
        failed |= check(text, [
            'SAVE "Test"\nREADY.',
            "TEST              BASIC      19\n",
            "BLOCKS FREE.",
            "PERSIST\nREADY.",
        ], timed_out, rc, forbidden=["ERROR"])
        # the number of free blocks: 4096 - 6 reserved - 1 used
        failed |= check(text, ["4089 BLOCKS FREE."], timed_out, rc)

        # 2. a fresh QEMU process; then the host looks at the image
        text, timed_out, rc = session(kernel, image, b"1",
            ['LOAD "test"', "LIST", "RUN"], "PERSIST\nREADY.")
        failed |= check(text, ['10 PRINT "PERSIST"\nREADY.', "PERSIST\nREADY."], timed_out, rc, forbidden=["ERROR"])
        files = disk.read_files(image)  # also checks chains and the free map
        if files != {"TEST": ("BASIC", '10 PRINT "PERSIST"\n')}:
            print("host view of the image is wrong:", files, file=sys.stderr)
            failed = 1

        # 3. LOGO: save, fresh process, load and use
        text, timed_out, rc = session(kernel, image, b"2",
            ["to sq :n", "repeat 4 [fd :n rt 90]", "end", 'save "shapes', "dir"], "BLOCKS FREE.")
        failed |= check(text, ["SHAPES            LOGO       "], timed_out, rc)
        text, timed_out, rc = session(kernel, image, b"2",
            ['load "shapes', "print pos", "sq 10", "print heading", 'erasefile "nothere'], "File not found")
        failed |= check(text, ["load \"shapes\n? print pos\n0 0\n", "print heading\n0\n", "File not found"], timed_out, rc)
        files = disk.read_files(image)
        if files.get("SHAPES") != ("LOGO", "to sq :n\nrepeat 4 [fd :n rt 90]\nend\n"):
            print("host view of SHAPES is wrong:", files, file=sys.stderr)
            failed = 1

        # 4. a file made on the host; BASIC and LOGO files seen across languages
        disk.put_file(image, "basic", "hostmade", '10 PRINT "FROM HOST"\n')
        text, timed_out, rc = session(kernel, image, b"1",
            ['LOAD "hostmade"', "RUN", 'LOAD "shapes"', "DIR"], "BLOCKS FREE.")
        failed |= check(text, [
            "FROM HOST\nREADY.",
            "?FILE TYPE MISMATCH ERROR\nREADY.",
            "HOSTMADE          BASIC      21\n",
            "SHAPES            LOGO       ",
        ], timed_out, rc)

        # 6. a blank image
        blank = os.path.join(tmp, "blank.img")
        with open(blank, "wb") as f:
            f.write(bytes(4096 * 512))
        text, timed_out, rc = session(kernel, blank, b"1", ["DIR"], "ERROR")
        failed |= check(text, ["?DISK NOT FORMATTED ERROR"], timed_out, rc)

    # 5. no disk at all
    text, timed_out, rc = session(kernel, None, b"1", ['SAVE "X"'], "ERROR")
    failed |= check(text, ["?DEVICE NOT PRESENT ERROR"], timed_out, rc)
    text, timed_out, rc = session(kernel, None, b"2", ["dir"], "No disk attached")
    failed |= check(text, ["No disk attached"], timed_out, rc)

    if not failed:
        print("disk test passed")
    return failed


if __name__ == "__main__":
    sys.exit(main())
