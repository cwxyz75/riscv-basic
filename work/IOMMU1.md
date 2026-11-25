---
id: IOMMU1
kind: slice
title: Write the IOMMU and DMA-isolation spec and the dated amendments (drafted; open until the owner merges the PR)
area: drivers
horizon: long
complexity: H
change_risk: L
blocked_by: [IOMMU0]
refs: [T42, OD148, OD136, OD14, OD162, OD163, OD164, OD165, OD166, OD167, IOMMU-SPEC, KERNEL-V1-SPEC, OS-ARCHITECTURE, S6-SPEC, SCHED-SMP-SPEC]
---

Child 1 of the IOMMU epic (`work/T42.md`), and OD148's step (a). The owner's yes was given (Q-IOMMU-SPECEDIT, OD167): it
creates the new spec `docs/specs/IOMMU-SPEC.md` and appends dated amendments to six specs. Nothing else
in the epic that the owner's choices shape (IOMMU3 onwards) starts before this merges. The section list below is the brief the
spec was written from, kept in step with the owner's decisions; the spec itself is the contract.

**Revised 2026-11-25** after Q-IOMMU-IRQ was answered B (MSI-X with MSI remapping in this epic): sections 2, 3, 5, 6, 8, 9, 10, 12
and 14 below changed, and Q-IOMMU-MSI joined the blockers. The design the spec writes down for interrupts is in IOMMU7 ("The
isolation design"); the spec states it as the end state and cites the specification sections IOMMU0 items 10 to 14 establish.

## Plan (2026-11-25)

### Deliverable

One documentation PR to `main` (documentation-only changes go straight to `main`): the new spec, the amendments, an OD entry in
`docs/records/decisions.md` for each choice the spec makes that OD148 did not (the answers to the four other decision items become
ODs), and the planned rows for `docs/reference/INVARIANTS.md` and `SYSCALLS.md` **quoted inside the spec** (the reference documents
describe the system as it is and change with the code, in IOMMU4 to IOMMU7, not here). The spec describes the end state only: no
build order, no "not built yet" (that is `work/`).

### What the new spec (IOMMU-SPEC) must say, section by section

1. **Goal and threat model.** A device reaches exactly the frames mapped into its domain, with the access (read, write) that was
   granted, and nothing else: not kernel memory, not another process's, not another device's buffers or registers, not an interrupt
   controller. **A device causes exactly the interrupts of the vectors granted on its domain, and no other**: not another
   device's, not the inter-hart interrupt, not the kernel's own sources. The adversary is a driver process that fully controls
   its device's registers, including its MSI-X table if the device lets it. Out of scope, named: a hostile device
   that lies about its requester id, denial of service by a driver against its own device, side channels.
2. **Platform requirement.** The device the profile requires (Q-IOMMU-MODEL 1), how it is discovered (the `riscv,iommu` node, the
   PCI host's `iommu-map`), the capabilities the kernel insists on (version, the translation mode it uses, the queue sizes, wired
   interrupts for the IOMMU itself, the 64-byte device context with `MSI_FLAT`, and `MSI_MRIF` per Q-IOMMU-MSI 1) and the exact boot refusals with their console lines (no node; a capability missing; the device does not accept the
   mode). The state at hand-over: Off is expected; an IOMMU handed over in any other mode is written Off, read back and then brought
   up as usual, and only one that will not go Off refuses the boot (OD166 point 11). The rule that the kernel **never** writes a
   pass-through state: not `ddtp` Bare, not a device context with both stages Bare (writing Off is not one). Which devices are DMA-capable and therefore must be behind it, and what happens to those
   that cannot be (fw-cfg's DMA interface and the virtio-mmio slots: never a device window).
3. **Address-space model.** A domain per device (Q-IOMMU-MODEL 2): what a domain is (a kernel object, its handle rights, one live
   per device id, who may create one: the root resource), its I/O address space (the translation mode and width, 4 KiB pages
   only or not, the reserved ranges: page 0, anything the device must never see), first-stage against second-stage tables and why (the plan recommends **second stage only**, so the MSI
   address check is made on the device's own address before any page table: IOMMU7 design step 1, IOMMU0 items 3 and 10), the
   window of synthetic interrupt-file addresses reserved in every domain,
   the A and D bits (the QEMU device does not set them), the default for every device id that has no domain (deny).
4. **Mapping.** The calls (names, arguments, rights, errors in order, in the style of SYSCALLS.md): create a domain for a device
   id; map a range of a VMO into it with read or read-write access; unmap; destroy (by a call with a right of its own, and by the
   last handle). **The rights split** (Q-IOMMU-MODEL 3): the driver holds a domain handle with the map and unmap rights only and
   maps its own buffers; the supervisor keeps the control rights (destroy, grant vectors, read faults). Who may map what: the
   rights needed on the domain handle and on the VMO handle (the `DMA` right's new meaning, Q-IOMMU-ABI 1), which VMO kinds are
   mappable (pinned ones only: a DMA VMO; never a device VMO, a lazily committed, pager-backed, resizable or copy-on-write VMO),
   access never wider than the VMO handle's. A mapping has a record allocated at map time; there is no fixed number of mappings
   per domain (OD166 point 5). **I/O address allocation** (Q-IOMMU-MODEL 4): the algorithm's guarantees, not its
   code (never 0, never identity, no reuse of an address before its invalidation completed, deterministic or not).
5. **Lifetime, pinning and charging.** A mapping holds its VMO (an extension of invariant M8: a VMO lives while any handle, any
   region **or any device mapping** holds it), so frames are never freed while a device can reach them, whoever dies in whatever
   order. What is charged to whom: the VMO's pages (Q-IOMMU-ABI 4), the domain's table frames (kernel bytes of the creator's
   job), a per-job limit on I/O-mapped pages, the system backstop; the MSI page table and the interrupt files in memory (kernel bytes
   of the same job) and the per-job limit on interrupt vectors (Q-IOMMU-MSI 4); both limits hierarchical, with no default, and
   set finite by the supervisor on driver jobs (OD166 point 4). The new call `vmo_create_dma_in`, and what the two-argument
   `vmo_create_dma` and `vmo_phys_addr` become: library stubs that return NOT_SUPPORTED (Q-IOMMU-ABI 1 and 3, OD166 point 9).
6. **Invalidation protocol** (the heart; the analogue of TLB2 and SMP5). Unmap, in order: clear the leaf, queue the invalidation
   for that device and range, queue a fence that also orders earlier device reads and writes, **wait for the fence**, and only then
   drop the VMO hold (which may free frames). Detach and destroy: point the device context at the deny (or quarantine) state,
   invalidate the device-context cache, invalidate the translations, fence, then free the domain's tables and drop every hold. A
   device context is changed only invalid-to-valid or through the deny state, never in place. What happens when a fence does not
   complete in the stated bound: the frames are **leaked and counted, never freed**, and the machine says so. **The same protocol for an MSI entry**: revoke or move clears or rewrites the entry, queues its
   invalidation and a fence, waits, and only then lets the interrupt identity be retired on its hart and reused; an identity whose
   fence did not complete is leaked and counted, never reused. That the command
   queue is one shared resource: every hart submits under one lock; no per-hart shootdown exists because the IOMMU is one device.
7. **Faults.** The fault queue: which records exist, the console line's format (cause, device id, I/O address, read or write),
   rate limiting, the overflow bit, the counters (`debug_stat` selectors), delivery to the supervisor through a ring of 16 records
   and a dropped count per domain (Q-IOMMU-MODEL 5, OD166 point 8), what
   the kernel does **not** do (reset or kill). The quarantine state for a device whose domain is gone (Q-IOMMU-MODEL 6).
8. **Interrupts.** MSI-X with MSI remapping (Q-IOMMU-IRQ: B; Q-IOMMU-MSI). The spec states:
   - **Identities**: every interrupt on a hart's file is an identity the kernel allocated (per hart, Q-IOMMU-MSI 2) with one
     route (inter-hart, kernel-owned source, wired source, vector); how many exist (`riscv,num-ids`); the retire rule on a move.
   - **MSI translation**: `msiptp` in flat mode, the mask and pattern, one entry per vector, **MRIF mode only, never
     basic-translate**; what the device's message does (a pending bit in the vector's own file) and what reaches a hart (the
     notice, with the kernel's identity and target); the fault for each kind of forged message (an ungranted vector page, an
     address outside the window, a wrong data word) and that none is delivered.
   - **Vector grants and interrupt-object rights**: who may grant (the domain handle with the grant right: the supervisor), what
     the grant returns (an interrupt handle; the address and data for the MSI-X entry, which are neither secret nor authority),
     what a driver holds (wait and acknowledge only), the kinds of interrupt object (wired source, vector) and that a vector
     object keeps its domain alive; limits (per device, per job).
   - **Masking** (D5 for a vector): the enable bit of the vector's file; the acknowledgement protocol and its exactly-once
     argument; storms.
   - **Placement**: where a vector is delivered and when it moves (Q-IOMMU-MSI 3); hart offline.
   - **Wired sources**: PCI INTx is never admitted and is disabled in every claimed device; a DMA device without MSI-X is not
     claimable (Q-IOMMU-MSI 5). The IOMMU's own interrupts are wired, kernel-owned sources (Q-IOMMU-IRQ 3) and are retargeted
     when a hart goes offline.
   - **The threat-model limit, stated**: identity isolation rests on `MSI_MRIF`, which is a boot requirement; what an IOMMU
     without it would give (file confinement only); that guest interrupt files are not used and are left for a hypervisor.
   - ATS, PRI and process contexts (PASID): never enabled, stated.
9. **Bus and device manager.** What the kernel knows about PCI (nothing but the tree's `iommu-map` and the host's windows as
   device windows for the root resource) and what `devmgr` does (enumeration, BAR assignment since no firmware does it, capability
   walk, the MSI-X capability: table and pending-bit BARs, programming the table from the kernel's grants, MSI-X enable and INTx
   disable; bus-master enable last, after the domain exists, the vectors are granted and the driver has its handles). That a driver
   never sees configuration space, and never the MSI-X table where the device keeps it in a BAR of its own.
   The claim protocol's new shape (`proto::devmgr`): what a driver is handed (register BAR VMO, a domain handle with the map and
   unmap rights only, its DMA VMOs with the `DMA` right, one interrupt handle per vector); it maps its buffers itself and learns
   the I/O addresses from the map call.
10. **Device reset and teardown ordering**, written as numbered sequences for: a clean driver exit, a killed driver, a killed
    supervisor, a re-claim, system shutdown. A killed driver's domain is destroyed and made anew at the re-claim: mappings and
    grants are not kept across driver lifetimes (OD166 point 2). For each: who resets the device (the supervisor, for function, no
    longer for safety: bus-master enable cleared and a function-level reset before the domain is torn down),
    when bus mastering goes off, when the domain goes, when frames return. **For interrupts, in this order: the MSI-X table is
    masked and MSI-X disabled (supervisor, for quiet); the vector's entry is invalidated and the device's translations and MSI
    entries are fenced (kernel); the domain's mappings are unmapped; only then are interrupt identities retired and freed and the
    files and table returned.** Which steps are the kernel's guarantee (all but the first) and which are the supervisor's
    manners. The T42 hazard stated as closed by construction.
11. **Locking.** The new lock class, its place in the order of SCHED-SMP-SPEC 3.3 and INVARIANTS.md C1 and K5 and why (proposal
    for the spec to confirm or change: after `KSTACK` and before `FRAME`, so a path holding `VMO` or `JOBS` may take it and it may
    take `FRAME`, `HEAP` and `UART`; taken by the fault interrupt handler; never held across a wait, the fence poll included
    (OD166 point 6); nothing that takes `OBJ`, an `AS` lock or a thread call runs under it). Unmap and destroy never allocate (the T177 rule:
    removal is class R-free).
12. **Invariants** (new rows IO1 to IO12; IO9: a device causes only the interrupts of vectors granted on its
    domain; IO10: an identity is freed only after the fence that follows its entry's invalidation and after its hart retired it;
    IO11: no MSI entry is ever written in basic-translate mode; and the identity row of IOMMU4). IO1 to IO8: default deny; no pass-through state is ever written; a frame mapped for a
    device is freed only after the fence that follows its invalidation; a mapping's access is within the VMO handle's rights; one
    live domain per device id; a physical address is not learnable from user mode (D2 restated); I/O mapped pages and table frames
    are charged and credited exactly once; a fence timeout leaks, never frees. Changes to D2, D4, D5, M8, H6, M3, J1 and J2.
13. **Testing regime** (what IOMMU2 to IOMMU12 build): the host models and their oracles, the negative-control kernels, the QEMU
    checks and the hart counts they run at, the independent device (`iommu-testdev`, or the fallback IOMMU0 names).
14. **Non-goals:** nested translation for guests, ATS, PRI, PASID, superpage I/O mappings, hotplug, an IOMMU other than the
    RISC-V one, PCI INTx, plain MSI (the older capability: MSI-X only), guest interrupt files, and basic-translate MSI entries.

### The owner's decisions

All answered on 2026-11-25: **Q-IOMMU-SPECEDIT** (OD167), **Q-IOMMU-ABI** (OD162), **Q-IOMMU-IRQ** (OD163), **Q-IOMMU-MODEL**
(OD164), **Q-IOMMU-MSI** (OD165), and the twelve further choices the draft had to make (OD166). The spec states each.

### Acceptance

The PR merges when: the owner has read and accepted the draft (the PR is the review); `make docs-check` passes (`check_docs.py`
knows the spec list in `docs/README.md`: add the new file there); every section above is present; every statement about the device
cites the RISC-V IOMMU specification section or the IOMMU0 record; no section says what is built. A fresh-context review pass
(default reviewer; Fable if the owner wants one on the protocol of section 6) attacks sections 4, 5, 6, 8, 10 and 11 against
INVARIANTS.md and looks for an ordering in which a frame is freed while a translation to it can still be used, or an interrupt
identity reused while an entry can still name it, and for any message a device can cause that is not one of its vectors.

### Hazards

- Writing the spec to fit what is easy (CLAUDE.md forbids it): the unmap-wait-free rule and "no pass-through" are OD148's point and
  are not to be softened; if one cannot be built on QEMU's device, stop and record the blocker.
- Restating the RISC-V IOMMU specification instead of citing it: the spec says which options this system uses, not what the
  formats are.
- A lock position chosen on paper that the checker (`klock`) then refuses in IOMMU6: have the nesting list of section 11 name
  every path (map, unmap, destroy, fault interrupt, VMO release, process teardown) and what each already holds.

- **Amendments beyond Q-IOMMU-SPECEDIT's first table**: KERNEL-V1-SPEC 2.3 and 2.4 (identities from `riscv,num-ids`, MSI-X as
  the device interrupt model, the `MSI_MRIF` requirement), S6-SPEC's interrupt objects paragraph, OS-ARCHITECTURE's
  interrupt-object bullet, and the passages found while drafting (KERNEL-V1-SPEC 4.1; NET-SPEC 3.1, 5.2, N2, Q16;
  OS-ARCHITECTURE's device-memory bullet). The owner's yes covers them (OD167); nothing else under `docs/specs/` is covered.

PR size: one new spec (about 500 to 750 lines) and the short amendments. Not Fable by default (no code).

## State (2026-11-25): drafted, the owner's decisions are in it; open until the owner merges the PR

All five decisions are answered and recorded: OD162 (Q-IOMMU-ABI), OD163 (Q-IOMMU-IRQ), OD164 (Q-IOMMU-MODEL), OD165
(Q-IOMMU-MSI), OD167 (Q-IOMMU-SPECEDIT: the permission). In the working tree: `docs/specs/IOMMU-SPEC.md` (sections 1 to 14 as
listed above, plus 15 "Limits and assumptions" and 16 "Choices made beyond the recorded decisions"), one dated amendment at the
end of each of KERNEL-V1-SPEC, OS-ARCHITECTURE, S6-SPEC, NET-DESIGN, NET-SPEC and SCHED-SMP-SPEC, and the spec's row in
`docs/README.md`.

Done since the first draft:

- **The owner decided the twelve choices of section 16** (OD166): eight as drafted (one on a condition: the job limits are
  hierarchical and a supervisor sets finite ones on driver jobs), four changed (a record per mapping in place of a 64-entry
  table; a ring of 16 fault records; `vmo_create_dma_in` as a new call with the old call a stub; an IOMMU handed over in
  another mode is turned Off, not refused). The spec states the results.
- **The OD entries** OD162 to OD167 are in `docs/records/decisions.md`.
- **The passages outside the first permission** are amended (the same dated amendments, extended): KERNEL-V1-SPEC 4.1;
  NET-SPEC 3.1, 4.1, 4.2, 5.2, N2's check and Q16; OS-ARCHITECTURE's device-memory bullet.
- **The tickets** IOMMU3 to IOMMU12 were rewritten to the rights split and to the decisions above.

What remains before this item can close:

- **The owner reads and merges the PR.** The five Q-IOMMU items close with it, not before.
- **IOMMU0 is still open and has no record.** Every device fact it was to establish is in the spec as a labelled assumption
  (A1 to A17) tied to a boot check or a named test, and the spec cites the RISC-V IOMMU and AIA specifications by the title of
  the part, not by section number: the acceptance line "every statement about the device cites the specification section or
  the IOMMU0 record" is met only that far. When IOMMU0's record exists, an amendment of the spec turns each assumption it
  settles into a citation (that amendment needs the owner's permission like any other).
- **The fresh-context review** of sections 4, 5, 6, 8, 10 and 11 named under Acceptance.
