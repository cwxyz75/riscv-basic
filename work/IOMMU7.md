---
id: IOMMU7
kind: slice
title: MSI translation: per-vector MSI page-table entries in MRIF mode, vector grants as interrupt objects, forged interrupts fault and are reported
area: drivers
horizon: long
complexity: H
change_risk: H
epic: iommu-kernel
blocked_by: [IOMMU4, IOMMU6]
refs: [T42, OD148, OD163, OD165, OD166, Q-IOMMU-IRQ, Q-IOMMU-MSI, IOMMU-SPEC, INVARIANTS, SYSCALLS, T262, FZ1]
---

Child 7 of the IOMMU epic (`work/T42.md`). **New on 2026-11-25** (Q-IOMMU-IRQ answered B). It gives a device domain its interrupt
side: the kernel owns the device's MSI translation, a supervisor is granted vectors, a driver gets interrupt objects, and a
device told to raise anything else faults in the IOMMU and is reported. Last feature PR on `epic/iommu-kernel`; the epic's smoke
after it is the checkpoint that closes IOMMU2 to IOMMU7. The formats and the simulated device's MSI path are in `iommucore`
(IOMMU2); this slice is the `kcore` model, the calls and the kernel wiring.

## The isolation design (2026-11-25)

**The question:** a driver fully controls its device, and through it (or through the MSI-X table, or a vendor register) the
address and the data word of every message the device sends. What stops it from raising an interrupt it was not given: another
device's, the inter-hart interrupt, an IOMMU fault interrupt?

**What the hardware offers** (RISC-V IOMMU, extended device context: `msiptp`, `msi_addr_mask`, `msi_addr_pattern`):

| Mechanism | What it confines | Verdict |
|---|---|---|
| No MSI page table; the IMSIC page mapped in the device's ordinary page table (what mainline Linux does on RISC-V today: its `drivers/iommu/riscv/iommu.c` has no MSI page-table code, read 2026-11-25) | The page: one hart's file. The data word is the device's: **any identity on that file** | Rejected |
| MSI page table, **basic-translate** entries (mode 3) | The same: QEMU forwards the device's data word unchanged to the file page (`riscv_iommu_msi_write`, `dma_memory_write(..., &data, ...)`, read in 11.1.3) | Rejected as the design, and as a fallback too (Q-IOMMU-MSI 1, OD165: no MRIF, no boot) |
| One **guest interrupt file** per device (`-M virt,aia-guests=N`, basic-translate to that file) | The device, fully, but there are at most 7 guest files per hart on QEMU (observed: the option's range, and the tree's `riscv,guest-index-bits`), taking their interrupts needs the hypervisor extension's machinery (`hgeie`, `hstatus.VGEIN`), and the kernel today refuses an IMSIC with guest files (`kcore/src/aiatree.rs:104`) | Rejected: does not scale, and spends the mechanism a hypervisor would need |
| MSI page table, **MRIF** entries (mode 1): the device's message sets a pending bit in a 512-byte interrupt file **in memory**; if the matching enable bit there is set, the IOMMU sends a **notice** message whose target page (`NPPN`) and identity (`NID`) come from the entry | Everything: the only message that reaches a real interrupt file has an identity and a hart the kernel wrote | **Chosen** |

**The design.**

1. A device domain is translated by the **second stage only** (`iosatp` Bare, `iohgatp` Sv39x4 or wider), so that the MSI check
   is made on the device's address itself, before any page table: the interrupt-file addresses are never in the DMA page table.
   (Read in QEMU: the check runs early only when the first stage is Bare, otherwise on the first stage's output. IOMMU0 item 3
   settles the stage against the specification's text; if the first stage must be used, the design holds with one first-stage
   leaf per vector page and is otherwise unchanged.)
2. Each domain has one frame of **MSI page table** (256 entries of 16 bytes), `msiptp` in flat mode, and a mask and pattern that
   make a fixed 256-page window of **synthetic** I/O addresses "interrupt-file addresses": page `k` of the window is the device's
   vector `k`. The window is not the IMSIC's real address (a driver never learns a physical address: invariant D2) and is reserved
   in the domain's I/O address allocator.
3. **A granted vector `k`** is: a valid MRIF-mode entry at index `k`; its own 512-byte interrupt file in kernel memory (eight to a
   frame) in which only identity 1 is ever enabled; a notice target `(hart, identity)` from the per-hart allocator of IOMMU4; and
   an interrupt object routed from that identity. The supervisor is told the pair to put in MSI-X table entry `k`: address
   `window + k * 4096`, data 1. Neither is a secret or an authority.
