#!/usr/bin/env python3
"""CI's QEMU, the fact the kernel depends on (TEST-TOOLS.md, "CI's QEMU"): the device tree of `virt,iommu-sys=on` maps every PCI
requester id through the IOMMU in one `iommu-map` entry `<0 phandle 0 0x10000>`. QEMU 10.2 wrote two entries that left id 0xffff
outside the IOMMU, and the kernel then (rightly, IOMMU-SPEC 2.2) kept the PCI windows closed. Usage: check-iommu-map.py <dtb>."""
import struct
import sys


def props(blob):
    magic, _total, off_struct, off_strings = struct.unpack_from(">IIII", blob, 0)
    if magic != 0xD00DFEED:
        sys.exit(f"{sys.argv[1]}: not a flattened device tree")
    i = off_struct
    while True:
        (tok,) = struct.unpack_from(">I", blob, i)
        i += 4
        if tok == 1:  # FDT_BEGIN_NODE: a NUL-terminated name, padded to 4
            end = blob.index(b"\0", i)
            i = (end + 4) & ~3
        elif tok == 3:  # FDT_PROP: length, name offset, value padded to 4
            length, nameoff = struct.unpack_from(">II", blob, i)
            i += 8
            name = blob[off_strings + nameoff:blob.index(b"\0", off_strings + nameoff)].decode()
            yield name, blob[i:i + length]
            i = (i + length + 3) & ~3
        elif tok in (2, 4):  # FDT_END_NODE, FDT_NOP
            continue
        elif tok == 9:  # FDT_END
            return
        else:
            sys.exit(f"{sys.argv[1]}: bad token {tok} at {i - 4}")


maps = [v for n, v in props(open(sys.argv[1], "rb").read()) if n == "iommu-map"]
if len(maps) != 1:
    sys.exit(f"::error::expected one iommu-map property, found {len(maps)}")
cells = struct.unpack(f">{len(maps[0]) // 4}I", maps[0])
print("iommu-map = <" + " ".join(f"{c:#x}" for c in cells) + ">")
if len(cells) != 4 or cells[0] != 0 or cells[2] != 0 or cells[3] != 0x10000:
    sys.exit("::error::iommu-map is not the single full entry <0 phandle 0 0x10000>: this QEMU leaves requester ids outside the IOMMU")
