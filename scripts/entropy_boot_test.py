#!/usr/bin/env python3
"""HD1 acceptance on QEMU (a soak target, `make os-boot-test-entropy`): the kernel's entropy pool is keyed differently at every boot, and a
boot whose device tree has no seed says so and still passes the kernel-only shard.

1. Two boots of the kernel-only shard on the default machine report different `entropy: ... first output` values.
2. QEMU's own tree is dumped (`-machine ...,dumpdtb=`), `/chosen/rng-seed` is removed from it, and a boot with `-dtb` of that tree reports
   `entropy: unseeded` and passes every test (the self-test `the_entropy_pool_is_seeded_as_the_tree_says` checks the pool is unseeded
   exactly when the tree has no seed).
3. In every one of those boots the random service's `rngd` seeds from the PCI entropy function behind the IOMMU (IOMMU11: the Kernel
   test `rng_seeds_through_the_iommu` prints `entropy: rngd seeded from the PCI device through the IOMMU`).

Usage: entropy_boot_test.py <kernel ELF>
"""
import os
import re
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
MACHINE = "virt,aia=aplic-imsic,iommu-sys=on"
RNGD_SEEDED = "entropy: rngd seeded from the PCI device through the IOMMU"
RAM = "256M"


def boot(kernel, extra_env):
    env = dict(os.environ, OS_SHARD="k", OS_SMP="1", OS_DISK="0", OS_SHOW="entropy:", **extra_env)
    r = subprocess.run([sys.executable, os.path.join(HERE, "os_boot_test.py"), kernel], env=env, capture_output=True, text=True)
    return r.returncode, r.stdout + r.stderr


def stripped_dtb(tmp):
    raw = os.path.join(tmp, "qemu.dtb")
    subprocess.run(["qemu-system-riscv64", "-machine", f"{MACHINE},dumpdtb={raw}", "-cpu", "rva23s64", "-smp", "1", "-m", RAM,
                    "-display", "none", "-bios", "default"], check=True, capture_output=True)
    dts = subprocess.run(["dtc", "-I", "dtb", "-O", "dts", raw], check=True, capture_output=True, text=True).stdout
    stripped, n = re.subn(r"\s*rng-seed = <[^>]*>;", "", dts)
    if n != 1:
        sys.exit(f"expected exactly one rng-seed in QEMU's tree, found {n}")
    out = os.path.join(tmp, "stripped.dtb")
    subprocess.run(["dtc", "-I", "dts", "-O", "dtb", "-o", out], input=stripped, check=True, text=True, capture_output=True)
    return out


def main():
    kernel = sys.argv[1]
    outs = []
    for i in (1, 2):
        rc, text = boot(kernel, {})
        m = re.search(r"entropy: seeded .*first output (0x[0-9a-f]+)", text)
        if rc != 0 or not m:
            sys.exit(f"boot {i} failed (status {rc}) or printed no seeded entropy line:\n{text[-2000:]}")
        if RNGD_SEEDED not in text:
            sys.exit(f"boot {i}: rngd did not seed from the PCI entropy function:\n{text[-2000:]}")
        outs.append(m.group(1))
    if outs[0] == outs[1]:
        sys.exit(f"two boots produced the same first output {outs[0]}")
    print(f"entropy: two boots differ ({outs[0]} and {outs[1]})")
    with tempfile.TemporaryDirectory() as tmp:
        rc, text = boot(kernel, {"OS_DTB": stripped_dtb(tmp)})
    if rc != 0 or "entropy: unseeded" not in text or RNGD_SEEDED not in text:
        sys.exit(f"the boot without a seed failed (status {rc}) or did not report unseeded:\n{text[-2000:]}")
    print("entropy: a tree without a seed boots unseeded and passes")


if __name__ == "__main__":
    main()