4. **What a hostile device can do, and what happens:**
   - a write to a page of the window whose vector was not granted (or beyond the table): the entry is invalid, the IOMMU records
     fault 262 ("MSI PTE not valid") with the device's id, nothing is delivered;
   - a write anywhere else (a real IMSIC page, the APLIC, RAM it was not mapped): an ordinary translation fault, recorded;
   - a write to a granted page with a data word other than 1: a pending bit in that vector's **own** file whose enable bit is
     clear: nothing is delivered. The kernel sees the stray bit when it next looks (at acknowledgement and at revoke) and counts
     it; a data word above 2047 or a misaligned address is fault 263;
   - a write to a granted page with data 1: its own vector fires. That is the grant;
   - a flood of valid messages: see masking, next.
5. **Masking (D5 for a vector) is the file's enable bit**, an atomic memory operation by the kernel from any hart, with no IOMMU
   command and no cross-hart call. Taken: clear enable, read-and-clear pending; signal the object only if pending was set. While
   masked, a message sets pending and sends nothing. Acknowledged: set enable, then read-and-clear pending; if it was set, the
   vector fires again at once from software (the analogue of today's re-pend with `setipnum`). A storming device costs its target
   hart one interrupt per acknowledgement, as a wired source does.
6. **Moving a vector** (hart offline, or the explicit call of Q-IOMMU-MSI 3): take an identity on the new hart (IOMMU4), rewrite
   the entry's `NPPN` and `NID`, invalidate, fence, then retire the old identity on the old hart.
7. **Revoke, detach and destroy, in order:** clear the enable bit; invalidate the entry (V = 0); queue the invalidation and a
   fence; wait for the fence; retire the identity on its hart (IOMMU4's step); only then free the identity, the file and, at
   destroy, the table frame. **An identity is never reused while a cached entry or a message in flight can still name it**: the
   interrupt twin of "no frame is freed before the fence". The supervisor's part (masking the MSI-X table, clearing MSI-X enable
   and bus mastering before it closes the handles) keeps the fault log quiet and is not a safety property.
8. The kernel-owned quarantine context of IOMMU6 has `msiptp` off: a device left running by a dead driver cannot interrupt.

**What is verified, and how** (2026-11-25; no guest code was run):

- *Read in QEMU 11.1.3's source* (`hw/riscv/riscv-iommu.c`, `riscv-iommu-bits.h`, tag `v11.1.3`): basic-translate forwards the
  data word unchanged; MRIF mode is implemented (pending bit, enable test, notice to `NPPN` with `NID`; data above 2047 or a
  misaligned address is cause 263; an invalid entry is cause 262; a file number beyond the table is cause 261); the MSI check
  applies to writes only and runs before translation only when the first stage is Bare; trapped MSI translations are **not
  cached** by QEMU; the device context is the 64-byte format when interrupt remapping is on; `MSI_FLAT` is capability bit 22 and
  `MSI_MRIF` bit 23, both set in the value read from the platform device (`0x78_e2cf_4f10`; also `IGS` = both, `AMO_HWAD` clear).
  QEMU's update of the file is **not atomic** (its own comment); racing the kernel's atomic clear it can re-set a pending bit the
  kernel just cleared, which costs one spurious re-fire and never a lost interrupt (reasoned from the code, not observed).
- *Observed with QEMU's own options and a dumped tree:* `riscv,num-ids = <0xff>` on the supervisor IMSIC, one file page per hart
  at `0x2800_0000`; the PCI host and the IOMMU both have `msi-parent` the supervisor IMSIC; `aia-guests` is 0 to 7;
  `riscv-iommu-pci` has `intremap` (default on).
- *Read in Linux (master, 2026-11-25):* no MSI page-table or MRIF use in the RISC-V IOMMU driver; the IMSIC driver allocates
  identities per CPU and has a move protocol that reads and clears the old identity's pending bit.
- *From memory, to be established by IOMMU0 (items 10 to 14) before the spec states them:* the specification's own text on using
  an MSI page table with the second stage only; which command invalidates a cached MSI entry on real hardware; that a notice is
  sent for every message whose enable bit is set, whatever the pending bit was; that an IOMMU does not cache a file's contents;
  whether the platform device (`iommu-sys=on`) has interrupt remapping on (the capability bits say yes); how virtio-pci sizes its
  MSI-X table; whether the IOMMU test device can send a message. How other systems do it, from memory only: x86 uses a remapping
  table indexed by the message with a source-id check, Arm's ITS a per-device translation table; Fuchsia's kernel allocates
  messages for a driver but, as far as remembered, has no interrupt remapping.

**Honest limits.**

- Identity isolation needs `MSI_MRIF`. The owner made it a boot requirement with no fallback (OD165), because a machine
  without it would give per-hart-file confinement only: a hostile driver could raise any interrupt on the harts its vectors
  target (forgery and storms against other drivers and the kernel's inter-hart interrupt; no memory access). The known
  fallback design (one guest interrupt file per device) is not built.
- On QEMU the notice path is an emulated memory update per message; on hardware MRIF mode is slower than a direct file write.
  That is the price of the isolation; nothing here measures it.
- A stale cached MSI entry cannot be shown on QEMU (it caches none), so "identity freed only after the fence" has **no negative
  control in the machine**: the host model with the simulated device's MSI cache is its only proof. Stated, not hidden.
- QEMU's is the only implementation tested (the epic-wide caveat, IOMMU12).

## Plan (2026-11-25)

### The acceptance check, written and seen to fail before the code

The device is a `virtio-rng-pci` function the suite drives by hand (one queue; a request makes it send a message). The suite
holds the root resource and programs the MSI-X capability and table itself with a small local helper (first written for IOMMU5's
message check), which IOMMU9 replaces by `pcicore`. If IOMMU0 item 14 finds the IOMMU test device can send a chosen message, the forgery rows use it as the independent
device instead.

| Check | Where | What must be seen |
|---|---|---|
| `kcore/tests/iodomain_vectors_model.rs` | host | Random grant, mask, message, acknowledge, move, revoke, destroy, hart offline, against a reference; the simulated device (`iommucore::sim`, with an MSI-entry cache that only invalidation commands drop) sends messages to granted pages, ungranted pages, wrong data words and stale entries. After every step: a real hart file receives a message **iff** the reference says a granted, enabled vector was signalled, and only with the identity the reference holds for it; **no identity is in the free set while any entry, including a cached one, names it**; file, table and identity counts are exact and at baseline at the end |
| `kcore/tests/iodomain_vectors_concurrency.rs` (shuttle) | host | Revoke against a message in flight, move against acknowledge, destroy against the notice interrupt, two harts: the identity-free event is always after the fence-complete and the retire events |
| `msi_granted_vector_is_delivered` | suite, shard `k`, 1, 2 and 4 harts | A domain, a buffer mapped, one vector granted, the MSI-X entry programmed with the pair the kernel returned: the request completes, the interrupt object fires, stays masked until acknowledged while the device sends again (the file's pending bit, D5), fires again after the acknowledgement; delivered on the hart the test asked for (`debug_stat`); zero IOMMU faults |
| `msi_ungranted_vector_faults` | suite, same harts | The MSI-X entry pointed at window page `k + 1` (not granted): the request completes without an interrupt (the test polls the used ring), **one fault record, cause 262, naming this device**; no interrupt object anywhere fires; the no-route counter of IOMMU4 is 0 |
| `msi_forgery_faults` | suite, same harts | The MSI-X entry pointed, in turn, at each hart's real IMSIC page with the inter-hart identity, with another live interrupt object's identity, and with the IOMMU's fault-source identity; at the APLIC; and at a granted page with data 7. Every row: nothing is delivered to anyone (the other object does not fire, the spurious inter-hart counter does not move); the first four give a fault record naming this device; the last gives none and the stray-identity counter rises |
| `msi_revoke_stops_the_vector` | suite | After revoke returns, a message to the same page faults; the identity count is back at baseline |
| negative control `fault-iommu-msi-basic` | kernel feature: entries written in basic-translate mode to the target hart's file | `msi_forgery_faults`' "granted page, wrong data" row must **fail** (the wrong identity arrives on the hart). Proof that the test can tell identity isolation from file confinement. Run once by hand (TESTING.md rule 9) |
| rights and errors | `ksys/tests/iodomain_calls.rs` (extended), `docs/reference/syscall-tests.tsv` | Every error of the new calls in SYSCALLS.md's order: no grant right on the domain handle; index beyond the table; a second grant of a live index; over the job's vector limit, and over a **parent** job's vector limit from a child job that has none of its own; no identity left; the domain handle a driver gets (IO_MAP and IO_UNMAP) cannot grant, and its interrupt handle cannot revoke another's vector or learn the pair. MSI faults of a live domain arrive in its fault ring (IOMMU6) |
| fuzz, allocation failure, leak oracles | `ksys/tests/fuzz.rs`, `allocfail.rs`, `kfuzz-gen`, the suite's end-of-session checks | The calls are in the generators; grant fails cleanly at every allocation point; revoke and destroy never fail on memory; zero live vectors and the identity count at baseline at the end |

### What is built

1. **`kcore/src/iodomain.rs`** (extending IOMMU3's store): per domain the MSI table frame, the vector table (index, file, route,
   state: `Granted`, `Masked`, `Moving`, `Revoking`), sized at creation; the operations of the design in phases like unmap's
   (clear, queue, fence, wait, retire, free); the job charge against the per-job **interrupt vectors** limit (Q-IOMMU-MSI 4).
   The limit's field, its usage counter and its hierarchical rule are IOMMU3's and its call is IOMMU6's
   (`job_set_io_limits`); this slice makes the first charge against it and extends the inheritance test to a real grant (a
   child job is refused a vector at its parent's limit).
2. **ABI and calls** (`abi`, `libsys`, `rt`, `kfuzz-gen`, the shuffled tables of AB8; names fixed by IOMMU-SPEC 8): grant a vector
   on a domain handle (a new right bit on the domain, not `MANAGE`: OD159), returning an **interrupt handle** and the pair for the
   MSI-X entry; revoke is the interrupt object's last handle or the domain's destruction; `interrupt_deliver_here`
   (Q-IOMMU-MSI 3, IOMMU-SPEC 8.5). `interrupt_wait` and `interrupt_ack` are unchanged for the holder. The interrupt
   object gains a kind (wired source, or vector of a domain); a vector object keeps the domain's **record** alive but does not
   keep the device attached (IOMMU-SPEC 4.5). The grant needs IO_GRANT, which only the supervisor's control handle has: a
   driver's duplicate (IO_MAP and IO_UNMAP) cannot grant.
3. **Kernel** (`kernel/src/arch/riscv64/iommu.rs`, `aia.rs`, `user/`): the context of a domain is written with `msiptp`, mask and
   pattern from the start (IOMMU6 writes it empty); entry writes and their invalidation through the command queue; the notice
   interrupt arrives as a `Vector` route (IOMMU4) and runs the mask protocol with atomic operations on the file through the
   direct map (every `unsafe` block with its `// SAFETY:`); hart offline moves vectors (`thread.rs:542`'s path); the fault path
   names MSI causes 261 to 264 and 273 in the console line; the stray-identity counter and the vector statistics in `debug_stat`.
4. **Boot requirement**: `capabilities.MSI_FLAT` and `MSI_MRIF` are among the capabilities IOMMU5 insists on (OD165): IOMMU5
   already refuses the boot without them and this slice only uses them.
5. **Reference documents in place**: SYSCALLS.md (the calls, rights, errors, statistics); INVARIANTS.md: new rows (IO9: a device
   can cause only the interrupts of vectors granted on its domain; IO10: an identity is freed only after the fence that follows
   its entry's invalidation and after its hart retired it; IO11: no MSI entry is ever written in basic-translate mode), D5
   reworded to cover a vector ("masked" is the file's enable bit), M3's oracle includes vectors and identities.

### Invariants, locks, SMP, error paths

- **Lock order.** Grant, move and revoke take `IOMMU` (entry write, command, fence) after `OBJ`, as map and unmap do. The notice
  path (interrupts off, under `OBJ`) takes **no** lock for masking: the file is touched only with atomic operations. Never
  `IOMMU` in the interrupt path for a vector.
- **SMP.** The file is written by the IOMMU and by any hart: every kernel access is an atomic read-modify-write. A notice that
  arrives after software already took the pending bit finds it clear and does nothing (exactly-once by the pending bit, not by
  the notice).
- **A killed driver**: its interrupt handles close; the vector objects go only when the supervisor's handles go too, or the
  domain is destroyed, which is what the supervisor then does (IOMMU-SPEC 10.2: the domain is destroyed and made anew at the
  re-claim, so vectors are granted afresh for the next driver; OD166 point 2). A killed supervisor: the domain's destruction revokes every vector in the order of design step 7.
- **Headless diagnosis**: every forged message is a fault line with the device id; a lost interrupt shows as a request that
  completes only by the driver's safety poll, counted by the suite; an identity leak fails the end-of-session count.

### Acceptance

Everything in the table passes in the epic's smoke at 1, 2 and 4 harts (say plainly which ran); the negative control fails as
stated; the feature gate is green. **Fable review** on its own PR (kernel and security slice) against IOMMU-SPEC 8 and 10,
SYSCALLS.md and INVARIANTS.md D5, IO9 to IO11, the lock table: the reviewer looks for any message a device can cause that is not
one of its granted vectors, and any order in which an identity is reused early. Then merge `epic/iommu-kernel` into `main` on a
green smoke and close IOMMU2 to IOMMU7 in the closing commit.

PR size: large (about 900 lines of `kcore`, `ksys`, kernel and ABI, 1,000 of tests). Shortcuts become tidy items in the PR
(expected: a fixed 256-vector table; no superpage of files; the stray-identity sweep only at acknowledgement and revoke).
