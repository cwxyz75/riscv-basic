---
id: IOMMU0
kind: slice
title: IOMMU facts: what QEMU's RISC-V IOMMU and iommu-testdev really do (a record, before the spec)
area: drivers
horizon: long
complexity: M
change_risk: L
refs: [T42, OD148, OD166, IOMMU-SPEC, TESTING]
---

Child 0 of the IOMMU epic (`work/T42.md`). A fact-finding step whose only merged output is a record under `docs/records/notes/`
(documentation only: straight to `main`). It exists because the spec (IOMMU1) and the acceptance checks (IOMMU5, IOMMU6) lean on
device behaviour that the planning pass of 2026-11-25 could not verify without running guest code.

## Plan (2026-11-25)

### What is already verified (2026-11-25, QEMU 11.1.3 on this machine, no guest code run)

- `qemu-system-riscv64 -device help` lists `riscv-iommu-pci`, `virtio-iommu-pci` and `iommu-testdev`; there is no user-creatable
  `riscv-iommu-sys` (`-device riscv-iommu-sys,help`: not found). The platform IOMMU is the machine option `-M virt,iommu-sys=on`.
- `-M virt,aia=aplic-imsic,iommu-sys=on,dumpdtb=...`: the tree gets `iommu@3010000` (`compatible = "riscv,iommu"`, `reg` one page,
  `#iommu-cells = <1>`, four wired interrupts 36 to 39 on the supervisor APLIC, `msi-parent` the supervisor IMSIC). The PCI host
  (`pci-host-ecam-generic`, ECAM at `0x3000_0000`) gets `iommu-map = <0 &iommu 0 0x10000>` (device id = requester id, all of bus 0 to
  255). **The `virtio_mmio@...` nodes get no `iommus` property**: the platform IOMMU does not cover virtio-mmio. That settles OD148's
  open check as far as the tree goes: the bus move is needed.
- A paused machine (`-S`) with `iommu-sys=on`, `-device virtio-blk-pci,iommu_platform=on,disable-legacy=on`,
  `-device virtio-rng-pci,iommu_platform=on,disable-legacy=on` and `-device iommu-testdev` is accepted. `info pci`: host bridge at
  00.0, the devices at 01.0, 02.0, 03.0; **every BAR is "not mapped"** (nobody has assigned addresses: there is no firmware PCI
  setup), virtio-pci has BAR1 (32-bit, 4 KiB: the MSI-X table) and BAR4 (64-bit prefetchable, 16 KiB: the modern registers),
  `iommu-testdev` has BAR0 (32-bit, 4 KiB). No default network card appears with `-display none`.
  `iommu_platform=on` is refused on a transitional device ("supported by neither legacy nor transitional"): `disable-legacy=on` is
  required.
- Monitor read of the IOMMU registers at reset: `capabilities` (offset 0) = `0x0000_0078_e2cf_4f10`, `fctl` (8) = 0, `ddtp` (0x10) =
  0. Decoded **from memory of the RISC-V IOMMU 1.0 register layout, to be re-checked against the text**: version 1.0; first-stage
  Sv32, Sv39, Sv48, Sv57; second-stage Sv32x4 to Sv57x4; MSI_FLAT and MSI_MRIF (so device contexts are the extended, 64-byte
  format); AMO_HWAD **clear** (the IOMMU does not set A and D: a PTE must be written with both set); ATS set (the default
  `ats=on`); IGS = both wired and MSI; HPM; **DBG set** (the debug translation interface, a possible in-machine oracle); PAS = 56;
  PD8 only; bit 14 set, meaning not known. `ddtp` mode 0 is Off: all DMA is refused until the kernel programs it. `fctl.WSI` = 0:
  the IOMMU's own interrupts are MSIs until the kernel sets WSI.
