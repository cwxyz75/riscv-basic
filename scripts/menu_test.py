#!/usr/bin/env python3
"""The language menu, and rebooting back to it (milestones M9 and L2).

Usage: menu_test.py KERNEL_ELF

Drives the boot menu by hand, with no framebuffer device: a stray key is
ignored; `2` starts LOGO (text only, with a note that there is no screen); LOGO's
`bye` reboots the machine back to the menu; `1` starts BASIC; BASIC's `BYE`
reboots to the menu again. Three boots, three menus.
"""
import sys

from boot_test import boot, check


def menus(n):
    return lambda text: text.count("2) LOGO") >= n


def count_of(marker, n):
    return lambda text: text.count(marker) >= n


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    steps = [
        (b"x", 0.1),                  # not a choice: ignored
        (b"2", 0.3),                  # LOGO
        count_of("RISCV LOGO V0.2", 1),
        (b"print 6 * 7\r", 0.2),
        (b"bye\r", 0.0),              # reboot...
        menus(2),                     # ...back to the menu
        (b"1", 0.2),                  # BASIC
        count_of("RISCV BASIC V0.1", 1),
        (b"PRINT 8*9\r", 0.2),
        (b"BYE\r", 0.0),              # reboot again
        menus(3),
    ]
    steps = [s if isinstance(s, tuple) else (s, 0.0) for s in steps]
    text, timed_out, returncode = boot(
        sys.argv[1], steps, menu_choice=b"", timeout=60, stop_when=[menus(3)]
    )
    status = check(
        text,
        [
            "Choose a language:\n  1) BASIC\n  2) LOGO\nYour choice: ",
            "Your choice: 2\n(no screen: no ramfb device (start QEMU with -device ramfb))\n"
            "RISCV LOGO V0.2\n\n? ",
            "print 6 * 7\n42\n? ",
            "bye\n",
            "Your choice: 1\nRISCV BASIC V0.1",
            "PRINT 8*9\n 72 \nREADY.",
            "BYE\n",
        ],
        timed_out,
        returncode,
    )
    if status:
        return status
    if (
        text.count("2) LOGO") != 3
        or text.count("RISCV LOGO V0.2") != 1
        or text.count("RISCV BASIC V0.1") != 1
    ):
        print("expected three menus, one LOGO banner and one BASIC banner", file=sys.stderr)
        print(text, file=sys.stderr)
        return 1
    print("menu test passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
