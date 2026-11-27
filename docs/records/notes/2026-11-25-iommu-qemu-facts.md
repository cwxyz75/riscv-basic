# IOMMU facts: what QEMU 11.1.3's RISC-V IOMMU and `iommu-testdev` do (IOMMU0)

Record of 2026-11-25 for work item IOMMU0. Machine: `qemu-system-riscv64` 11.1.3, `-M virt,aia=aplic-imsic,iommu-sys=on -cpu rva23s64
-smp 1 -m 512M -bios default`, with `-device iommu-testdev`, `virtio-blk-pci` and `virtio-rng-pci` (`iommu_platform=on,disable-legacy=on`)
and one `virtio-rng-pci` without `iommu_platform`. Source read: the tagged tree `v11.1.3` of QEMU (`hw/riscv/riscv-iommu.c`,
`riscv-iommu-bits.h`, `riscv-iommu-pci.c`, `riscv-iommu-sys.c`, `virt.c`, `hw/misc/iommu-testdev.c`, `hw/virtio/virtio-pci.c`,
`tests/qtest/riscv-iommu-test.c`). Observations come from a throwaway S-mode guest (a single Rust file, not merged; kept outside the
repository) that drove the IOMMU registers, the testdev and the virtio devices directly, with no kernel in between.

Marks: **[source]** read in the QEMU source, **[observed]** seen on a run of the probe, **[not established]**. The ratified RISC-V IOMMU
and AIA specification texts were **not** read for this record (they were not available here); every answer that depends on what
*hardware* must do, rather than what QEMU does, says so.

## Corrections to the work item's earlier decode

`capabilities` = `0x0000_0078_e2cf_4f10` **[observed]**: `version` register reads `0x10` (1.0). Bit 14 is `SVRSW60T59B`, as the work item
already says. The remaining stage bits were not individually re-decoded; IOMMU2 must derive its constants from `riscv-iommu-bits.h`,
not from the work item's prose.

## 1. `iommu-testdev` [source, observed]

- BAR0 (32-bit, 4 KiB). Registers: `0x00` triggering (a **read** of it runs the DMA: `iommu_testdev_mmio_read` calls
  `iommu_testdev_maybe_run_dma`), `0x04`/`0x08` IOVA lo/hi, `0x0c` length, `0x10` result, `0x14` doorbell (write `1` to arm; one DMA per
  arm), `0x18` attributes, `0x1c`/`0x20` the physical address lo/hi used for the read-back.
- One command does a **write of the fixed value `0x12345678` (repeated, little-endian) to the IOVA through the IOMMU address space,
  then reads the same length back directly from the physical address `dma_paddr` (bypassing the IOMMU) and compares.** So it is a
  write-then-verify tester: it proves "the IOVA translates to this physical page", not a read path, and it **cannot read** through
  the IOMMU.
- Results [source, observed]: `0` pass; `0xdead0001` bad length; `0xdead0002` the IOMMU-side write failed (**the fault shows as this
  code; the fault record is in the fault queue**); `0xdead0003` read-back failed; `0xdead0004` mismatch; `0xdead0005` not armed;
  `0xdead0006` bad attributes; `0xffffffff` idle, `0xfffffffe` busy.
- Bus mastering: **not needed** [observed]: with the command register's bus-master bit clear, a mapped DMA still succeeded
  (`result=0`, buffer written). The testdev uses `pci_device_iommu_address_space` directly. With the IOMMU `Off` it fails
  (`0xdead0002`, fault cause 256) whatever the bit.
- Requester id: its PCI address, device id `0x08` at slot 1 (`did = bus<<8 | dev<<3 | fn`) [observed].
- **Consequence:** it is a usable independent oracle for IOMMU5/IOMMU6 (translation, permission and invalidation checks, with the
  fault queue as the evidence), but **not a forger** for interrupts or arbitrary reads: see 14.

## 2. virtio and the IOMMU [source, observed]

- A platform device (`iommu_platform=on`) offers feature bit 33 (`VIRTIO_F_ACCESS_PLATFORM`) [observed: `hi=0x103`]. A driver that does
  not accept it **cannot reach `FEATURES_OK`**: writing it leaves status `0x3`, the bit reads back clear [observed A0].
- A device **without** `iommu_platform` bypasses the IOMMU: with `ddtp=Off` it completed a request using a physical address
  [observed B]; no fault is recorded. So the kernel must not give out a device without `iommu_platform` (a driver that finds bit 33
  absent must refuse to bind: IOMMU8).
