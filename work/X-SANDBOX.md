---
id: X-SANDBOX
kind: topic
title: Sandboxing untrusted code
area: security
horizon: someday
---

A helper that runs a program with a deliberately narrowed capability set, and limits on resource exhaustion and covert channels beyond quotas.

(Not specified, not scheduled, not committed: a direction from the design conversations of 2026-11-14 and 2026-11-15. Needs a spec, an acceptance check and an OD entry before it becomes a slice.)

## Note (2026-11-25): the precondition "an IOMMU" is planned (T42)

Running an untrusted **driver** needs DMA and interrupt isolation, which the system does not have yet: without an IOMMU a driver can point its device at any RAM. That is now specified (`docs/specs/IOMMU-SPEC.md`: a device reaches only the frames mapped into its domain and causes only the interrupts granted on it) and decided (OD148, OD162 to OD166), and planned as the IOMMU epic (state and order: `work/T42.md`). An untrusted driver becomes possible only when **IOMMU12** has merged (virtio-mmio off the allowlist, no physical address learnable from user mode, the hostile-driver matrix green), within the limits the spec states (its section 15.1). The per-job limits on I/O-mapped pages and interrupt vectors (hierarchical, set finite by the supervisor) are the resource-exhaustion part for drivers. Nothing else in this topic is changed by the epic.

## Note (2026-11-27): the precondition "an IOMMU" is met (IOMMU12)

The IOMMU epic's last slice (IOMMU12, `epic/iommu-drivers`) has merged into its epic: every DMA device the kernel admits is a PCI function
behind the IOMMU (`virtio,mmio` is off the device-window allowlist, and its slots and sources are refused), no physical address is
learnable from user mode (`vmo_phys_addr` and the old `vmo_create_dma` are NOT_SUPPORTED library stubs, their numbers retired), and the
hostile-driver matrix (a driver process with exactly a claim's handles, aiming its device and its messages at everything it must not
reach) is in the suite. The precondition is met in the build; it is met on `main` when the epic's smoke is green and the epic merges.
An untrusted driver is possible within the limits the spec states (IOMMU-SPEC 15.1), with finite per-job limits on I/O-mapped pages
and vectors set by its supervisor (`every_driver_job_has_finite_io_limits`).
