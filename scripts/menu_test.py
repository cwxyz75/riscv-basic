#!/usr/bin/env python3
"""Milestone M9 boot test: the language menu and `BYE`.

Usage: menu_test.py KERNEL_ELF

Drives the boot menu by hand: a stray key is ignored; `2` says LOGO is not
installed yet and asks again; `1` starts BASIC. Then `BYE` must reboot the
machine back to the menu, and BASIC must work again after choosing it a second
time.
"""
import sys

from boot_test import boot, check


def menus(n):
    return lambda text: text.count("2) LOGO") >= n


def basic_banners(n):
    return lambda text: text.count("RISCV BASIC V0.1") >= n


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    steps = [
        (b"x", 0.1),                 # not a choice: ignored
        (b"2", 0.2),                 # LOGO: not installed yet, asks again
        (b"1", 0.2),                 # BASIC
        basic_banners(1),
        (b"PRINT 6*7\r", 0.2),
        (b"BYE\r", 0.0),             # reboot...
        menus(2),                    # ...and the menu comes back
        (b"1", 0.2),
        basic_banners(2),
        (b"PRINT 8*9\r", 0.2),
    ]
    steps = [s if isinstance(s, tuple) else (s, 0.0) for s in steps]
    text, timed_out, returncode = boot(
        sys.argv[1],
        steps,
        menu_choice=b"",
        timeout=40,
        stop_when=["PRINT 8*9\n 72 \nREADY."],
    )
    status = check(
        text,
        [
            "Choose a language:\n  1) BASIC\n  2) LOGO\nYour choice: ",
            # Without `-device ramfb` the LOGO path says so before "not installed".
            "Your choice: 2\nframebuffer: no ramfb device (start QEMU with -device ramfb)\n"
            "LOGO is not installed yet.\nYour choice: 1\nRISCV BASIC V0.1",
            "PRINT 6*7\n 42 \nREADY.",
            "BYE\n",
            "PRINT 8*9\n 72 \nREADY.",
        ],
        timed_out,
        returncode,
    )
    if status:
        return status
    if text.count("2) LOGO") != 2 or text.count("RISCV BASIC V0.1") != 2:
        print("expected exactly two boots (two menus, two banners)", file=sys.stderr)
        print(text, file=sys.stderr)
        return 1
    print("menu test passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
