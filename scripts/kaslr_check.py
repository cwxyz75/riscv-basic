#!/usr/bin/env python3
"""HD3 acceptance (KERNEL-V1-SPEC 7.3): ten boots of the kernel give at least six distinct slides of the image and at least four distinct direct-map
bases, and `kaslr=off` gives one of each. Each boot is stopped as soon as the kernel has printed its `kaslr:` line (a few seconds). Usage: kaslr_check.py KERNEL_ELF."""
import re
import subprocess
import sys

MACHINE = "virt,aia=aplic-imsic"
LINE = re.compile(r"kaslr: image slid (0x[0-9a-f]+) bytes, direct map at (0x[0-9a-f]+)")


def boot(kernel, args=""):
    cmd = ["qemu-system-riscv64", "-machine", MACHINE, "-bios", "default", "-m", "256M", "-display", "none", "-serial", "stdio", "-monitor", "none",
           "-cpu", "rva23s64", "-append", f"shard=k ram=256M {args}", "-kernel", kernel]
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, errors="replace")
    try:
        for line in p.stdout:
            m = LINE.search(line)
            if m:
                return int(m.group(1), 16), int(m.group(2), 16)
    finally:
        p.kill()
        p.wait()
    sys.exit("the kernel never printed its kaslr line")


def main():
    kernel = sys.argv[1]
    runs = [boot(kernel) for _ in range(10)]
    slides, bases = {s for s, _ in runs}, {b for _, b in runs}
    print(f"kaslr: ten boots, {len(slides)} distinct slides, {len(bases)} distinct direct-map bases")
    if len(slides) < 6 or len(bases) < 4:
        sys.exit("kaslr_check: too little variation (want at least 6 slides and 4 bases)")
    off = {boot(kernel, "kaslr=off") for _ in range(3)}
    if off != {(0, 0xFFFFFFC000000000)}:
        sys.exit(f"kaslr_check: kaslr=off gave {off}, not one unslid layout")
    print("kaslr: kaslr=off gives the unslid layout")


main()
