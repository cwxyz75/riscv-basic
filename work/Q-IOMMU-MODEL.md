---
id: Q-IOMMU-MODEL
kind: decision
title: The shape of DMA isolation: which IOMMU device, what a domain belongs to, fault policy, and the end of virtio-mmio
area: drivers
horizon: long
refs: [T42, IOMMU1, OD148, OD47, OD127]
---

OD148 settled that the IOMMU is required, that every DMA device goes behind it and that the drivers move to virtio-pci. These are
the choices it left open that only the owner can make. Each has a recommendation; "as recommended" is a complete answer.

1. **Which IOMMU does the profile require?** QEMU 11.1.3 offers the platform device (`-M virt,iommu-sys=on`: a `riscv,iommu` node
   in the device tree with fixed registers) and `riscv-iommu-pci` (a PCI function the kernel would have to find by walking PCI
   configuration space). Recommended: **the platform device**. The kernel then needs no PCI code at all (it reads one tree node,
   like the AIA), and the IOMMU's registers are not reachable through the ECAM window that `devmgr` will hold. Every QEMU command
   line of the OS track gains `iommu-sys=on`; the kernel refuses to boot without the node.
2. **What does a translation domain belong to?** OD148 says "per DMA-capable device or driver". Recommended: **one domain per
   device** (per requester id), a kernel object created by the holder of the root resource, at most one live per device. A driver
   that drives two devices holds two. A per-process domain (one I/O address space shared by all of a driver's devices) saves
   nothing here and lets one device reach another's buffers.
3. **Who maps?** Recommended: the **supervisor** (`devmgr`, `netsup`) holds the domain handle and maps the buffers; the driver is
   told the I/O addresses in its startup message and holds no domain handle and no way to learn or change a mapping. Alternative:
   the driver holds the domain and maps for itself (more flexible, more authority in the least trusted process).
4. **Who chooses I/O addresses?** Recommended: the kernel (lowest fit above a non-zero base, never an identity mapping, so a
   driver that uses a physical address by mistake faults at once). Alternative: the caller names the address, as `process_map` does.
5. **What happens when a device faults in the IOMMU?** Always: one structured console line (rate-limited) and counters. In
   addition, recommended: a signal on the domain object that its holder can wait for, with the fault record readable by the holder,
   and **no automatic kill** (the supervisor's restart policy decides). Alternative: the kernel detaches the device at the first
   fault.
6. **A device nobody has claimed, or whose domain has gone:** recommended: its DMA faults; a device whose domain was destroyed is
   left attached to a kernel-owned empty domain with fault reporting switched off (so a device left running by a dead driver cannot
   flood the fault queue), until it is claimed again. The kernel never resets a device (it knows no bus).
7. **virtio-mmio at the end of the epic:** recommended: `virtio,mmio` is removed from the kernel's device allowlist
   (`kcore::devwin::ALLOWED`), because the platform IOMMU does not cover those slots and "no pass-through" (OD148) then means no
   process can be given one. The OS track loses virtio-mmio entirely; the home computer keeps its own kernel and its mmio disk.
8. **A QEMU-only weakness to accept or not:** a virtio-pci device started **without** `iommu_platform=on` ignores the IOMMU (QEMU
   behaviour, to be confirmed in IOMMU0), and the kernel cannot see the difference. Recommended: accept, state it in the spec as a
   property of the machine (like `-cpu rva23s64`), have `devmgr` refuse a virtio device that does not offer
   `VIRTIO_F_ACCESS_PLATFORM`, and keep a negative test that proves each test machine's devices do fault.

Blocks IOMMU1. Source: T42 re-planning, 2026-11-25.

## Answer (owner, 2026-11-25)

Accepted as recommended, with 3 and 6 as revised:

1. The profile requires the platform IOMMU (`-M virt,iommu-sys=on`, a `riscv,iommu` node); the kernel refuses to boot without it.
2. One domain per device (requester id), a kernel object created by the holder of the root resource, at most one live per device. The spec states the rule for devices that share a requester id behind a bridge (they would share a domain); nothing is built for it.
3. **Revised: the rights are split.** The **driver** holds a domain handle with MAP and UNMAP rights: it maps VMOs it already holds with the `DMA` right into its own device's domain, learns its I/O addresses, and its job pays (Q-IOMMU-ABI 4). This adds no authority: the device can reach only memory the driver could already write. The **supervisor** (`devmgr`, `netsup`) holds the control rights: create and destroy the domain, grant interrupt vectors (Q-IOMMU-MSI 6), read fault records. No per-buffer IPC through the supervisor. Tickets that assumed supervisor-only maps (IOMMU1, IOMMU6, IOMMU8, IOMMU10, IOMMU11) are to be read with this split.
4. The kernel chooses I/O addresses: lowest fit above a non-zero base, never an identity mapping.
5. A fault gives a rate-limited console line, counters, and a signal on the domain with a record the supervisor can read; no automatic kill.
6. **Revised:** an unclaimed device's DMA faults; a device whose domain was destroyed sits on a kernel-owned empty domain with fault reporting off. The kernel never resets a device, but **`devmgr` clears bus-master enable and issues a function-level reset when a driver dies, before the domain is torn down.**
7. `virtio,mmio` leaves the kernel's allowlist at the end of the epic.
8. A virtio-pci device without `iommu_platform=on` bypasses the IOMMU in QEMU; the spec states this as a property of the machine, `devmgr` refuses a device that does not offer `VIRTIO_F_ACCESS_PLATFORM`, and a negative test proves each test machine's devices fault.

