#!/usr/bin/env python3
"""The QEMU devices no test machine carries (IOMMU-SPEC amendment 2026-11-27 part 2, L7; OD175).

    refused_devices.py --self-test   check that a command line naming one is refused, by this module and by the boot harness

A device that can be commanded to read or report memory at a physical address, outside the IOMMU, is never put on a test machine:
`iommu-testdev` verifies the word it wrote by a direct read of physical memory, and its BAR lies in the admitted PCI window, so the
holder of the root resource could use it to probe physical memory, the kernel's included. `scripts/os_boot_test.py` calls `refuse`
on every command line before it starts QEMU. The self-test starts no emulator (the feature gate runs it).
"""

import os
import sys

REFUSED_DEVICES = ("iommu-testdev",)


def refused(cmd):
    """The arguments of `cmd` that name a device of REFUSED_DEVICES (anywhere: a `-device` option, the machine's options, a
    `-global`, the extra arguments a caller adds), as (argument, device) pairs."""
    return [(a, d) for a in cmd for d in REFUSED_DEVICES if d in str(a)]


def refuse(cmd):
    """Stop the run (SystemExit) when `cmd` names a device of REFUSED_DEVICES."""
    bad = refused(cmd)
    if bad:
        raise SystemExit(f"the command line names {', '.join(sorted({d for _, d in bad}))} ({', '.join(a for a, _ in bad)}): no test "
                         "machine carries a device that reads or reports memory at a physical address (IOMMU-SPEC amendment "
                         "2026-11-27 part 2, L7)")


def self_test() -> int:
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    import os_boot_test as h
    good = ["qemu-system-riscv64", "-machine", h.DEFAULT_MACHINE] + [a for slot, model, _, extra, _ in h.PCI_DEVICES
                                                                      for a in h.pci_device(slot, model, extra)]
    results = {
        "the harness's machine names no refused device": refused(good) == [],
        "a -device with properties is refused": refused(good + ["-device", "iommu-testdev,addr=0x8"]) != [],
        "a -device with none is refused": refused(good + ["-device", "iommu-testdev"]) != [],
        "a device named in another argument is refused": refused(good + ["-global", "iommu-testdev.x=1"]) != [],
        "the harness's PCI_DEVICES holds no refused device": not any(m in REFUSED_DEVICES for _, m, _, _, _ in h.PCI_DEVICES),
    }
    # the harness refuses before anything else: before it reads the kernel, and so before QEMU starts
    try:
        h.run("/nonexistent-kernel", extra=("-device", "iommu-testdev,addr=0x8"))
        results["the boot harness refuses the command line before QEMU starts"] = False
    except SystemExit as e:
        results["the boot harness refuses the command line before QEMU starts"] = "iommu-testdev" in str(e.code)
    except OSError:
        results["the boot harness refuses the command line before QEMU starts"] = False
    for what, ok in results.items():
        print(("ok: " if ok else "FAIL: ") + what)
    return 0 if all(results.values()) else 1


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        sys.exit(self_test())
    print(__doc__, file=sys.stderr)
    sys.exit(2)