- Faults on a platform device: ring fetch with `ddtp=Off` records cause 256 (`ttype=2`, read) three times and the device does nothing
  (no completion, status stays `0xf`, no `NEEDS_RESET`) [observed A1]. With a context but nothing mapped: cause 21 (instruction/read
  page fault class: `ttype=2`) [observed A2]. Mapped: the request completes [observed A3].
- **Read-only mapping [observed A4, partly]:** the device never completed (`used.idx=0`) and **no fault record was seen**. The probe
  did not isolate why (the used-ring write needs a writable page, so the device stops there; the missing record is **not
  established**). IOMMU8's tests should map descriptors read-only and the used ring writable instead of relying on a fault here.

## 3. First stage or second stage for a plain device domain [source, observed]

- Second stage only worked as intended: `ddtp=1LVL`, extended context, `iohgatp` Sv39x4, `fsc`=Bare, `tc.GADE` clear. A DMA through a
  leaf with `U=0` **faults** (cause 23, store page fault, `iotval2` = the GPA; guest-page fault class, not 21) [observed]: a
  G-stage leaf **needs U=1**. A leaf with `A=0`/`D=0` faults with the same cause when `GADE` is clear (no hardware update) [observed].
- `Bare`/`Bare` (iohgatp Bare and fsc Bare) is pass-through [observed: T2 `ddtp=Bare` lets `iova==pa` through; for a context with both Bare the
  source translates identity, as for `ddtp=Bare`]: the kernel never writes it, as the spec says.
- **[not established from the ratified text]** which stage the specification intends for a non-virtualised device; QEMU accepts
  both. The spec's choice (second stage) is internally consistent with what was observed.

## 4. Formats [source, observed]

- DDT: with `MSI_FLAT/MSI_MRIF` set the device context is the 64-byte extended format [source]. Only the 1LVL layout was exercised; the device-id split of
  2LVL and 3LVL is **not established** here (IOMMU2 reads `RISCV_IOMMU_DDI_*` in `riscv-iommu-bits.h`).
- `ddtp` read-back after writing mode 1LVL with PPN `0x83000`: `0x20c00c02` (PPN `0x83000` in bits, mode 2 = 1LVL) [observed].
  Writing 2LVL gave `…03`.
- Command and fault record layouts, IOFENCE bits and queue sizing worked as the probe encoded them from the source: cqcsr/fqcsr read
  `0x10001` (enabled, active) after bring-up [observed]. A fault record carries `cause`, `ttype` (2 read, 3 write), `did`, `iotval`,
  `iotval2` [observed]. `IOFENCE.C` with AV wrote its completion word [observed].
- **Reset state:** `ddtp=0` (Off), `fctl=0` (MSI interrupts, not WSI), `icvec=0`, `ipsr=0` [observed].
- The platform device is created by the machine option; **no `-global` property gives a different reset `ddtp`** [source: the
  `off` property exists only on `riscv-iommu-pci`]. See 15.

## 5. MSI writes with no MSI page table [source, observed]

A device MSI write is translated like any DMA write. Under a context whose `msiptp` is off, writing the IMSIC page faults unless
the G-stage maps it: cause 23, `iotval` = the target address [observed M4]. With `msiptp` in basic mode and the target inside the
window, the write is redirected to the IMSIC of the entry's page (M1: eip2 bit set; M2: `data=5` raised eip0 bit 5 **even though
the identity was not "granted" to the device**: the IOMMU does not filter data words, the page choice is the only gate).

## 6. INTx on `virt` [observed]

The probes ran with MSI-X enabled and no INTx was seen. M14 (MSI-X disabled in the capability) shows the PCI status interrupt bit
change `0x10` -> `0x18` (the INTx path **does** fire when MSI-X is off) [observed]. The command register's interrupt-disable bit was
**not** tested for silencing it: **not established**. Q-IOMMU-IRQ chose MSI-X, so this only blocks a defensive "driver turns INTx off".

## 7. The DBG interface [source, observed]

`riscv_iommu_process_dbg` implements `tr_req_iova`/`tr_req_ctl`/`tr_response` [source]. The probe's T5 wrote `tr_req_ctl` and got
`response=0x20c00000` instead of the expected PPN `0x83000` [observed T5 DBG: `req_ctl=0x80000000000`]. The probe's request encoding
was not verified, so **the interface is present but its use is not established**; a kernel self-test should not rely on it until
IOMMU5 has run it against the register layout in the specification.

## 8. Timing and caching [source, observed]

- Commands are processed **synchronously on the tail write** (`riscv_iommu_process_cq_tail`) [source]; `IOFENCE.C` with AV wrote
  immediately [observed].
