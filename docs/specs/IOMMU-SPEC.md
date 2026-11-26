# IOMMU-SPEC: DMA isolation and interrupt isolation for devices (specification, v1)

Version: v1, written 2026-11-25 from OD148 and the owner's answers of 2026-11-25 to Q-IOMMU-ABI, Q-IOMMU-IRQ, Q-IOMMU-MODEL and
Q-IOMMU-MSI (recorded as OD162 to OD165; each answer is cited here by the name of its question and its number), and from the
owner's decisions of the same day on the further choices of section 16 (OD166). This file is the acceptance contract
for the IOMMU work (CLAUDE.md: spec, then acceptance checks, then code, then independent review). It describes the end state
only. It amends `KERNEL-V1-SPEC.md`, `OS-ARCHITECTURE.md`, `S6-SPEC.md`, `NET-DESIGN.md`, `NET-SPEC.md` and `SCHED-SMP-SPEC.md`
by a dated amendment at the end of each.

How to read it. "The IOMMU specification" is the ratified RISC-V IOMMU Architecture Specification, version 1.0; "the AIA
specification" is the RISC-V Advanced Interrupt Architecture. They are cited by the title of the part, not restated: this
document says which options the system uses. A statement about what the device does that nobody has checked against those
texts or in the machine carries a label **(A1)** to **(A17)**; section 15 lists each, says what the design needs from it and
names the boot check or the test that settles it. Nothing labelled so is relied on silently. Section 16 lists the choices this
document needed beyond OD148 and the four answers, as the owner decided them.

Words. A **device id** is the number the IOMMU identifies a device by: for a PCI function, its requester id (bus, device,
function: 16 bits) through the PCI host's `iommu-map`. A **domain** is the kernel object that holds one device's I/O address
space and its interrupt vectors. An **I/O address** is an address as the device issues it. A **supervisor** is the process
that claims a device and controls its domain (`devmgr`, `netsup`); a **driver** is the process that programs the device.
An **interrupt identity** is a number in one hart's IMSIC interrupt file. A **fence** is the `IOFENCE.C` command.

---

## 1. Goal and threat model

**Memory.** A device reaches exactly the frames mapped into its domain, with the access (read, or read and write) that was
granted, and nothing else: not kernel memory, not another process's memory, not another device's buffers or registers, not an
interrupt controller. A frame is never returned to the allocator while any device can still reach it, whoever dies and in
whatever order (closed by construction: section 10).

**Interrupts.** A device causes exactly the interrupts of the vectors granted on its domain, and no other: not another
device's, not the inter-hart interrupt, not one of the kernel's own sources.

**The adversary** is a driver process that fully controls its device's registers, including the device's MSI-X table where
the device lets it, and that may be killed or may misbehave at any instant. A supervisor is trusted with *control* of the
devices it claims (it can stop them, reset them and deny them service) but gains no reach through a device either: the
kernel's rules below hold against a supervisor too, since it holds nothing but handles.

**Out of scope, named:**

* a hostile *device* that lies about its requester id, or hardware between the device and the IOMMU that lets a transaction
  bypass it (on a real bus: peer-to-peer routing below a bridge);
* denial of service by a driver against its own device, or by a supervisor against the devices it controls;
* side channels (timing of faults, of interrupts, of the shared command queue);
* what devices did before the kernel ran, on a machine that hands the IOMMU over in a state other than Off (section 2.4 turns
  it Off before anything else; what was done before that cannot be undone).

The honest limits of what is proven are in section 15: the properties of QEMU's emulation the isolation rests on, and the
assumptions nobody has verified.

---

## 2. Platform requirement and boot

### 2.1 The required device (Q-IOMMU-MODEL 1, OD148, OD47, OD127)

The profile requires a **platform RISC-V IOMMU**: a device-tree node with `compatible = "riscv,iommu"` and fixed registers
(on QEMU `virt`: `-M virt,aia=aplic-imsic,iommu-sys=on`, node `iommu@3010000`). The kernel contains no PCI code; an IOMMU that
is itself a PCI function (`riscv-iommu-pci`) does not satisfy the requirement. There is no fallback and no mode of the kernel
without an IOMMU.

### 2.2 Discovery

Everything comes from the device tree (KERNEL-V1-SPEC 2.3), read by hostile-input-safe code that is host-tested against the
real tree:

| Fact | Source |
|---|---|
| The IOMMU's register range | the `riscv,iommu` node's `reg` |
| The IOMMU's own interrupt sources | that node's `interrupts` (wired, on the supervisor-domain APLIC; four on `virt`) |
| Which device ids exist and that they are behind this IOMMU | the PCI host's `iommu-map` (on `virt`: every requester id of buses 0 to 255, device id = requester id) |
| How many interrupt identities a hart's file has | the supervisor IMSIC's `riscv,num-ids` |
| Each hart's interrupt-file page | the supervisor IMSIC node (as KERNEL-V1-SPEC 2.3) |

Exactly one `riscv,iommu` node is supported. The register page is a range the kernel drives: it is cut from every device
window (as the console and the interrupt controllers are), so no process can map it.

### 2.3 Capabilities the kernel insists on

Read from `capabilities` at boot and checked before anything is written. Each is required; the absence of any is a boot
refusal (2.5).

| Requirement | Why |
|---|---|
| Specification version 1.0 | the formats this system encodes |
| Second-stage translation in Sv39x4 | the translation mode of every domain (section 3.2) |
| `MSI_FLAT` | the extended (64-byte) device context with a flat MSI page table (section 8.2) |
| `MSI_MRIF` | identity isolation of interrupts (Q-IOMMU-MSI 1; section 8.2) |
| Wired interrupt generation (`IGS` includes wired) | the IOMMU's own interrupts are wired (Q-IOMMU-IRQ 3) |
| A physical address width (`PAS`) that covers every RAM address of the tree | the tables and every mapped frame are addressable |
| Little-endian operation, and `fctl` reading back as written | the kernel writes `fctl` with wired interrupts on and big-endian off |
| A command queue of 256 entries and a fault queue of 128 records (one frame each) | sizes written and read back |
| A device directory that covers every device id of the `iommu-map` in the extended format | the mode written to `ddtp` is read back (A4 gives the level count) |

Capabilities the device may have and the kernel never uses are in section 14 (ATS, PRI, process contexts, first-stage tables,
hardware A and D updates, guest interrupt files, the performance monitor).

### 2.4 Bring-up order, and the states the kernel never writes

1. Read `ddtp`. The expected hand-over state is **Off** (every device transaction refused; on the reference machine `ddtp`
   reads 0 at reset: observed). If it is anything else (a directory mode, or Bare, left by firmware or an earlier kernel),
   the kernel **writes Off**, which denies every transaction, waits for the change to take effect as the IOMMU specification
   requires (A17; the wait is bounded by `IOMMU_FENCE_BOUND`, section 6.4), and reads Off back; a command queue or a fault
   queue found enabled is disabled and read back the same way. Then the bring-up continues as on a machine handed over Off.
   Only an IOMMU that will not go Off is a boot refusal (2.5). Writing Off is not a pass-through state (IO2): it is the
   strongest deny the device has.
2. Check the capabilities (2.3).
3. Allocate the directory root, the command queue and the fault queue from boot-time frames, never freed.
4. Write `fctl` (wired interrupts), register the IOMMU's fault and command sources as kernel-owned interrupt sources (8.6),
   enable the command queue and the fault queue, and read each enable back.
5. Write `ddtp` with the directory mode and an **empty** directory, **once**, and read it back.

From step 5 on, every device id is denied, for memory and for messages alike (IO1). Device windows (2.6) are built only
after step 5: the kernel asserts the order.

**The kernel never writes a pass-through state** (IO2): not `ddtp` in the Bare mode; not a device context in which both
stages are Bare; not an MSI page-table entry in basic-translate mode (IO11). The only values the kernel ever writes to `ddtp`
are Off (step 1, and only when the hand-over state was not Off) and the directory mode of step 5; `ddtp` is not written again
after step 5 while the kernel runs; after a resume from a suspend that lost the IOMMU's state, steps 1 to 5 run again, with the same directory,
before any process runs. The code that encodes these structures has no constructor for a forbidden state; a negative-control
kernel that writes one exists only as a test fixture (section 13).

### 2.5 Boot refusals

Each is a panic before user mode exists, with a fixed first line on the console that a script can match:

| Condition | Line contains |
|---|---|
| No `riscv,iommu` node, or more than one | `no IOMMU` |
| The hand-over mode was not Off and the IOMMU does not read back Off within the bound after the kernel wrote Off (2.4 step 1) | `IOMMU: will not go off` |
| `MSI_MRIF` missing | `IOMMU: no MRIF` |
| Any other row of 2.3 missing | `IOMMU: missing <name>`, naming the first missing capability |
| A register does not read back what was written (`fctl`, a queue enable, `ddtp`) | `IOMMU: refused <register>` |
| The tree names a physical address at or above the base of the I/O window (3.3) | `IOMMU: address map too high` |

A boot that passes prints `kernel: iommu version=<major>.<minor> mode=<directory mode read back> caps=0x<capabilities>`,
with ` handover=<mode found>` appended when the hand-over state was not Off.
The boot scripts check the mode on that line, because Off also denies everything and a test of denial alone could not tell
a programmed IOMMU from one left off.

### 2.6 Which devices are DMA-capable, and what a device window may be