- **Added 2026-11-25 for the MSI-X plan (Q-IOMMU-IRQ answered B), read in QEMU's source at tag `v11.1.3`** (`hw/riscv/riscv-iommu.c`,
  `riscv-iommu-bits.h`) **and in the dumped tree**: the capability bits are `MSI_FLAT` 22, `MSI_MRIF` 23, `AMO_HWAD` 24, `ATS` 25,
  `IGS` 29:28, `HPM` 30, `DBG` 31, `PAS` 37:32, `PD8` 38, and bit 14 is `SVRSW60T59B`: so the value above has `MSI_FLAT` and
  `MSI_MRIF` set and `IGS` = both (the from-memory decode was right on those). An MSI page-table entry in basic-translate mode
  forwards the device's data word unchanged; MRIF mode is implemented with the notice message; the MSI check is made on writes,
  before translation only when the first stage is Bare; MSI translations are not cached; fault causes 261 to 264 and 273. The
  supervisor IMSIC has `riscv,num-ids = <0xff>`; the PCI host and the IOMMU node have `msi-parent` the supervisor IMSIC;
  `-M virt,aia-guests=N` takes 0 to 7. Detail and what follows from it: IOMMU7, "The isolation design".
- `riscv-iommu-pci,help`: `ats`, `g-stage`, `s-stage`, `intremap`, `off` (default on), `ioatc-limit` (default 1048576: there is a
  translation cache, so a skipped invalidation can be observed), `hpm-counters`, `pas-bits`.

### To establish (read QEMU's source for 11.1.3 and the ratified RISC-V IOMMU specification; one targeted experiment where reading does not settle it)

1. **`iommu-testdev`**: its register map (how a DMA is commanded: address, length, direction, result), whether it can read and
   write, what it reports for a faulted access, whether it needs bus mastering enabled, which requester id it uses. This decides
   whether it is the independent "hostile device" of the acceptance checks or whether the checks use a virtio device with a
   descriptor outside its mapping (the fallback).
2. **virtio and the IOMMU**: confirm in the source that with `iommu_platform=on` a driver that does not accept
   `VIRTIO_F_ACCESS_PLATFORM` (bit 33) cannot reach `FEATURES_OK` (from memory: `virtio_validate_features` refuses), and that with
   `iommu_platform=off` a virtio-pci device **bypasses** the IOMMU whatever the kernel does. What a virtio device does on a faulted
   descriptor (device error, `NEEDS_RESET`).
3. **First-stage or second-stage for a plain device domain**: which of `fsc` (iosatp) and `iohgatp` Linux and the specification
   intend for a non-virtualised device, the U-bit rule for first-stage leaves, and what `Bare`/`Bare` means (pass-through: the
   state the kernel must never write).
4. **Formats**: DDT levels and the device-id split for the extended format, the device context fields, command encodings
   (`IOTINVAL.VMA`, `IODIR.INVAL_DDT`, `IOFENCE.C` with its PR, PW, AV and WSI bits), the fault record layout and cause numbers,
   queue size encoding, `ipsr`/`icvec`/`fctl`, the reset state of `ddtp` under the `off` property, and whether the machine option
   exposes those properties (`-global riscv-iommu-device.off=...` or similar).
5. **MSIs with no MSI page table**: what happens to a device's MSI write under a domain whose `msiptp` is off (from memory:
   translated like any write, so it faults unless the IMSIC page is mapped). What `msiptp` restricts is settled for QEMU (read in
   source, above): a page in basic-translate mode, a kernel-chosen notice in MRIF mode.
6. **INTx on `virt`** (no longer used: Q-IOMMU-IRQ answered B): confirm that a virtio-pci device with MSI-X enabled never raises
   INTx, and that setting the command register's interrupt-disable bit silences it when MSI-X is off, so APLIC sources 32 to 35
   stay quiet whatever a driver does.
7. **The DBG interface**: whether `tr_req_iova`/`tr_response` work in QEMU, so a kernel self-test can ask the IOMMU itself what an
   IOVA of a device translates to.
8. **Timing and caching**: whether command processing is synchronous on the tail write, and whether the translation cache really
   serves a stale entry after a PTE is cleared without `IOTINVAL` (the negative control of IOMMU6 depends on it).
9. A copy of the specification's worked examples or QEMU's qtest vectors (`tests/qtest/riscv-iommu-test.c`) that IOMMU2 can use as
   **spec-derived vectors**, with their licence noted.