- The translation cache serves a **stale entry after the PTE is cleared** without invalidation [observed T4: `result=0`, buffer
  written]. `IOTINVAL.VMA` (no GV) did **not** flush it; `IOTINVAL.GVMA` (all) did [observed]: the second-stage cache is keyed by
  GSCID and removed only by the GVMA form. The spec's invalidation protocol must use `IOTINVAL.GVMA` for second-stage PTE changes.
- The device-context cache also serves a stale context after the context is rewritten (V=0) until `IODIR.INVAL_DDT`; afterwards cause
  258 (DDT entry not valid) [observed]. A context change needs IODIR **and**, for already-cached translations, the IOTINVAL.
- The negative control of IOMMU6 therefore works as planned.

## 9. Spec-derived vectors [source]

`tests/qtest/riscv-iommu-test.c`: GPL-2.0-or-later (Ventana, 2024). It tests register reset values, ddtp transitions and an
IOMMU PCI device's bring-up, **not** page-table walk vectors; it is therefore not a source of translation vectors. Because of the
licence, IOMMU2 must not copy it. Independent vectors for IOMMU2 are **not established**; the in-machine testdev (1, 8) is the
independent oracle for walks.

## 10. MSI page tables in the specification's words [not established]

The ratified text was not read. QEMU's behaviour (read and observed): `msiptp` works with `iohgatp` in paging mode and `iosatp`
Bare; the MSI address check is made per write, after the device-context load, against `(addr & ~mask) == pattern`
[source: `riscv_iommu_msi_check`]; entry layout: basic = V, M=3, the target page PPN; MRIF = V, M=1, MRIF address, notice PPN/NID
[source: `riscv_iommu_msi_write`]. Fault causes: 261 (MSI PTE load access fault), 262 (MSI PTE invalid), 263 (MSI PTE misconfigured),
264 (MRIF access fault), 273 [source, observed]. M5 (entry invalid) -> 262; M4 (address outside the window) is checked as an ordinary
write -> 23. **Hardware conformance of these is not established.**

## 11. MRIF semantics on hardware [not established]

QEMU, observed: M7 (enable clear): pending bit set, **no notice**; M8 (enable set): pending and notice delivered (eip2 bit 36); M9: data
5 with only 120 enabled: pending bit 5 set, **no notice**; M10 data 3000: fault 263 (data above 2047); M11 data 2047: accepted.
Pending bits are written for every message; the notice is sent only when the enable bit is set. QEMU does not update the file
atomically. Whether real hardware notifies on every enabled message or only on 0-to-1, or may cache a file, needs the AIA text.

## 12. Invalidating a cached MSI entry [source]

QEMU does **not** cache MSI translations [source], so no invalidation is observable here. From the source the commands that would
apply are `IOTINVAL.GVMA` with the file page's guest address; the simulated device of IOMMU2 must model a cache or the host test
cannot catch a missing flush. Hardware requirement: **not established**.

## 13. The machine [source, observed]

- Interrupt remapping is on for the platform device: capability `IGS` = both; `MSI_FLAT` and `MSI_MRIF` set [observed].
- A message with data above `num-ids - 1`: M3 (basic entry, data 3000) delivered **nothing and faulted nothing** [observed]. The
  spec's assumption "a wrong data word is dropped, not faulted" holds for basic entries; for **MRIF** entries data above 2047 is a
  **fault** (cause 263) [observed M10, TDMSI]. IOMMU-SPEC's wording needs to say which.
- Unaligned address (M12: window + 4096 + 2): QEMU raised no fault and delivered nothing [observed]; the source check suggests an
  alignment fault: unexplained, **not established**.
- Each virtio-pci device here: **MSI-X table size 2**, table in BAR1 at offset 0, pending-bit array in BAR1 at `0x800` [observed
  `table_size=2`]. `vectors` is a device property; the drivers must request what they need or use two vectors.
- Messages from virtio-pci go through the device's address space with its requester id (M1/M2 under `did 0x18`) [observed].
- A masked vector in the MSI-X table (M13) produced no MSI [observed].
- The stopped-hart behaviour of the IMSIC was **not** tested.

## 14. `iommu-testdev` and interrupts [source]

No. It writes the fixed value `0x12345678` to an IOVA; it has no MSI-X table and no way to choose a data word. **Its basic-mode
MSI check** passed by pointing the IOVA at the window page (TDMSI: the 32-bit write with data `0x12345678` reached the MSI path
at did 0 and did 8 without fault): useful but data-fixed, 4-byte only (a 1-byte write is rejected without a fault record). The
forger for IOMMU7's checks is **a virtio-pci device whose MSI-X table the test programs**, exactly as M1 to M14 did.

## 15. Turning an enabled IOMMU Off [source]