A device is **DMA-capable** if it can issue memory transactions of its own: every PCI function, and every platform device
with a DMA interface. The rule: **a DMA-capable device is reachable from user mode only if the device tree puts it behind
the IOMMU** (the PCI host's `iommu-map`, or an `iommus` property).

* The PCI host (`pci-host-ecam-generic`) is behind it: its configuration window (ECAM) and the memory windows of its
  `ranges` are device windows for the holder of the root resource. Its I/O-port window is not.
* `virtio,mmio` slots are **not** behind the platform IOMMU (the tree gives them no `iommus` property: observed) and are
  therefore never a device window (Q-IOMMU-MODEL 7). The OS track has no virtio-mmio driver. The home computer keeps its own
  kernel and its mmio disk and is not affected.
* fw-cfg (whose DMA interface writes guest RAM) and the flash banks are never a device window.
* Devices with no DMA ability (the RTC) are admitted as before.

The list of admitted devices stays an allowlist that fails closed (KERNEL-V1-SPEC, amendment 2026-11-20).

---

## 3. Address-space model

### 3.1 The states of a device (translation contexts)

Every device id is in exactly one of three states. The state is the device's entry in the device directory.

| State | Directory entry | Memory access | Message (MSI) | Fault reporting |
|---|---|---|---|---|
| **Unclaimed** (never had a domain) | none (invalid) | refused | refused | reported (section 7) |
| **Attached** to a live domain | valid, translating through the domain's tables | as mapped | as granted | reported |
| **Quarantined** (its domain was destroyed) | valid, translating through a kernel-owned **empty** table shared by all such devices, MSI page table off | refused | refused | **off** (Q-IOMMU-MODEL 6) |

Default deny (IO1): a device id with no live domain reaches nothing. Quarantine exists so that a device left running by a
dead driver cannot flood the fault queue; the device stays quarantined until a new domain is created for its id. The kernel
never resets a device: it knows no bus.

Transitions and how they are written (the encoding is the IOMMU specification's, "Device-context fields"):

* Unclaimed to Attached, Attached to Quarantined, Quarantined to Attached. There is **no** direct change from one domain's
  tables to another's: one side of every write is a deny state.
* A context is 64 bytes and is not written atomically. Every transition is safe under a torn read: the IOMMU may combine
  fields of the old and the new context, and every such combination reaches only tables that are valid and owned by one of
  the two states at that moment (tables are freed only after the fence that follows: section 6.3). Going from invalid to
  valid, the word that holds the valid bit is written last.
* Every transition is followed by a device-directory invalidation for that device id, an invalidation of the translations of
  the address space left behind, and a fence, in that order, before the call that caused it returns.

### 3.2 A domain

A **domain** is a kernel object: one device's I/O address space, its mappings, its MSI page table and its vectors.

* **One live domain per device id** (IO5; Q-IOMMU-MODEL 2). A second `iodomain_create` for an id whose domain is live is
  BAD_STATE. A driver that drives two devices holds two domains. Devices that share a requester id behind a bridge are one
  device id and therefore one domain: they are claimed, isolated and torn down together, and a supervisor must treat them as
  one unit. (On the reference machine every function is on bus 0 with an id of its own; bridges are a non-goal, section 14.)
* **Created only with the root resource** (invariant D3 extended to domains).
* **Translation: second stage only.** The device context has the first stage Bare and the second stage (`iohgatp`) in
  Sv39x4, with a guest soft-context id the kernel allocates per domain and frees only after the domain's last fence (A2).
  The reason is the MSI check: with the first stage Bare the IOMMU tests a device's write against the MSI address window
  *before* any page table, on the address the device itself issued, so the interrupt-file addresses are never in the DMA page
  table (A2; IOMMU specification, "Process to translate addresses of MSIs").
* **4 KiB pages only.** No superpage leaf is ever written (superpage I/O mappings are a non-goal: section 14).
* **Leaves** are written with the accessed and dirty bits set and hardware A and D updates off in the context, on every
  machine (QEMU's device does not update them); the remaining leaf bits are as the IOMMU specification requires for the stage
  (A3).
* The context enables nothing else: no ATS, no page requests, no process directory (section 14).

### 3.3 The I/O address space of a domain

An I/O address is 41 bits wide (Sv39x4). Every domain has the same layout:

| Range | Use |
|---|---|
| `0` to `IO_BASE - 1` | never mapped. Page 0 is never mapped; no physical address of the machine is ever a valid I/O address |
| `IO_BASE` to `IO_BASE + 256 pages - 1` | the **interrupt window**: page `k` is the device's vector `k` (section 8.2). Synthetic addresses: not the IMSIC's |
| the rest of the window, `IO_WINDOW_PAGES` pages in all | DMA mappings, allocated by the kernel (section 4.6) |
| everything above | never mapped |

`IO_BASE` is 2^40 and `IO_WINDOW_PAGES` is 2^18 (1 GiB), constants of the ABI. The kernel refuses to boot on a machine whose
device tree names any physical address (RAM, reserved memory, a `reg` or a `ranges` window) at or above `IO_BASE` (2.5), so
**no I/O address ever equals a physical address** of RAM or of a device: a driver that programs a physical address by mistake
faults at once, and a transaction to an I/O address can never be claimed by another device's BAR.

Only frames of DMA VMOs are ever in a domain's page table: never a device VMO, never an interrupt controller's page, never a
kernel frame.

---

## 4. Mapping: the calls

The style is `SYSCALLS.md`'s; these paragraphs are the rows that document carries. Errors are checked in the order
listed. All handles are resolved before any state changes. New names are in the system library as `sys_<name>`.

### 4.1 Rights and signals

New rights (bits of the handle rights mask), defined for a domain handle:

| Right | Bit | Allows |
|---|---|---|
| IO_MAP | 14 | `iodomain_map` |
| IO_UNMAP | 15 | `iodomain_unmap` |
| IO_GRANT | 16 | `iodomain_grant_vector` (section 8.3) |
| IO_DESTROY | 17 | `iodomain_destroy` |

A domain handle may also carry DUPLICATE, TRANSFER, WAIT (wait for `DEVICE_FAULT`) and INSPECT (`iodomain_fault_read`).
`iodomain_create` returns all of them. None is `MANAGE` (OD159).

**The split of authority** (Q-IOMMU-MODEL 3): the **supervisor** keeps the handle from `iodomain_create` (control: destroy,
grant, faults). The **driver** is given a duplicate with **IO_MAP and IO_UNMAP only**: it maps VMOs it already holds with
the `DMA` right into its own device's domain and learns their I/O addresses, with no per-buffer message to its supervisor.
This adds no authority to the driver: the device can reach only memory the driver could already read or write.

The right **DMA** (bit 12) on a VMO handle means **"this VMO may be mapped into a device domain"** (Q-IOMMU-ABI 1). It no
longer has anything to do with physical addresses.

New signal on a domain: **DEVICE_FAULT** (bit 6), raised when a fault record is waiting (section 7.4).

### 4.2 iodomain_create(resource, device_id, job) -> status, handle

A domain for `device_id`, attached at once (the device leaves Unclaimed or Quarantined). `resource` is the root resource
with MANAGE. `job` is a job handle with MANAGE, or 0 for the caller's own job: that job pays for everything the domain
holds (section 5.3) and the domain holds a use of it until it is freed. Errors, in order: BAD_HANDLE; WRONG_TYPE;
ACCESS_DENIED (either handle); INVALID_ARGS (`device_id` is not an id the tree's `iommu-map` covers); BAD_STATE (a live
domain exists for the id); NO_MEMORY (the job's kernel bytes or objects, the object backstop, the handle table, table frames,
no soft-context id). On any failure nothing is left behind and the device's state is unchanged.

### 4.3 iodomain_map(domain, vmo, vmo_offset, pages, access) -> status, io_address

Maps `pages` pages of `vmo` starting at byte `vmo_offset` into the domain at an I/O address **the kernel chooses**
(section 4.6) and returns it. `access` is `READ` (the device may read the memory) or `READ | WRITE`; there is no
write-only access. The mapping holds the VMO (section 5.1).

Needs IO_MAP on `domain`; on `vmo`, the `DMA` right, READ, and WRITE if `access` has WRITE: **a mapping's access is never
wider than the VMO handle's** (IO4). The VMO must be a DMA VMO (section 5.2): a plain, lazily committed, resizable,
pager-backed, copy-on-write or device VMO is refused.

Errors, in order: BAD_HANDLE; WRONG_TYPE (`domain` is not a domain, `vmo` is not a VMO, or it is a VMO that is not a DMA
VMO); ACCESS_DENIED (a missing right on either handle, or `access` wider than the VMO handle's); INVALID_ARGS (`pages` is 0,
`vmo_offset` is not page aligned, the range is not inside the VMO, a bit of `access` other than READ and WRITE, or READ
missing); BAD_STATE (the domain was destroyed); NO_MEMORY (the job's limit on I/O-mapped pages, its kernel bytes for the
mapping's record or for table frames, or no free I/O range of that length).

**The mapping record.** Each mapping has a record of its own, allocated by this call and charged to the domain's job as
kernel bytes (5.3). There is no fixed number of mappings per domain: what bounds them is the job's limit on I/O-mapped pages
(a mapping is at least one page) and the size of the window (3.3). The record owns everything the mapping's removal will
need (its place in the domain's index, its I/O range, its hold on the VMO, its fence ticket, its link in the pending and
leaked lists), so removal allocates nothing (6.5). It is freed when the mapping's fence has completed; freeing never fails.

Before the call returns, the kernel queues an invalidation for the range and a fence and waits for it (so a device that had
cached "not mapped" for those addresses sees the mapping). On any failure, including that fence, the leaves are cleared and
the roll-back is itself an unmap under section 6.1: nothing is left mapped, and nothing is freed before its fence.

The same page of a VMO may be mapped more than once, in one domain or several; each mapping is charged.

### 4.4 iodomain_unmap(domain, io_address, pages) -> status

Removes one mapping. `io_address` and `pages` must name **exactly** a range one `iodomain_map` returned: a mapping is not
split or merged. Needs IO_UNMAP. When the call returns OK the device can no longer reach the frames and the mapping's hold on
the VMO is gone (section 6.1). Errors, in order: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; INVALID_ARGS (not exactly a live
mapping of this domain); BAD_STATE (the domain was destroyed: its mappings went with it). The call never fails for want of
memory (section 6.5). If the fence does not complete in the bound, the call still returns OK: the device's reach could not be
proven gone, so the frames are leaked, not freed (section 6.4).

### 4.5 iodomain_destroy(domain) -> status, and the last handle

Detaches the device and empties the domain: the device becomes Quarantined, every mapping is removed, every vector is
revoked, in the order of section 6.3. Needs IO_DESTROY. Errors: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED. Destroying a
destroyed domain is OK and does nothing.

**Closing the last handle to a domain destroys it** in the same way (so does the teardown of the last process that held
one). A destroyed domain's remaining handles name a dead object: `iodomain_map` and `iodomain_grant_vector` are BAD_STATE.
The record is freed, its job credited and its use of the job dropped, when the destruction has completed and the last domain
handle and the last vector object of the domain are gone. A vector object (section 8.3) keeps the record alive but does
**not** keep the device attached.

### 4.6 I/O address allocation (Q-IOMMU-MODEL 4)

The kernel chooses every I/O address. Guarantees, not code:

* **Lowest fit** in the DMA part of the window: the lowest run of free pages long enough. Deterministic: the same sequence
  of calls on a fresh domain returns the same addresses.
* Never page 0, never below `IO_BASE`, never inside the interrupt window, never an identity mapping (section 3.3).
* **No reuse before the fence.** A range that was unmapped is *pending* until the fence that follows its invalidation has
  completed; a pending range is never handed out. A range whose fence was abandoned stays pending for the life of the
  domain (section 6.4).
* The allocator's storage is sized when the domain is created; freeing a range allocates nothing.

### 4.7 vmo_create_dma_in(resource, pages, job) -> status, handle

**A new call**, and the only one that makes a DMA VMO (Q-IOMMU-ABI 3 and 4). `resource` is the root resource with MANAGE.

* `job` names the job that is charged the pages (a job handle with MANAGE, or 0 for the caller's own job), as `vmo_create`
  does, so a driver's job pays for its buffers and not `devmgr`'s.
* The frames are **not promised physically contiguous**. There is no per-object page limit and no NO_MEMORY for want of an
  aligned block. The device sees a contiguous range of I/O addresses whatever the frames are.
* The VMO is **pinned, committed and zeroed at creation, and fixed**: its frames never change for its whole life (no
  decommit, no resize, no copy-on-write child, no pager).

Limits: the job's pages; the system backstop on pages held by DMA VMOs (RAM / 16, KERNEL-V1-SPEC 2.5). Errors, in order:
BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED (`resource`, `job`); INVALID_ARGS (`pages` is 0 or past the backstop); NO_MEMORY. The
handle has the VMO rights without EXECUTE, plus DMA.

### 4.8 The two calls the kernel no longer provides: vmo_create_dma and vmo_phys_addr

The system library is additive (KERNEL-V1-SPEC 3.1: a function is never removed or changed; a call the kernel no longer
provides stays as a stub returning NOT_SUPPORTED). Two functions are such stubs:

* **`sys_vmo_create_dma(resource, pages)`**, the two-argument call that `vmo_create_dma_in` replaces. It is not changed in
  place to take a job: it keeps its signature and returns NOT_SUPPORTED, whatever its arguments.
* **`sys_vmo_phys_addr(vmo)`**. It returns NOT_SUPPORTED, whatever its argument.

The kernel has no call behind either (through `sys_raw` the old numbers are BAD_SYSCALL, and they are never reused). **No
physical address is learnable from user mode** (invariant D2 as restated in section 12; Q-IOMMU-ABI 1). This holds whenever
the change lands, before or after the kernel is frozen (Q-IOMMU-ABI 2).

---

## 5. Lifetime, pinning and charging

### 5.1 A mapping holds its VMO

Invariant M8 gains a third kind of holder: **a VMO lives while any handle, any region or any device mapping holds it.** A
device mapping takes its hold at `iodomain_map`, before any leaf is written, and drops it only after the fence that follows
the mapping's invalidation (section 6.1). Therefore:

* closing every handle to a DMA VMO while a device mapping exists frees nothing; the frames are freed when the last holder
  goes, exactly once (D4);
* a killed driver, a killed supervisor, or both, in any order, cannot cause a frame to be freed while a device can reach it:
  process teardown closes handles, and a handle is not what keeps the frames alive for the device;
* the kernel's teardown order is not load-bearing for safety. It is nonetheless fixed (section 10).

### 5.2 What may be mapped: pinned VMOs only

Only a DMA VMO (4.7) can be mapped for a device. Its frame list is fixed at creation, which is what makes the hold enough:
no path changes which frame backs a page of it. Every operation that would change it is refused for the kind (decommit,
resize, a copy-on-write child of it, pager backing). A device VMO (MMIO) is never mappable for a device.

### 5.3 Who pays (Q-IOMMU-ABI 4, Q-IOMMU-MSI 4, OD152)

| What | Charged to | As | When credited |
|---|---|---|---|
| A DMA VMO's pages | the job named at `vmo_create_dma_in` | pages | when the VMO is freed (after its last holder, so after the last fence) |
| A domain's object body, its root table (16 KiB), its MSI page table (one frame), its fault ring (7.4) and its allocator storage | the domain's job | kernel bytes | when the domain's record is freed |
| A mapping's record (4.3) | the domain's job | kernel bytes | when the mapping's fence completes and the record is freed |
| Interior and leaf page-table frames | the domain's job | kernel bytes, exactly | when the table is freed: after the fence of the unmap that emptied it, or at destruction |
| I/O-mapped pages (one per page per mapping) | the domain's job | the per-job **I/O-mapped pages** resource | when the mapping's fence completes |
| A granted vector | the domain's job | the per-job **interrupt vectors** resource | when the vector's identity is freed (section 8.5) |
| A vector's interrupt file in memory (512 bytes, eight to a frame) | the domain's job | kernel bytes | with the vector |

All charges are all-or-nothing up the ancestors (J1) and credited exactly once (J2). A charge whose fence was abandoned is
**not** credited: the memory is leaked and stays charged (section 6.4).

Two resources join the job limits:

* **I/O-mapped pages**: pages mapped into domains whose job this is. It bounds the page tables, the mapping records and the
  address space a job can make the kernel hold for devices.
* **Interrupt vectors**: live vectors of domains whose job this is. It is what stops one job from taking every identity of
  a hart (Q-IOMMU-MSI 4).

`job_set_io_limits(job, io_pages, vectors) -> status` sets them (needs SET_LIMITS; errors BAD_HANDLE, WRONG_TYPE,
ACCESS_DENIED). `job_info` reads usage and limit of each with two new selectors. Exhaustion is NO_MEMORY.

**The limits are hierarchical and have no default.** The smallest value set on the job and its ancestors applies, so a child
job inherits its parent's cap and can never exceed it; usage is counted on the job and on every ancestor (J1). A job on
which none is set, and none of whose ancestors has one, is unlimited (as `job_set_region_limit`). Because there is no
default, **a supervisor sets finite limits of both kinds on every driver job** before it creates a domain for it (section 9),
and a driver's children are bound by them.

The system backstops stay behind the job limits whatever is set: pages held by DMA VMOs, RAM / 16; I/O-mapped pages, the
window of each domain (3.3) and the job's kernel bytes; vectors, the identities the harts' files have; 256 vectors per
device.

---

## 6. The invalidation protocol

This is the heart of the design: the analogue, for a device, of the TLB shootdown rules for a hart. The IOMMU is **one**
device with **one** command queue: there is no per-hart shootdown and no inter-hart interrupt in any of this. Every hart
submits commands under the `IOMMU` lock (section 11). Commands are the IOMMU specification's ("Command queue"): the
translation invalidation for a second-stage address space, by soft-context id and address (`IOTINVAL.GVMA`); the
device-directory invalidation (`IODIR.INVAL_DDT`); and the fence, `IOFENCE.C`, always with its PR and PW bits set, so that
it also orders the device reads and writes the IOMMU had already let through (A5).

### 6.1 Unmap (also the roll-back of a failed map)

1. Under `IOMMU`: clear the leaves of the range; mark the I/O range **pending**; queue the invalidation that covers every
   page of the range for this domain's soft-context id; queue a fence; take a **ticket** (the queue position of the fence).
   Release the lock.
2. **Wait for the fence**: until the queue's head has passed the ticket (6.4). No lock is held.
3. Under `IOMMU`: the range becomes free; table frames the unmap emptied are detached. Release the lock.
4. Only now: drop the mapping's hold on the VMO (which may free its frames), free the detached table frames and the
   mapping's record, credit the job.

**No frame mapped for a device is freed before step 2 has completed** (IO3). The order of steps 1 to 4 is not negotiable
and no step is skipped for speed: no batching across calls, no deferred invalidation, no lazy fence.

### 6.2 Changing an MSI entry (revoke, move): the same protocol

1. Under `IOMMU`: change the entry (invalid, or its notice target rewritten: section 8.5); queue the invalidation of that
   entry (A6) and a fence; take a ticket.
2. Wait for the fence.
3. Only now may the old interrupt identity be retired on its hart (section 8.5), and only after the retirement is it freed
   and may be reused; only now may a revoked vector's interrupt file be freed.

**No identity is reused, and no interrupt file freed, while a cached entry or a message in flight can still name it**
(IO10): the interrupt twin of IO3.

### 6.3 Detach and destroy

1. Under `IOMMU`: mark the domain destroyed (new maps and grants fail); clear the enable bit of every vector's file; write
   the device's context to the **quarantine** state; queue the device-directory invalidation for the device id, the
   invalidation of every translation of the domain's soft-context id, the invalidation of its MSI entries, and a fence; take
   a ticket.
2. Wait for the fence.
3. Retire every vector's identity on its hart (section 8.5).
4. Only now: free the identities, the interrupt files, the MSI page table and every page-table frame; drop every mapping's
   hold; free the soft-context id; credit the job. The record itself goes as in 4.5.

A destruction that begins under another lock (a handle close, a process teardown: section 11) does step 1 at once and
completes steps 2 to 4 before the call returns to user mode or the teardown step ends, holding no lock while it waits. A
ticket whose waiter died is completed by the destruction of its domain, which fences everything.

### 6.4 A fence that does not complete

The wait is a poll of the command queue's head, bounded by `IOMMU_FENCE_BOUND` = 100 ms of the `time` counter. The IOMMU
specification gives no bound for a fence; this is the system's, chosen so that a device that works is never near it. The
wait also ends, as a failure, if the command queue reports an error (an illegal command, a memory fault, a timeout).

If the bound passes or the queue fails: **everything the fence would have released is leaked and counted, never freed and
never reused** (IO8): the frames (the mapping's record, with its VMO hold, is moved to a leaked list and never dropped), the
I/O range (pending for ever), table frames, the soft-context id, interrupt identities and files. Their charges stay. The machine says so: one
console line (`## iommu: fence timeout` with the device id), a counter (section 7.3), and the leak oracles of the test suite
fail. The call that waited returns OK to an unmap or a destroy (the handle's meaning is gone either way) and NO_MEMORY to a
map.

### 6.5 Removal never allocates

`iodomain_unmap`, `iodomain_destroy`, the last-handle path, a vector's revoke and move, and the fault path allocate nothing
(removal is free of allocations that can be refused). Everything they need was allocated and paid
for at creation, map or grant: a mapping's removal uses only what its own record owns (4.3), a destruction walks the records
the domain already has, and a fault is stored in the ring the domain got when it was created (7.4). They cannot fail for want
of memory, and freeing a record cannot fail.

---

## 7. Faults

### 7.1 The fault queue and the handler

The IOMMU's fault-queue interrupt is a kernel-owned source (8.6). Its handler, under `IOMMU`, drains at most 64 records per
interrupt (a device that retries for ever cannot hold a hart), decodes each (IOMMU specification, "Fault/Event queue":
cause, transaction type, device id, address), counts it, finds the device's domain if it has one and stores the record in
that domain's ring (7.4), clears the queue's overflow bit if set (counting the overflow) and acknowledges the interrupt. The console line is
printed after the lock is released, from a copy.

Records that matter here: a device with no directory entry (an Unclaimed device); a read, write or execute of an address
that is not mapped or not mapped for that access; and the MSI causes of section 8.4.

### 7.2 The console line

One structured line per reported fault, at most 16 lines per second for the whole system (faults past the rate are counted
as dropped, not printed):

```
## iommu: fault cause=<decimal> dev=<bb:dd.f> iova=0x<hex> <read|write|exec|none>
```

`dev` is the device id as a PCI address; `iova` is the address the device issued; the last word is the transaction's
direction. A message is a write.

### 7.3 Counters

Readable through `debug_stat` (selector numbers are in `SYSCALLS.md`): faults taken; faults dropped by the rate limit;
fault-queue overflows; the last fault's device id, address and cause; commands issued; fence timeouts; frames leaked by
fence timeouts; live domains; I/O-mapped pages; live vectors; free interrupt identities; stray identities found in vector
files (8.4); messages taken with no route (8.1). The leak oracles of the suite read the last five and the leak counters.

### 7.4 Delivery to the supervisor (Q-IOMMU-MODEL 5)

Every domain has a **ring of 16 fault records and a dropped count**, allocated when the domain is created and part of its
object body (5.3): the fault path allocates nothing (6.5). A fault of a device with a live domain is stored in the ring and
raises **DEVICE_FAULT** on the domain. When the ring is full the new fault is not stored: the dropped count rises by one
(saturating) and the sixteen oldest unread records are kept.

`iodomain_fault_read(domain) -> status, cause, io_address, direction, dropped` returns the **oldest** unread record and
removes it; `dropped` is the number of faults dropped since the previous successful read, and the read resets it.
DEVICE_FAULT stays raised while a record is waiting and is cleared by the read that empties the ring. Needs INSPECT; errors,
in order: BAD_HANDLE; WRONG_TYPE; ACCESS_DENIED; SHOULD_WAIT (the ring is empty). The driver's handle has neither WAIT nor
INSPECT.

The ring is per domain and independent of the console's rate limit (7.2) and of the bound on records drained per interrupt
(7.1): a fault past the console's rate is still stored for its domain.

### 7.5 What the kernel does not do

It does not reset a device, detach it, or kill or signal any process because of a fault. The supervisor's restart policy
decides. A fault never changes a mapping.

---

## 8. Interrupts

Device interrupts are **MSI-X through the IOMMU's MSI translation** (Q-IOMMU-IRQ: B). The kernel owns each device's MSI
translation; a supervisor is granted vectors; a driver gets interrupt objects; a device told to raise anything else faults
in the IOMMU and is reported.

### 8.1 Interrupt identities (Q-IOMMU-MSI 2)

* Every interrupt taken on a hart's interrupt file is an **identity the kernel allocated**, and every allocated identity has
  exactly one **route**: the inter-hart interrupt, a kernel-owned source, a wired source behind an interrupt object, or a
  vector of a domain (IO12). Identity 0 does not exist; identity 1 on every hart is the inter-hart interrupt.
* Identities are allocated **per hart**. A hart's file has the identities `riscv,num-ids` gives (255 on the reference
  machine), so the system has that many per hart. No identity is derived from a source number.
* An identity is opened in its hart's file by that hart itself, when the route is installed, and a route is not usable
  before the hart confirmed it; nothing is open that has no route.
* **Retire.** When a route moves to another hart or ends, its old identity is *retiring*: the old hart reads and clears the
  identity's pending bit (delivering the event to the route if it was set), closes it, and only then is the identity free.
  An identity is never given to another route while a message for its old owner can still arrive.
* A message taken on an identity with no route is counted and reported once; it is delivered to nobody (A12).

### 8.2 MSI translation: MRIF mode only

Every domain's context carries an MSI page table in flat mode, with an address mask and pattern that make the interrupt
window of section 3.3 the only "interrupt-file addresses" of the device: page `k` of the window selects entry `k`, for `k`
below 256 (IOMMU specification, "Process to translate addresses of MSIs"). The table is one frame, empty at creation.

**Every valid entry is in memory-resident-interrupt-file (MRIF) mode; no entry is ever written in basic-translate mode**
(IO11). An MRIF entry names an interrupt file in kernel memory and a *notice*: a target interrupt-file page and an identity.

What a device's message does, and what reaches a hart (AIA specification, the chapter on IOMMU support for MSIs to virtual
machines; A7):

1. The device writes a data word to an address. If the address is in the interrupt window and entry `k` is valid, the IOMMU
   sets the pending bit numbered by the data word in **that vector's own file**.
2. If the enable bit of the same number is set in that file, the IOMMU sends the notice: a message to the entry's target
   page with the entry's identity. **The page and the identity of every message that reaches a real interrupt file were
   written by the kernel**, never by the device.

The kernel enables only identity 1 in a vector's file. The MSI-X entry for vector `k` is therefore: address
`IO_BASE + k * 4096`, data 1.

Guest interrupt files are not used and are left for a hypervisor. A fallback for an IOMMU without MRIF (one guest interrupt
file per device with basic translation) is a non-goal (section 14): without `MSI_MRIF` the kernel does not boot.

### 8.3 Vector grants and interrupt objects (Q-IOMMU-MSI 4 and 6)

**iodomain_grant_vector(domain, index) -> status, interrupt, msi_address, msi_data.** Grants vector `index` of the domain's
device: a valid MRIF entry at `index`, an interrupt file of its own, an identity on a hart, and an **interrupt object** of
the kind *vector*. Needs IO_GRANT: **only a holder of the domain's control handle (the supervisor) grants**; a driver's
handle cannot. Returns a handle to the interrupt object and the pair to program into MSI-X table entry `index`. The pair is
neither a secret nor an authority: it works only for this device, through this domain. Errors, in order: BAD_HANDLE;
WRONG_TYPE; ACCESS_DENIED; INVALID_ARGS (`index` is 256 or more); BAD_STATE (the index is granted and live, or the domain was
destroyed); NO_MEMORY (the job's vector limit, its kernel bytes, no free identity on any online hart, the handle table).

There are two kinds of interrupt object: a **wired source** (`interrupt_create`, as before, with the identity now the
kernel's choice) and a **vector** of a domain. For its holder they behave alike: wait for INTERRUPT, service the device,
`interrupt_ack`. The rights are those of an interrupt object. The handle a driver is given can wait and acknowledge
and **names nothing else**: no identity, no hart file, no address, and it cannot grant or revoke.

A vector is **revoked** when the last handle to its interrupt object closes, or when its domain is destroyed (6.2, 6.3).
After a domain's destruction a remaining vector handle names a dead object: it never fires again and `interrupt_ack` is
BAD_STATE. Limits: 256 vectors per device; the job's vector limit; the identities of the harts.

### 8.4 What a hostile device can do

| The device writes | What happens |
|---|---|
| to a page of the interrupt window whose vector is not granted | the entry is invalid: an IOMMU fault (the MSI-entry-not-valid cause), reported with the device id; nothing is delivered |
| anywhere else: a real IMSIC page, the APLIC, RAM it was not mapped | an ordinary translation fault, reported; nothing is delivered and nothing is written |
| to a granted page with a data word other than 1 | a pending bit in that vector's own file whose enable bit is clear: nothing is delivered. The kernel finds the stray bit at the next acknowledgement or at revoke, clears it and counts it. A data word no file can hold, or a misaligned address, is an IOMMU fault |
| to a granted page with data 1 | its own vector fires. That is the grant |
| a flood of valid messages | masked after the first: 8.5. The cost to the target hart is one interrupt per acknowledgement |

Cause numbers are the IOMMU specification's (A4); every reported fault names the device.

### 8.5 Masking, acknowledgement, placement and moves

**Masking is the enable bit of the vector's own file** (invariant D5 for a vector). The kernel changes the file only with
atomic operations, from any hart, with no IOMMU command, no inter-hart call and without the `IOMMU` lock; the IOMMU may
write the file at any moment. The object is *armed* (enable set, INTERRUPT clear) or *fired* (enable clear, INTERRUPT
raised).

* **A notice arrives** (on the vector's identity, on its hart). If the object is fired: nothing (a stale notice). If armed:
  clear enable; read and clear pending. If pending was set: the object is fired, INTERRUPT is raised. If it was not (another
  path took it): set enable again, read and clear pending once more, and if that finds it set, clear enable and fire.
* **`interrupt_ack`** on a fired object: clear INTERRUPT; set enable; read and clear pending; if it was set, clear enable
  and fire again at once from software.
* While fired, a message sets pending and sends nothing. So an interrupt is delivered **exactly once per pending bit**,
  never by counting notices; a notice that arrives after software took the bit finds it clear and does nothing. The
  argument does not depend on whether the IOMMU sends a notice for every message or only when the pending bit rises, nor on
  the IOMMU's update of the file being atomic: the worst a non-atomic update can do is set a pending bit the kernel just
  cleared, which costs one spurious interrupt and never a lost one (A7). Drivers tolerate a spurious interrupt, as they
  always had to.

**Placement** (Q-IOMMU-MSI 3). At the grant the vector is placed on the online hart with the fewest routes. At the first
`object_wait` or `interrupt_ack` on it, it is moved once to the hart that call runs on. After that the kernel moves it
**only** when its hart goes offline, or when its holder asks: **`interrupt_deliver_here(interrupt) -> status`** moves it to
the calling hart (needs the acknowledge right; errors BAD_HANDLE, WRONG_TYPE, ACCESS_DENIED, BAD_STATE for a dead object,
NO_MEMORY when the calling hart has no free identity; on a wired source it re-aims the source as an acknowledgement does).
There is **no automatic follow** of the driver thread: an acknowledgement never issues an IOMMU command.

**A move**: take an identity on the new hart and have it opened there; under `IOMMU`, rewrite the entry's notice target
(page and identity) **with one store**, so that the entry never names a pair the kernel did not allocate to this vector
(A16); invalidate the entry and fence (6.2); wait; retire the old identity on the old hart (8.1). Both identities route to
the vector until the old one is retired.

**Hart offline** (KERNEL-V1-SPEC 5.3): every vector, wired source and kernel-owned source placed on the hart is moved first,
by the same steps. A hart is not taken offline while a route on it cannot be moved (no identity free elsewhere):
`hart_set_online` fails with NO_MEMORY and nothing has changed.

**Revoke**: clear enable; make the entry invalid; invalidate and fence; wait; retire the identity; free the identity, the
file and the vector's charge (6.2).

A Quarantined device has no MSI page table: it cannot interrupt at all.

### 8.6 Wired sources, and the IOMMU's own interrupts

* **PCI INTx is never admitted.** No source named by a PCI host's `interrupt-map` is ever a source `interrupt_create`
  accepts; every claimed function has INTx disabled in its command register (section 9); **a DMA-capable device without an
  MSI-X capability is not claimable** (Q-IOMMU-MSI 5). A shared wired line is itself an isolation hole.
* The older MSI capability is not used: MSI-X only.
* **The IOMMU's own interrupts are wired** (Q-IOMMU-IRQ 3): kernel-owned sources, with identities from the same per-hart
  allocator, never in the set `interrupt_create` admits, moved when their hart goes offline. The kernel uses the
  fault-queue and command-queue sources; the performance-monitor and page-request sources stay disabled.

### 8.7 The limit of the interrupt isolation, stated

Identity isolation rests on `MSI_MRIF`, an optional capability of the IOMMU specification. It is a profile requirement
here. An IOMMU without it could confine a device only to a *file* (one hart), not to an identity: a hostile driver could
then raise any interrupt on that hart, another device's or the inter-hart interrupt. This system does not run on such a
machine. MRIF mode costs a memory update and a second message per interrupt; that is the price of the isolation.

---

## 9. The bus and the device manager

**What the kernel knows about PCI: nothing** but the tree's `iommu-map` (which device ids exist) and the PCI host's windows
as device windows for the root resource. It walks no configuration space and knows no capability.

**What `devmgr` does**, holding the root resource: enumeration of bus 0; BAR assignment (no firmware does it on the
reference machine: every BAR is unassigned at hand-over); the capability walk; the MSI-X capability (the table and the
pending-bit BARs); the claim.

**The claim, in order.** Each step is a precondition of the next, and **bus-master enable comes last**:

1. Refuse the device, loudly, and leave bus mastering off, if it has no MSI-X capability (8.6), or if it is a virtio device
   that does not offer `VIRTIO_F_ACCESS_PLATFORM` (section 15, L1).
2. `iodomain_create` for the device id, naming the **driver's job**, on which the supervisor has set finite limits on
   I/O-mapped pages and on interrupt vectors (`job_set_io_limits`, 5.3).
3. Create the driver's DMA VMOs (`vmo_create_dma_in` with the driver's job).
4. Grant the vectors the driver class needs; write each returned pair into the MSI-X table with the function masked;
   disable INTx; enable MSI-X.
5. Hand the driver its authority (below).
6. Enable bus mastering.

**What a driver is handed** (`proto::devmgr`): a VMO over its register BAR (never the BAR that holds the MSI-X table where
the device keeps the table in a BAR of its own, as virtio-pci does); a duplicate of the domain handle with IO_MAP and
IO_UNMAP only; its DMA VMOs with the `DMA` right and the access it should be able to give the device; one interrupt handle
per vector. The driver maps its buffers itself and learns the I/O addresses from `iodomain_map`.

**What a driver never sees**: configuration space; the ECAM window; the MSI-X table (where separate); a control right on its
domain; a physical address.

**Isolation does not depend on any of this.** A device whose MSI-X table sits beside its registers can be pointed anywhere
by its driver and is still confined by section 8. A supervisor that enables bus mastering too early exposes nothing: the
device is denied until it is attached and reaches only what is mapped.

**Read-only for the device** is how a supervisor narrows a driver: a handle to a buffer given to a driver without WRITE can
only be mapped for device reads (IO4). The network supervisor gives the driver the transmit pool that way, so a device can
never be made to overwrite frames the stack is sending.

---

## 10. Device reset and teardown ordering

In each sequence, **K** marks a step the kernel guarantees whatever user mode does, and **S** a step that is the
supervisor's manners: it keeps the device working and the fault log quiet, and safety does not depend on it. A reset is
always the supervisor's, for function, never for safety.

### 10.1 A clean driver exit

1. (S) The driver stops its device and unmaps its buffers (`iodomain_unmap`: K, the protocol of 6.1), then exits.
2. (S) The supervisor masks the MSI-X table, disables MSI-X and clears bus-master enable.
3. (S) The supervisor resets the device: the device's own reset (for a virtio function, status 0, read back), and a
   function-level reset where the function advertises one.
4. (S) The supervisor destroys the domain. (K) Section 6.3: quarantine, invalidations, fence, wait, identities retired, then
   identities, files and tables freed and every remaining hold dropped.
5. (K) Frames return to the allocator when the last holder of each VMO has gone: never before step 4's fence for a VMO that
   was still mapped.

### 10.2 A killed driver (Q-IOMMU-MODEL 6)

1. (K) The driver's handles close. Its mappings **stay**: they belong to the domain, which the supervisor's handle keeps
   live, and they hold their VMOs. The device may still be running and reaches exactly what it reached before. Nothing is
   freed.
2. (S) The supervisor masks the MSI-X table, disables MSI-X and **clears bus-master enable**, then resets the
   device as in 10.1 step 3: before the domain is torn down.
3. (S) The supervisor destroys the domain. (K) As 10.1 step 4.
4. (K) As 10.1 step 5.

### 10.3 A killed supervisor

1. (K) Its handles close. If a driver still holds a handle to the domain, the device stays attached and the driver keeps
   exactly its reach; no one can grant, destroy by call or read faults any more, and the domain is destroyed when the
   driver's handle goes.
2. (K) When the last domain handle goes (at once, if no driver holds one): section 6.3, with no reset and with bus
   mastering still on. The device, still running, is Quarantined: its transactions are refused and not reported, and it
   cannot interrupt.
3. (K) Frames return as in 10.1 step 5.

Killing the driver and the supervisor together, in either order, is 10.3: the
kernel's steps alone make it safe.

### 10.4 A re-claim

1. The device is Unclaimed or Quarantined: it reaches nothing.
2. (S) The supervisor resets the function (it may have been left running) and runs the claim of section 9 from step 1.
3. (K) `iodomain_create` moves the device from its deny state to the new domain's (3.1), with an empty page table and an
   empty MSI page table: nothing of the previous domain is inherited, and its I/O addresses mean nothing in the new one.

### 10.5 System shutdown, reset and suspend

1. (S) Supervisors stop their drivers and clear bus mastering.
2. (K) The kernel does **not** tear domains down and does not turn the IOMMU off or to pass-through: its programmed state
   stands until the machine resets, so no device gains reach at any instant the kernel still runs.
3. (K) After a resume that lost the IOMMU's state, section 2.4 runs again before any process does.

### 10.6 The order for interrupts, in every sequence

1. (S, for quiet) The MSI-X table is masked and MSI-X disabled.
2. (K) Each vector's enable bit is cleared, its entry made invalid, and the device's translations and MSI entries are
   invalidated and fenced.
3. (K) The domain's mappings are unmapped under the same fence or their own.
4. (K) Only then are the interrupt identities retired on their harts and freed, and the files and the MSI page table
   returned.

All but step 1 are the kernel's guarantee.

---

## 11. Locking

### 11.1 The class

One new lock class, **`IOMMU`**, one instance. It protects the domain store (every domain's page table, allocator, mapping
records, fault ring and state), the device directory, the MSI page tables, the command queue and the fault queue, and the soft-context
ids. Its place in the order of SCHED-SMP-SPEC 3.3 (as amended) is **after `KSTACK` and before `FRAME`**:

`OBJ`, `AS`, `PROCS`, `FUTEX`, `VMO`, `JOBS`, `PI`, `TT`, `RQ`, `KSTACK`, **`IOMMU`**, `FRAME`, `HEAP`, `ENTROPY`, `UART`.

* It is after `OBJ` because the fault interrupt arrives on the external-interrupt path, which holds `OBJ`, and because the
  handle-close path reaches destruction under `OBJ`.
* It is after `VMO` and `JOBS` so that no path ever needs `IOMMU` and then `VMO`: the order forbids it, and section 6 is
  written so that the hold is dropped after the lock is released.
* It is before `FRAME` and `HEAP` because a map takes table frames while it writes leaves.
* It is taken with interrupts off (the fault handler takes it). **It is never held across a wait**: every fence is waited
  for with the lock released (the ticket and the pending state make that safe).
* Under it nothing takes `OBJ`, an `AS` lock, `VMO`, `JOBS`, or makes a thread or scheduler call. The console line of a
  fault or a timeout is printed after it is released.
* Never two.

Interrupt identities and routes (8.1) and a vector object's armed or fired state are under `OBJ`, with the other interrupt
objects. A vector's interrupt file is touched only with atomic operations and belongs to no lock.

### 11.2 Every path, and what it already holds

| Path | Holds on entry | Takes, in order |
|---|---|---|
| `iodomain_create` | nothing (handles resolved under `OBJ`, released) | `JOBS` (charge), released; `IOMMU` (context, queue), with `FRAME` inside; released; wait; `OBJ` (the handle) |
| `iodomain_map` | nothing | `VMO` (check the kind, take the hold, read the frame list), released; `JOBS` (charge), released; the mapping's record is allocated with no lock held; `IOMMU` (reserve, link the record, write leaves, queue), `FRAME` inside for table frames; released; wait |
| `iodomain_unmap`, and a map's roll-back | nothing | `IOMMU` (6.1 step 1), released; wait; `IOMMU` (step 3: the record is unlinked), released; then `VMO`, `JOBS`, `FRAME` for the release, each in order, and the record is freed with no lock held |
| `iodomain_destroy`, last handle close, process teardown | `OBJ` for the close; nothing when it completes | `IOMMU` (6.3 step 1) under `OBJ` or alone; then, with nothing held: wait; `OBJ` (retire identities), released; `IOMMU` (detach frames), released; `VMO`, `JOBS`, `FRAME` for the releases |
| The fault interrupt | `OBJ` | `IOMMU` (drain), released; raises DEVICE_FAULT under `OBJ`; prints with nothing but `OBJ` |
| `iodomain_grant_vector`, revoke, a move, `interrupt_deliver_here` | `OBJ` (identity and object) | `JOBS` under `OBJ` (the vector charge, as object quotas are); `IOMMU` (entry write, queue) under `OBJ`; all released; wait; `OBJ` again to retire and free |
| A notice (a vector fires) | `OBJ` | **no lock**: atomic operations on the file. Never `IOMMU` |
| `interrupt_ack` on a vector | `OBJ` | no lock |
| The release of a VMO whose last holder was a device mapping | nothing (it is step 4 of 6.1) | `VMO`, `JOBS`, `FRAME`, as any VMO release |
| Hart offline | what the offline path holds, without `TT` or `RQ` | per route: `OBJ`, then `IOMMU` for a vector; wait with nothing held |

No path holds `TT`, `RQ` or `KSTACK` when it takes `IOMMU`. When the single `OBJ` lock is split (OD149), the route table and
the vector state get a lock rule of their own; `IOMMU` stays where it is.

---

## 12. Invariants

New rows for `INVARIANTS.md` (the reference document changes with the code; the text is fixed here):

| Id | Invariant |
|---|---|
| IO1 | **Default deny.** A device id with no live domain reaches no memory and causes no interrupt. From the write of the directory mode to `ddtp` on, the directory holds an entry only for a device that is Attached or Quarantined |
| IO2 | **No pass-through state is ever written**: not `ddtp` Bare, not a device context with both stages Bare. The encoding layer cannot construct one. The only values written to `ddtp` are Off and the directory mode |
| IO3 | A frame mapped for a device is freed only after the fence that follows the invalidation of its mapping has completed |
| IO4 | A mapping's access is within the rights of the VMO handle it was made with, and only a pinned (DMA) VMO is ever mapped |
| IO5 | At most one live domain per device id; a device context changes only between a deny state and one domain's state, never from one domain to another |
| IO6 | No physical address is learnable from user mode, and no I/O address equals a physical address of the machine |
| IO7 | I/O-mapped pages, mapping records, table frames, vectors and interrupt files are charged to the domain's job before they exist and credited exactly once, when they are freed |
| IO8 | A fence that does not complete leaks what it guarded (frames, mapping records, I/O ranges, tables, identities, files): counted, reported, never freed, never reused |
| IO9 | A device causes only the interrupts of vectors granted on its domain: every message that reaches a real interrupt file carries a page and an identity the kernel wrote |
| IO10 | An interrupt identity is freed only after the fence that follows its entry's invalidation and after its hart retired it; an interrupt file is freed only after that fence |
| IO11 | No MSI page-table entry is ever written in basic-translate mode. The encoding layer cannot construct one |
| IO12 | An interrupt identity has at most one live route, and every identity open in a hart's file has a route |

Changes to existing rows:

| Row | Becomes |
|---|---|
| D2 | **No physical address is learnable from user mode** (was: learnable only through a DMA-right handle) |
| D3 | Device objects, **including device domains**, are minted only with the root resource |
| D4 | DMA frames are freed exactly once, after the last handle, the last region **and the last device mapping** go, and for a device mapping after its fence; the DMA backstop and the job's page charge come back then |
| D5 | Unchanged for a wired source. For a vector, "masked" is the enable bit of the vector's own file; **one live object per source, and one per vector index of a domain**; a destroyed vector object leaves its entry invalid |
| M8 | A VMO lives while any holder does: a handle, a region **or a device mapping** |
| H6 | Process death releases every object it held; a domain whose last handle it held is destroyed by section 6.3, and frames a device could reach are freed only after that fence |
| M3 | The baseline includes domains, I/O-mapped pages, mapping records, table frames, vectors, interrupt files and interrupt identities; deliberately leaked items (IO8) are counted apart and fail the leak oracle |
| J1 | The I/O-mapped page charge, the vector charge and the kernel bytes of a domain and of its mapping records are applied to the job and every ancestor, all or nothing; the two limits of 5.3 are the smallest set on the job and its ancestors |
| J2 | They are credited exactly once; a domain holds a use of its job from creation until its record is freed |
| C1, K5 | The order gains `IOMMU` between `KSTACK` and `FRAME` (section 11) |

---

## 13. Testing regime and conformance

The policy is `TESTING.md`: logic on the host, syscall behaviour in the suite, only what the machine shows in QEMU. These checks define conformance: the kernel conforms when they pass. The QEMU checks run in the kernel-only boots at one, two and four harts.

**Independent oracles.** The hardware formats are checked against vectors transcribed from the IOMMU specification's tables
and from QEMU's own headers and qtest, not derived from the code under test; two walkers (one written from the translation
algorithm, one with the mapper) are compared; a **simulated device** walks the real structures the way a device access
would, with a translation cache and an MSI-entry cache that only invalidation commands drop; in the machine, QEMU's device
is the independent implementation, driven through an independent device (QEMU's IOMMU test device where it can do what the
check needs, otherwise a virtio-pci function driven by hand: A10) and, where it works, through the IOMMU's own debug
translation interface (A11).

**Negative-control kernels** (test fixtures; each must make its named test fail, run once by hand when the test is
written):

| Fixture | Breaks | Must fail |
|---|---|---|
| `fault-iommu-bare` | IO2: writes `ddtp` Bare | `iommu_is_on_and_denies_by_default` |
| `fault-iommu-skip-inval` | IO3: unmap frees without invalidation and fence | `iodomain_unmap_stops_the_device`, `dma_survives_a_dead_supervisor` (A5) |
| `fault-iommu-msi-basic` | IO11: entries in basic-translate mode | `msi_forgery_faults`, the wrong-data row |
| `fault-msiid-free-at-move` | IO10, IO12: an identity freed at the move | `interrupt_moves_do_not_lose_or_leak` (A12) |

**Conformance: each statement and the check that shows it.**

| Statement (section) | Check | Where |
|---|---|---|
| Formats, the forbidden constructors (2.4, 3.1, 8.2; IO2, IO11) | vectors, reference models, the two-walker differential test, the MSI model, mutation checks | host |
| Hold, unmap before free, charging (5, 6; IO3, IO7, IO8, M8, D4, J1, J2) | `iodomain_model` (with many more mappings in a domain than any fixed table would hold, bounded only by the job's limit), `iodomain_concurrency` (the frame-free event is always after the fence-complete event), `iodomain_noalloc` (6.5: unmap, destroy and the fault path allocate nothing; a map fails cleanly when its record cannot be allocated) | host |
| The two job limits are hierarchical (5.3; J1) | the job ledger tests: a child job is refused at its parent's limit on I/O-mapped pages and on vectors, with nothing charged anywhere | host |
| Identities and routes (8.1; IO12) | `msiid_model`; `interrupt_identities_above_63_work`, `interrupt_moves_do_not_lose_or_leak` | host and suite |
| The boot requirement and refusals (2) | `os-boot-test-iommu-neg` (the `no IOMMU` line); the capability check's host test; the boot line's mode | boot scripts and host |
| A hand-over state other than Off is turned Off first (2.4 step 1; IO2; A17) | the bring-up sequence against the simulated device handed over in each mode (Off, Bare, each directory mode, with and without live queues): the first write to `ddtp` is Off, no Bare is ever written, the directory mode is written once; a simulated device that never reads back Off gives `IOMMU: will not go off` | host |
| Default deny for memory and messages, the fault line (3.1, 7; IO1) | `iommu_is_on_and_denies_by_default`, `iommu_denies_messages_by_default`, the console line's format, `iommu_translation_matches_the_tables` | suite and kernel self-test |
| Fault delivery to the supervisor (7.4) | `iodomain_model` (the ring keeps the sixteen oldest unread records and counts the rest as dropped; a read returns the oldest and the count); `iodomain_faults_reach_the_supervisor` (DEVICE_FAULT, the records in order, the dropped count after more than sixteen faults, nothing readable through a driver's handle) | host and suite |
| Exactly what was mapped, with the access granted (1, 4; IO4, IO6) | `iodomain_maps_exactly_what_was_granted` (including an access at the frames' physical addresses and at address 0) | suite |
| Unmap cuts the device off before a frame is freed (6.1; IO3) | `iodomain_unmap_stops_the_device`; `iodomain_outlives_its_makers` (10.3: a quarantined device's late write fails in the device and the memory is intact) | suite |
| Rights and errors of every call, in order (4, 5.3, 8.3) | `iodomain_rights_and_errors` and the syscall-test table; the fuzzers and the allocation-failure tests | host |
| A granted vector is delivered, masked and re-fired (8.5; D5) | `msi_granted_vector_is_delivered`; `iodomain_vectors_model`, `iodomain_vectors_concurrency` | suite and host |
| Nothing ungranted is delivered (8.4; IO9) | `msi_ungranted_vector_faults`, `msi_forgery_faults`, `msi_revoke_stops_the_vector` | suite |
| An identity is not reused early (6.2; IO10) | `iodomain_vectors_model` with the simulated MSI-entry cache. **No check in the machine can show it** (section 15, L4) | host |
| The claim (9) | `pci_devices_are_found_and_claimed`, `pci_virtio_blk_reads_through_the_iommu`, `pci_virtio_outside_its_mapping_faults`, `a_child_has_no_device_authority`, `device_caps_need_the_resource` | suite |
| MSI-X through `devmgr`; a driver holds nothing it could forge with (8.3, 9) | `pci_virtio_blk_interrupts_through_msix`, `pci_two_devices_do_not_share`, `pci_driver_cannot_forge_through_devmgr` | suite |
| A killed driver, a hostile driver, on the block device (10.2) | `blk_driver_killed_mid_write`, `blk_driver_cannot_reach_outside`, `blk_driver_restart`; zero IOMMU faults in every boot that has no deliberate fault | suite and boot script |
| Read-only for the device (9; IO4) | `net_device_cannot_write_the_transmit_pool`; `rng_seeds_through_the_iommu` | suite |
| No DMA device outside the IOMMU; no physical address (2.6, 4.8; D2) | `no_dma_device_outside_the_iommu` (which also sees `sys_vmo_phys_addr` and the two-argument `sys_vmo_create_dma` return NOT_SUPPORTED); the device-window host test (`virtio,mmio` yields none) | suite and host |
| A killed driver and a killed supervisor (10.3) | `dma_survives_a_dead_supervisor` and the hostile-driver matrix for memory and for messages | suite |
| A driver job has finite limits, and its children inherit them (5.3, 9) | the hostile-driver matrix: a driver that maps until refused, and one that spawns a child job to map or to hold vectors for it, each end in NO_MEMORY at the limits its supervisor set, with nothing leaked and no other job disturbed | suite |
| Nothing leaks (IO7, IO8, M3) | the end-of-session oracles: zero live domains, I/O-mapped pages, vectors and leaked holds; identities at baseline | suite, at the end of every session |

A fresh-context review attacks sections 4, 5, 6, 8, 10 and 11 against `INVARIANTS.md`, looking for an order in which a frame
is freed while a translation to it can be used, an identity reused while an entry can name it, and any message a device can
cause that is not one of its vectors.

---

## 14. Non-goals

Nested translation for guests; first-stage tables; ATS, page requests (PRI) and process contexts (PASID), which are never
enabled even where the device offers them; hardware updates of accessed and dirty bits; superpage I/O mappings; splitting or
merging a mapping; sharing one domain between devices by choice; bridges and buses other than bus 0; hotplug; an IOMMU other
than the platform RISC-V one, or more than one; PCI INTx; the older MSI capability; guest interrupt files, and the fallback
that would use one per device on an IOMMU without MRIF; basic-translate MSI entries; a kernel that resets or kills on a
fault; the IOMMU's performance monitor; virtio-mmio on the OS track.

---

## 15. Limits and assumptions

### 15.1 Limits of the isolation, stated plainly

* **L1. QEMU's virtio devices opt in to the IOMMU.** A virtio-pci device started without `iommu_platform=on` does its DMA
  on guest-physical addresses and **bypasses the IOMMU**, whatever the kernel does, and the kernel cannot see the
  difference (A9). This is a property of the machine, like `-cpu rva23s64`: every test machine starts every virtio-pci
  device with `iommu_platform=on,disable-legacy=on`; `devmgr` refuses a virtio device that does not offer
  `VIRTIO_F_ACCESS_PLATFORM`; and a negative test on each test machine (`pci_virtio_outside_its_mapping_faults`) proves its
  devices do fault. A machine assembled otherwise is not isolated and nothing in the kernel says so.
* **L2. Identity isolation is MRIF-only** (8.7). There is no second mechanism behind it.
* **L3. One implementation.** QEMU's is the only IOMMU this system has run on. The timing of fences, the caching of
  translations and of MSI entries, and the update of interrupt files on real hardware are known only from the
  specifications' text, and partly not even from that (below).
* **L4. No negative control in the machine for MSI-entry caches.** QEMU caches no MSI translation, so "an identity is freed
  only after the fence" (IO10) cannot be made to fail there. Its only proof is the host model with the simulated device's
  cache. The same holds for IO3 if A5 turns out false.
* **L5. The fence bound** (6.4) is this system's number, not the hardware's.
* **L6.** What section 1 puts out of scope: a device that lies about its requester id, denial of service, side channels.

### 15.2 Assumptions about the machine

Each is a fact about the machine or the IOMMU specification that the design relies on, stated with what depends on it and
where it is settled: a boot check the kernel makes, or a named test. Where an assumption turns out false the design does not get to
proceed quietly: the named check fails, or the kernel refuses to boot.

| # | Assumed | The design needs it for | Settled by |
|---|---|---|---|
| A1 | The fields of `capabilities` are where the IOMMU specification's "Register layout" puts them (the positions of `MSI_FLAT`, `MSI_MRIF`, `AMO_HWAD`, `ATS`, `IGS`, `PAS` were read in QEMU's header; the version and stage fields were decoded from memory) | the boot check (2.3) | spec-derived vectors in the host tests; the boot line prints the raw value |
| A2 | A device that is not a guest's may be translated by the second stage alone (first stage Bare, `iohgatp` in a paging mode), with an MSI page table; and the MSI address check then precedes the page table (the second half was read in QEMU's source; neither was checked against the specification's text) | 3.2, 8.2 | `iodomain_maps_exactly_what_was_granted`, `msi_granted_vector_is_delivered`, `msi_ungranted_vector_faults` |
| A3 | The leaf bits the stage requires (user, accessed, dirty) are as the format layer encodes them | 3.2 | the two-walker test against vectors; `iommu_translation_matches_the_tables` |
| A4 | The directory's level count and device-id split for the extended format, the command and fault-record encodings, and the cause numbers (the MSI causes were read in QEMU's source) | 2.3, 6, 7, 8.4 | spec-derived vectors; the cause printed by the fault line in the suite's checks |
| A5 | A fence with PR and PW orders earlier device accesses, and QEMU's translation cache serves a stale entry until it is invalidated | 6; the `fault-iommu-skip-inval` control | the control is seen to fail its test; if it cannot, the host model is the only proof and that is said (L4) |
| A6 | Which command invalidates a cached MSI page-table entry on real hardware (from memory: the second-stage invalidation with the interrupt-file address), and that a fence orders it | 6.2 | the simulated device models the cache; nothing in the machine (L4) |
| A7 | MRIF behaviour on real hardware: when a notice is sent, whether the file update is atomic, whether an IOMMU may cache a file's contents or an entry's notice target | 8.2, 8.5 (written not to depend on the first two; a cached notice target is covered by 6.2) | the MSI model; `msi_granted_vector_is_delivered` on QEMU |
| A8 | The platform device has MSI translation switched on (its capability bits say so) | 8 | the boot check; `iommu_denies_messages_by_default` |
| A9 | With `iommu_platform=on`, a driver that does not accept `VIRTIO_F_ACCESS_PLATFORM` cannot finish feature negotiation; with it off, the device bypasses the IOMMU; what a virtio device does on a faulted descriptor | L1, section 9 | `pci_virtio_outside_its_mapping_faults`; the transport's host tests |
| A10 | What QEMU's IOMMU test device can be commanded to do (a DMA to a chosen address, a message to a chosen address with a chosen data word) | section 13's independent device | `iommu_is_on_and_denies_by_default`, the first check that needs it; the fallback is a hand-driven virtio-pci function |
| A11 | The IOMMU's debug translation interface works in QEMU | `iommu_translation_matches_the_tables` | that self-test: a counted skip the boot script refuses, never a silent pass |
| A12 | What an IMSIC does with an identity above `num-ids` or a message to a stopped hart's file, and whether QEMU ever leaves a message in flight | 8.1; the `fault-msiid-free-at-move` control | `msiid_model`; the control, or a plain statement that the host model is the only proof |
| A13 | A function with MSI-X enabled, or with INTx disabled in its command register, never raises INTx | 8.6 | the suite's count of interrupts with no route stays 0 |
| A14 | A valid context with fault reporting off suppresses the records of a Quarantined device's refused transactions | 3.1 | `iodomain_outlives_its_makers`, `blk_driver_killed_mid_write` (no fault storm; the counters) |
| A15 | A device's ATS or page request is refused when the context does not enable it | 14 | the hostile-driver matrix |
| A16 | An MRIF entry's notice page and identity are in one doubleword, so one store changes both | 8.5, the move | a spec-derived vector; were it false, a move is a revoke and a new grant, and 8.5 says so by amendment |
| A17 | `ddtp` may be changed directly from an enabled mode (a directory mode, or Bare) to Off; the change is complete when the register's busy bit is clear and Off reads back; nothing else must be waited for before the queues are set up again (transactions already in flight, a queue still enabled) | 2.4 step 1 | the boot check itself (Off is read back within the bound, or the boot is refused with `IOMMU: will not go off`); the host test of the bring-up against the simulated device handed over in each mode |

---

## 16. Choices made beyond the recorded decisions (owner-decided 2026-11-25)

OD148 and the four answered questions fix the model. Writing the contract needed twelve further choices. The owner decided
each of them on 2026-11-25 (OD166): eight as the draft proposed them, two of those with a condition, and four changed. The
sections named state the result; this list says which is which.

1. **An explicit `iodomain_destroy` with a right of its own** (4.5). Accepted. With the rights split a driver holds a domain
   handle, so "destroy by the last handle" alone would leave a supervisor unable to cut a live driver's device off.
2. **A killed driver's domain is destroyed and re-created at the re-claim** (10.2, 10.4). Accepted. It follows
   Q-IOMMU-MODEL 6 as revised; mappings and grants are not kept across driver lifetimes.
3. **The domain's job is named at creation** (4.2) and pays for tables, mapping records, I/O-mapped pages and vectors.
   Accepted. Q-IOMMU-ABI 4 says "the domain creator's job" and Q-IOMMU-MSI 4 "the job the domain was created for".
4. **The two new job limits have no default** (5.3): unlimited unless set, like the region limit. Accepted, **on a
   condition**: the limits are hierarchical, so a child job inherits its parent's cap; a supervisor sets finite limits on
   every driver job (5.3, 9), and the hostile-driver matrix tests that (13); the system-wide backstops apply whatever is set.
5. **Translation and the shape of a domain's address space** (3.2, 3.3, 4.3). **Changed.** Kept: second-stage translation
   only, Sv39x4, a fixed window of 1 GiB at 2^40, 4 KiB pages only. Replaced: the draft's fixed table of 64 mappings per
   domain. A mapping has a record allocated at map time and freed after its fence; the number of mappings is bounded by the
   job's limit on I/O-mapped pages; removal still allocates nothing, because the record owns everything removal needs (4.3,
   6.5).
6. **The fence is waited for with no lock held, by polling, for at most 100 ms** (6.4, 11.1). Accepted.
7. **A map also invalidates and fences** (4.3). Accepted.
8. **Fault delivery** (7). **Changed.** The draft kept one record (the oldest unread) and a count per domain. A domain has
   a ring of 16 records and a dropped count, allocated at its creation; `iodomain_fault_read` returns the oldest record and
   the count of dropped (7.4). Kept: 16 console lines per second; 64 records drained per interrupt.
9. **Names and numbers** (4.1, 4.7, 4.8, 8.3, 8.5). **Changed.** Kept: the call names, right bits 14 to 17 and signal bit 6.
   Replaced: the draft changed `vmo_create_dma` in place by giving it a third argument. The job is named through a new call,
   `vmo_create_dma_in`; the two-argument `vmo_create_dma` stays in the system library as a stub returning NOT_SUPPORTED, as
   `vmo_phys_addr` does (KERNEL-V1-SPEC 3.1: the library is additive), before or after the freeze.
10. **A hart is not taken offline while a route on it cannot be moved** (8.5). Accepted.
11. **The hand-over state** (2.4, 2.5). **Changed.** The draft refused to boot on any hand-over mode other than Off. The
    kernel writes Off, waits as the IOMMU specification requires, reads it back and continues with the normal bring-up; the
    boot is refused only if the IOMMU will not go Off (A17). IO2 holds: Off is not a pass-through state.
12. **Whole-mapping unmap only** (4.4). Accepted.

---

## Amendment 2026-11-25: what QEMU 11.1.3 showed (IOMMU0)

Owner-approved. Source: `docs/records/notes/2026-11-25-iommu-qemu-facts.md` (each fact there is marked read in source, observed
or not established). The body above is not rewritten; this amendment says what it changes, and the assumptions it settles.

1. **Messages with a wrong data word (8.2, 8.4).** What happens depends on the entry kind. In a **basic** entry the device's data
   word is forwarded unchanged to the page the entry names: a word above the file's identities delivers nothing and faults
   nothing; a word below them sets a pending bit in that vector's own file, where the enable bit is clear (the row of 8.4 holds).
   In an **MRIF** entry a data word above 2047 **faults** (cause 263, `iotval` 0), and a word whose enable bit is clear sets the
   pending bit and sends no notice. A check that expects "dropped, not faulted" for MRIF entries is wrong.
2. **Second-stage invalidation (6).** In QEMU an `IOTINVAL.VMA` without a guest address-space id does not flush a cached
   second-stage translation; `IOTINVAL.GVMA` does (the body already names it in 6.2; it is the **only** form the kernel issues
   for a domain's address space, and A5's negative control relies on that). A context rewrite needs `IODIR.INVAL_DDT` as well:
   the device-context cache serves the old context until it is issued (cause 258 afterwards).
3. **Directory modes (2.4).** `ddtp` moves directly between a directory mode and Off or Bare, and from Off or Bare to a directory
   mode. A direct change between two directory modes is **ignored** by the IOMMU: the kernel never writes one (it writes Off
   first). The platform device cannot be made to hand over not Off, so the boot check of 2.4 step 1 has no machine proof; the
   host test against the simulated device is the proof (A17).
4. **The test device (A10, section 13).** `iommu-testdev` writes a fixed word to an I/O address and verifies it at a physical
   address; it cannot read and cannot send a message with a chosen data word. It is the independent oracle for translation,
   permission and invalidation checks (its failure is read from the fault queue). The independent **forger** for interrupt
   checks is a virtio-pci function whose MSI-X table the test programs (the fallback of A10 becomes the rule).
5. **Function-level reset (10.1, 10.2).** The virtio-pci functions of `virt` advertise none (no PCI Express capability): the
   reset is the virtio status-0 write alone, after bus-master enable is cleared. A device that advertises one gets it as before.
6. **MSI-X capacity (8.6).** A virtio-pci function has **two** MSI-X vectors by default (table and pending bits in BAR 1); a
   driver that needs more requests them through the device's `vectors` property at creation, and a class's vector grant (5.3) is
   at most the function's table size.
7. **INTx (A13).** With MSI-X **disabled** the function does raise INTx. Whether the command register's interrupt-disable bit
   silences it is not established; the suite's check of A13 stays, and a driver enables MSI-X before the device is started.
8. **Settled or narrowed.** A5 (a stale entry is served until invalidated) and A9 (no `FEATURES_OK` without
   `VIRTIO_F_ACCESS_PLATFORM`; a device without `iommu_platform` bypasses the IOMMU) hold as observed. A8 holds. A14 (a
   Quarantined device's faults are suppressed by a valid context with reporting off) is untested. A11: the debug translation
   interface exists, but its use is not established; the self-test stays a counted skip until it works. A1, A2 (as to the
   ratified text), A4, A6, A7, A12 and A16 remain as written: nothing in QEMU settles them against hardware.

## Amendment 2026-11-26: bounded work under `IOMMU`, and the invalidation of freed table frames (Q-IOMMU-WORKBOUND, Q-IOMMU-NONLEAF)

Amendment authorized by the owner, 2026-11-26, who answered the two questions the Fable review of IOMMU3 (PR #580) raised:
Q-IOMMU-WORKBOUND option 2 (OD168) and Q-IOMMU-NONLEAF option 1 (OD169). The body above is not rewritten; this amendment says
what it changes. `RANGE_STEP` is the kernel's step for bulk work on a range (`kcore::aspace::RANGE_STEP`, 64 pages), the step the
VMO store already uses.

### 1. Work under `IOMMU` is done in steps (OD168)

11.1 takes `IOMMU` with interrupts off. A range is bounded only by the job's limit on I/O-mapped pages and the window, so one call
could otherwise write or clear about 262,000 leaves with interrupts off. From this amendment **every hold of `IOMMU` is bounded by
`RANGE_STEP` pages of work, not by the size of a mapping or of a domain**:

1. **Map (4.3, 11.2).** `iodomain_map` reserves the I/O range under `IOMMU` and then writes its leaves `RANGE_STEP` pages at a
   time, releasing and retaking `IOMMU` between steps. The range stays **reserved** throughout (4.6: it is never handed out), and
   the mapping's record is linked into the domain's index (**published**) only after the last step, so no other call can name the
   mapping while it is partly written. The invalidation and fence of 4.3 follow the last step. If a table-frame allocation fails
   partway, the roll-back clears the leaves already written in steps of the same size and is then an unmap under 6.1 (as
   amended in part 2 below).
2. **Unmap (4.4, 6.1).** `iodomain_unmap` clears the leaves in steps of `RANGE_STEP` pages, releasing `IOMMU` between steps. The
   range is marked **pending** at the first step and stays pending across the steps. One invalidation and one fence cover the
   whole range, queued in the last step (6.1 step 1 is the sequence of steps, the queueing in the last of them). The table frames
   and the range are freed only after that fence, as 6.1 steps 2 to 4 say.
3. **Destroy against a map in progress (4.5, 6.3).** A stepped map checks, each time it retakes `IOMMU`, whether its domain has
   been marked destroyed. If it has, it rolls back its own leaves (in steps, as in 1) and fails with **BAD_STATE**, the error
   4.3 already gives for a map on a destroyed domain; its record was never published, so the destruction does not see it, and
   its range and hold are released after the roll-back's fence as for any failed map. **A destruction never waits for a map in
   progress.**
4. **The destruction's completion (6.3 step 4, 11.2).** Detaching the domain's table frames and flattening its mapping records
   under `IOMMU` is done in steps of `RANGE_STEP` pages of work too, releasing the lock between steps. Nothing a step leaves
   behind is reachable by the device: the context is already Quarantined and the fence of step 2 has completed.
5. **`vmo_create_dma_in` (4.7).** The new VMO's pages are zeroed in steps of `RANGE_STEP` pages **outside the `VMO` lock**, while
   the VMO is not yet visible (no handle names it), as the other bulk paths of the VMO store do.

Why. A real kernel does not hold an interrupts-off lock across a quarter of a million leaf writes. The reserved and pending states
of 4.6 and 6.1 already make the intermediate states invisible: a partly written map is reachable by nobody but its own call, and a
partly cleared unmap's range is handed out to nobody until its fence. A per-call limit on pages (option 1) would still need steps
below its limit and would push the splitting of a large buffer onto every driver. Taking `IOMMU` with interrupts enabled outside
the fault drain (option 3) would change the lock rules of 11.1, and the lock ordering is not negotiable.

The rows of 11.2 read accordingly: where a path takes `IOMMU` for a map's leaves, an unmap's leaves or the destruction's detach,
it takes it once per step, with the same locks inside (`FRAME`, `HEAP`) and nothing held between steps. The order of 11.1 is
unchanged.

### 2. A table frame freed by an unmap is invalidated for the whole soft context (OD169)

6.1 step 1 asks for "the invalidation that covers every page of the range", and step 4 frees the table frames the unmap emptied.
`IOTINVAL.GVMA` with an address (AV=1) is specified for the leaf translation of that address; an IOMMU that caches non-leaf
entries (a page-walk cache) could keep a pointer into a table frame that a page-wise invalidation did not drop, and the frame,
freed and reused, would then be walked by the device: IO3 for table frames. A rule is added to **6.1**:

* **An unmap, or a map's roll-back, that empties and frees a table frame is invalidated with `IOTINVAL.GVMA` for the whole soft
  context** (the domain's soft-context id, no address), not page by page. A short unmap that frees no table frame may still be
  invalidated page by page.

And a row is added to **15.2**:

| # | Assumed | The design needs it for | Settled by |
|---|---|---|---|
| A18 | `IOTINVAL.GVMA` with an address (AV=1) drops the leaf entries for that address only; the whole-context form (AV=0) drops the non-leaf (page-walk cache) entries of the soft context too | 6.1 (as amended: the whole-context form whenever a table frame is freed); IO3 for table frames | the IOMMU specification's text ("Command queue", the `IOTINVAL` commands); and a page-walk cache added to `iommucore::sim`, so that a mutation of `kcore::iodomain::inval_for` (a page-wise invalidation where a table frame was freed) fails a test. QEMU caches no non-leaf entries, so nothing in the machine settles it (as L4) |

### 3. Destroy against an unmap in progress (OD170)

Amendment authorized by the owner, 2026-11-26 (a gap left by part 1, which settled only a map in progress).

A stepped unmap's mapping record stays in the domain, marked as being unmapped, until the unmap's own fence has completed. If
the unmap, on retaking `IOMMU` between steps, finds its domain marked destroyed, it **stops clearing leaves, leaves its record to
the destruction, and returns OK**. The destruction's completion (6.3) clears whatever leaves remain, its fence covers the whole
domain, and the record's frames and hold are released after that fence, as for every other record of the domain. The unmap's
range is not returned to the allocator on its own: the allocator goes with the domain. **A destruction never waits for an unmap
in progress**, as part 1 point 3 says for a map.

Why OK and not BAD_STATE: what the caller asked for (the device no longer reaches those pages) is guaranteed by the destruction,
and no frame is freed before a fence that covers it. A map is different: its effect does not happen, so it fails (part 1 point 3).

### 4. `iodomain_unmap`'s error order (OD171)

Amendment authorized by the owner, 2026-11-26 (answers Q-IOMMU-UNMAPORDER).

In 4.4 the destroyed-domain check comes **before** the mapping check. The errors of `iodomain_unmap`, in order: BAD_HANDLE;
WRONG_TYPE; ACCESS_DENIED; **BAD_STATE** (the domain was destroyed: its mappings went with it); INVALID_ARGS (not exactly a live
mapping of this domain). As 4.4 read before, a destroyed domain has no live mapping, so INVALID_ARGS always applied first and
BAD_STATE could not be returned. With this order a driver can tell "my domain was destroyed" from "a wrong address", and
`iodomain_unmap` agrees with `iodomain_map`, which already gives BAD_STATE for a destroyed domain. An unmap that finds its domain
destroyed **between its steps** is part 3's case and still returns OK.