10. **MSI page tables in the specification's own words** (IOMMU7's design leans on QEMU's reading): may `msiptp` be used with
    `iohgatp` in a paging mode and `iosatp` Bare for a device that is not a guest's; where the MSI address check sits in the
    translation process; the exact layout of a basic and an MRIF entry and of the mask and pattern; the fault causes. This also
    answers item 3 (first or second stage) for good: the plan recommends second stage only.
11. **MRIF semantics on real hardware** (AIA specification, the chapter on IOMMU support): is a notice sent for every message whose
    enable bit is set, or only on a pending bit's 0-to-1 change; must the IOMMU update the file atomically (QEMU does not); may an
    IOMMU cache a file's contents or an entry's `NID` and `NPPN`.
12. **Invalidating a cached MSI entry**: which command (from memory: `IOTINVAL.GVMA` with the guest address of the file page) and
    whether `IOFENCE.C` orders it; QEMU caches none, so say what a real device needs and what the simulated device must model.
13. **The machine**: that the platform device (`iommu-sys=on`) has interrupt remapping on (the capability bits say so; find the
    property and its default in `hw/riscv/virt.c` and `riscv-iommu-sys.c`); the IMSIC's behaviour for an identity above `num-ids`
    and for a write to a file of a hart that is stopped; how many MSI-X table entries each virtio-pci device has by default
    (`vectors`) and which BAR holds the table and the pending bits; that virtio-pci's messages go through the bus-master address
    space with the requester id (the comment above `riscv_iommu_trap_write` says device code must supply it).
14. **`iommu-testdev` and interrupts**: whether it can send a message to a chosen address with a chosen data word (the ideal
    independent forger for IOMMU7's checks); if not, a virtio-pci device whose MSI-X table the test programs is the forger.
15. **Turning an enabled IOMMU Off** (added 2026-11-25 for OD166 point 11; IOMMU-SPEC 2.4 step 1 and assumption A17): can
    `ddtp` change **directly** from an enabled mode (a directory mode, or Bare) to Off, and what must the kernel wait for
    before it goes on (the register's busy bit, transactions already in flight, a command or fault queue still enabled and
    how each is switched off)? The kernel does exactly this when an IOMMU is handed over in a mode other than Off, then reads
    Off back and runs the normal bring-up; it refuses to boot only if the IOMMU will not go Off. Answer from the
    specification's text on `ddtp` and on the queue control registers and from QEMU's source; and say whether QEMU can be made
    to hand the platform device over not Off (item 4's properties), so that IOMMU5 knows whether a boot check is possible or
    the host test against the simulated device is the only proof.
16. **Function-level reset** (added 2026-11-25; IOMMU-SPEC 10.1 and 10.2 have `devmgr` issue one where the function advertises it, after the virtio status-0 reset): do QEMU's
    virtio-pci functions on `virt` (`disable-legacy=on`, on the generic ECAM host) offer a function-level reset (the PCI
    Express capability's bit, or the Advanced Features capability), what it resets (bus mastering, MSI-X enable, the virtio
    state), and how long the driver of the reset must wait. If they offer none, say so: the spec's reset is then the virtio status-0 reset
    alone (bus mastering already cleared), and IOMMU8 records that.

### Deliverable and acceptance

`docs/records/notes/2026-..-iommu-qemu-facts.md`: one section per question, each answer marked **read in source** (file and
function), **observed** (the command and its output) or **not established**. Acceptance: every one of the sixteen has an answer or a
plain "not established, and here is what that blocks"; `make docs-check` passes. No kernel code is merged. An experiment that needs
guest code uses a scratch branch and one targeted boot (`make kquick` with the machine option added by hand); the branch is not
merged.

### Hazards

- Reading QEMU's source needs the source of the installed version (Homebrew ships none): fetch the 11.1.3 tarball or read the
  tagged tree; do not answer from memory of an older version (the RISC-V IOMMU landed in 9.2 and changed since).
- The decode of `capabilities` above was from memory; the bits named in the 2026-11-25 addition were re-checked against QEMU's
  header, the rest (the version field, the stage bits) were not: correct it in the record before IOMMU2 turns it into constants.

PR size: one records file, small. Review: default.
