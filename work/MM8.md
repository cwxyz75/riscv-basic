---
id: MM8
kind: slice
title: Copy-on-write children: `vmo_create_child` (SNAPSHOT, PRIVATE) and `process_clone_region`
area: kernel
horizon: long
blocked_by: [MM7]
refs: [KERNEL-V1-SPEC, OD133]
---

**How.** KERNEL-V1-SPEC 4.3, in a new pure module `kcore::vmofamily` used by `Vmos`: a family is a tree of nodes under one lock; a node has its own page index, a parent link with an offset window, and a kind (visible leaf, hidden). Lookup walks leaf to root within the window. SNAPSHOT: requires `job ==` the source's job (else ACCESS_DENIED); the source's pages move to a new hidden node, source and child hang under it. PRIVATE: the child hangs under the source directly; the source stays writable and visible. Write fault on a page not owned by the faulting leaf: if no other leaf can see the ancestor's page, move it; else allocate, copy, charge the leaf's job. Read fault: map the ancestor's page read-only with the software COW bit; a later write fault resolves as above and replaces the PTE (flush that page on every hart that ran the address space). A write by a PRIVATE parent to its own page that children still see needs no copy (PRIVATE children see later changes by definition). When a leaf dies and a hidden node has one child left, merge. Depth limit 64. `vmo_decommit` on a member frees only that member's own pages. `process_clone_region(source, target, vaddr, mode)`: MANAGE on both; target must not have a mapping there; SHARE maps the same VMO with the same permissions; SNAPSHOT makes a SNAPSHOT child (same job required: a `fork` child runs in its parent's job) and maps it.

**Acceptance, written first.** `kcore/tests/vmofamily_model.rs`: random trees of snapshots and private children with reads, writes, decommits and deaths against a reference that keeps a full byte copy per visible member (COW1), with the page count and the per-job charge checked by formula after every step (COW2), mutation-checked (copy skipped; merge to the wrong child; charge to the wrong job). Loom: two leaves write-faulting the same shared page. QEMU: a program snapshots a region into a child process built by hand, both write, each sees its own data; frame counts return to baseline when both end; the negative control that skips write-protecting the source's existing mappings at snapshot time must fail.

**Review.** Fable, soak. The riskiest memory item: its own PR, nothing else in it.