`riscv_iommu_process_ddtp` accepts `{OFF, BARE}` -> anything and `{1LVL, 2LVL, 3LVL}` -> `{OFF, BARE}` **directly** and
immediately: the busy bit is never set (it is cleared in the sanitised value) [source]; the probe wrote 1LVL -> Off directly
and read Off back (`T6`), with the command and fault queues left enabled (cqcsr/fqcsr stayed `0x10001`) [observed]. Queues are
switched off by writing cqcsr/fqcsr `0` and reading them clear (spins=0) [observed]. Direct 1LVL <-> 2LVL is **not** accepted by QEMU
(a change between two directory modes is ignored: the register keeps the old mode [observed T6: 2LVL -> 1LVL stayed 2LVL]); the
spec must go through Off. Hardware's wait for in-flight transactions is **not established**. QEMU cannot be made to hand the
platform device over not Off, so a boot check of IOMMU-SPEC 2.4 step 1 is impossible; the host test against the simulated device
is the only proof.

## 16. Function-level reset [observed]

The virtio-pci functions on the generic ECAM host expose only the MSI-X capability and the five virtio vendor capabilities
(types 1 to 5); **no PCI Express capability, hence no function-level reset** [observed capability walk]. The reset is therefore the
virtio status-0 reset alone, with bus mastering cleared by the kernel first. Bus mastering clear still allowed IOMMU-mapped DMA from the
testdev (1), but virtio stops at status 0 (the device state is reset by the virtio status write per virtio 1.x). IOMMU8 records this.

## What follows for the spec and the plan (for the owner; no spec was edited)

1. IOMMU-SPEC's "a wrong data word is dropped, not faulted": true for basic entries (silently ignored, both here and when a pending
   bit is set with enable clear), **false for MRIF entries above 2047** (fault 263). Needs wording in an amendment (needs your
   permission).
2. Second-stage invalidation: **`IOTINVAL.GVMA`**, not `.VMA`, is what flushes a second-stage-only context in QEMU (8).
3. `iommu-testdev` is a translation oracle, not a forger and cannot read (1, 14).
4. No FLR on these devices (16); virtio status 0 is the whole reset.
5. MSI-X table has two entries by default (13).
6. Direct directory-mode to directory-mode changes of `ddtp` are ignored (15).
7. Open, **not established**: 3 (spec's stage choice), 7 (DBG usage), 9 (independent walk vectors), 10 to 12 (hardware text), A4
   fault absence, M12, INTx silencing, stopped-hart IMSIC.

## Addendum 2026-11-26 (IOMMU7, observed)

These come from a kernel-only boot (`OS_SHARD=k`, one hart, `-cpu rva23s64`, virtio-rng-pci at 00:04.0 behind MRIF entries) with
`OS_SHOW='## iommu: fault'`. The fault lines of the four MSI suite tests:

- **Cause 262** (the entry not valid: a message to an ungranted page, and to a revoked vector's page) reports **`iotval` 0**:
  `cause=262 dev=00:04.0 iova=0x0 write`, twice. So both MSI causes this machine raises, 262 and 263, carry no address. The section 10
  remark about 263 holds for 262 too.
- **Cause 263** (data 4096 on a granted page): `iova=0x0`.
- **An unaligned address on a granted MRIF page** (window page + 2, data 1): **no fault**, nothing delivered, and no notice. This is
  M12 again, now through an MRIF entry. It is still unexplained against the source's alignment check.
- **A device write to the S-mode IMSIC page** (0x28000000): an ordinary translation fault (cause 23, `iova` the address). It is not an
  MSI fault, because the address is outside the window.


## Addendum 2026-11-26 (CI pins QEMU 11.1.3)

CI ran the boots on Ubuntu 26.04's QEMU, `1:10.2.1+ds-1ubuntu3.3`, whose `virt` machine with `iommu-sys=on` writes
`iommu-map = <0 ph 0 0>, <0 ph 0 0xffff>`: requester id 0xffff is outside the IOMMU, so the kernel (IOMMU-SPEC 2.2) kept the PCI windows
closed and the epic's IOMMU, MSI and device-capability tests failed. 11.1.0 is the first release with the single full entry; 11.1.3
writes `iommu-map = <0x0 0x8000 0x0 0x10000>` [observed, on a developer's machine and on CI]. By the owner's decision of 2026-11-26,
CI now builds 11.1.3, the version of this record, from the release tarball (sha256 and release-key signature checked) and caches it;
every job that boots asserts the version, and `qemu.yml` checks the `iommu-map` above (`docs/reference/TEST-TOOLS.md`, "CI's QEMU").
The bundled firmware is OpenSBI 1.8.1. A cold build on a hosted runner took 165 to 196 s (`riscv64-softmmu` only); restoring the cached
install (29 MB without the UEFI images) took about a second.
