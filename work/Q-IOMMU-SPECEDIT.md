---
id: Q-IOMMU-SPECEDIT
kind: decision
title: Permission to write the IOMMU spec and the dated amendments under docs/specs
area: drivers
horizon: long
refs: [T42, IOMMU1, OD148]
---

Agents may not touch `docs/specs/` without the owner's explicit permission (CLAUDE.md). OD148 ordered "the IOMMU spec and the OD
amendments" as the first step, so IOMMU1 cannot start until the owner says yes to exactly this list (or strikes rows):

| File | Change |
|---|---|
| a new file under `docs/specs/`, to be named IOMMU-SPEC | **New file**: the DMA-isolation spec (contents: `work/IOMMU1.md`) |
| `docs/specs/KERNEL-V1-SPEC.md` | Dated amendment: section 1's non-goal "an IOMMU" (line 32) is withdrawn; 2 (platform) and 2.3 (discovery) gain the required `riscv,iommu` node; line 350 ("DMA frames follow the policy of S6-SPEC") replaced by a pointer to IOMMU-SPEC; the limits row "DMA pages, whole system" (line 108) per the answer to Q-IOMMU-ABI; line 534 (the PCI ECAM window is not mappable) reversed; section 9 (the freeze) lists the new calls |
| `docs/specs/OS-ARCHITECTURE.md` | Dated amendment: lines 493 to 497 ("DMA is the honest weak spot") and 708; line 212 (virtio-mmio first) |
| `docs/specs/S6-SPEC.md` | Dated amendment: lines 15 and 16 (no IOMMU, no virtio-pci), 55 to 60 (DMA VMOs), 104 to 106 (kill safety by supervisor policy) |
| `docs/specs/NET-DESIGN.md`, `NET-SPEC.md` | Dated amendment: NET-DESIGN 28 to 30, 216 and 217, 250 and 251 (the DMA right and physical addresses); NET-SPEC 4.3 and 5.1 where they name `vmo_phys_addr` and the mmio slot |
| `docs/specs/SCHED-SMP-SPEC.md` | Dated amendment: 3.3 gains the lock class for the IOMMU and the reason for its position |

Question: **may IOMMU1 create the IOMMU-SPEC file and append these amendments?** (Recommended: yes, with the draft shown to the owner
as a PR before it merges.) The body of an existing spec is never rewritten: amendments are appended and dated.

Source: T42 re-planning, 2026-11-25.

## Note (2026-11-25): more rows after Q-IOMMU-IRQ was answered B

MSI-X with MSI remapping is now in the epic, so the yes asked for above would also have to cover:

| File | Change |
|---|---|
| `docs/specs/KERNEL-V1-SPEC.md` | The same dated amendment also covers 2.3 (the IMSIC's `riscv,num-ids` is read) and 2.4 (interrupt identities are allocated by the kernel; device interrupts are MSI-X through the IOMMU's MSI translation; `MSI_MRIF` required if Q-IOMMU-MSI 1 says so) |
| `docs/specs/S6-SPEC.md` | The same amendment also covers the interrupt-objects paragraph (line 63): a second kind of interrupt object, the vector of a device domain |
| `docs/specs/OS-ARCHITECTURE.md` | The same amendment also covers the interrupt-object bullet (line 485) |

## Answer (owner, 2026-11-25)

**Yes, to exactly this list**: the new file `docs/specs/IOMMU-SPEC.md` and dated amendments appended at the end of KERNEL-V1-SPEC,
OS-ARCHITECTURE, S6-SPEC, NET-DESIGN, NET-SPEC and SCHED-SMP-SPEC, covering the rows of both tables above (the "more rows" note
included). Nothing else under `docs/specs/` is covered, and the body of an existing spec is not rewritten. The draft goes to the
owner as a PR before it merges.
