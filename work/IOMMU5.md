---
id: IOMMU5
kind: slice
title: Kernel IOMMU bring-up: required at boot, default deny for every device, faults reported, the PCI windows opened
area: drivers
horizon: long
complexity: H
change_risk: H
epic: iommu-kernel
blocked_by: [IOMMU2, IOMMU4]
refs: [T42, OD148, OD47, OD127, OD165, OD166, IOMMU-SPEC, INVARIANTS, T262, T274, Q-IOMMU-IRQ, Q-IOMMU-MSI]
---

Child 5 of the IOMMU epic (`work/T42.md`), and OD148's step (c): "a QEMU boot with the IOMMU present and the kernel driving it,
with no driver moved yet". Feature PR on `epic/iommu-kernel`. After it the kernel refuses to boot without the IOMMU, every PCI
device's DMA is refused and reported, and the virtio-mmio drivers work exactly as before (those slots are not behind the IOMMU:
nothing is isolated yet, and nothing is worse).

**Revised 2026-11-25** (Q-IOMMU-IRQ answered B; this was IOMMU4 before the renumbering): it now comes after IOMMU4 (interrupt
identities), which builds the kernel-owned-source hook and the identity allocator this slice uses; the capability check gains
`MSI_FLAT` and `MSI_MRIF`; nothing here assumes ids below 64 or a shared hook with T14 any more.

## Plan (2026-11-25)

### The acceptance check, written and seen to fail before the code

Step 1 of the PR is the check alone: the machine option, the test device, the suite tests and the script's assertions, on the
unchanged kernel, with **one targeted boot** (`make kquick`, which is `-smp 4`) to watch them fail.

| Check | Where | What must be seen |
|---|---|---|
| `iommu_is_on_and_denies_by_default` | suite, shard `k` (kernel-only boots: 1, 2 and 4 harts) | With the root resource: map the ECAM window and the test device's BAR, assign the BAR, enable bus mastering, command a DMA **write** to the physical address of a pattern-filled frame the test owns, and a **read** of it. The device reports failure both times; the pattern is intact; the fault counters (`debug_stat`, new selectors) rose by two; the last fault record names the test device's id, the address, and read or write. The device is `iommu-testdev` if IOMMU0 says it can do this, otherwise a virtio-blk-pci device given one descriptor outside any mapping |
| `iommu_denies_messages_by_default` | suite, shard `k`, 1, 2 and 4 harts | A virtio-pci function with no domain, its MSI-X table programmed by the test (a small local helper, as in IOMMU7) with a hart's real IMSIC page and the inter-hart identity, then made to interrupt: nothing arrives (the spurious inter-hart counter does not move), one fault record names the device. Fails today for want of the IOMMU; after this slice it passes because the directory is empty |
| the console line | `scripts/os_boot_test.py` | `## iommu: fault cause=<n> dev=<bb:dd.f> iova=0x... write` (format fixed by IOMMU-SPEC 7) appears, with the device id the script itself put on the command line; and the boot line `kernel: iommu ...` with the capabilities |
| `iommu_translation_matches_the_tables` | kernel self-test (`kernel/src/selftest.rs`, test builds only) | If the debug translation interface works (IOMMU0 item 7): a scratch context and table, then ask the device to translate mapped and unmapped addresses; it agrees with `iommucore`. An independent check of the table formats by QEMU's walker. Skipped loudly (a counted skip the script refuses) if the interface is absent |
| the hand-over state (IOMMU-SPEC 2.4 step 1, OD166 point 11) | host: the bring-up sequence (the part of the driver that is pure, over `iommucore::Regs`) against `iommucore::sim` handed over in each mode (Off, Bare, each directory mode, queues on or off) | Handed over Off: `ddtp` is written once, with the directory mode. Handed over otherwise: the **first** write to `ddtp` is Off, the sequence waits for the busy bit and reads Off back, live queues are disabled and read back, then the normal bring-up; **Bare is never written** (the simulator's write log); a simulated device that never reads back Off ends in the refusal `IOMMU: will not go off`. In the machine: one boot with the platform device handed over not Off, if IOMMU0 item 15 finds QEMU can do that (the boot line then carries `handover=<mode>` and the default-deny tests pass); if it cannot, the host test is the only proof and the PR says so |
| `os-boot-test-iommu-neg` | new Makefile target, as `os-boot-test-cpu-neg` (`Makefile:467`) | The same kernel on `-machine virt,aia=aplic-imsic` (no `iommu-sys=on`) stops with a panic line containing "no IOMMU"; `--expect-panic` |
| negative control `fault-iommu-bare` | a kernel feature (`kernel/Cargo.toml`, next to `fault-skip-shootdown`) that programs `ddtp` Bare | `iommu_is_on_and_denies_by_default` must **fail** on it: proof that the test can tell isolation from none. Run once by hand when the test is written (TESTING.md rule 9), listed with the other negative controls |
| the whole existing suite | epic smoke | Unchanged and green: the virtio-mmio drivers still run |

Harts: the kernel-only boots at 1, 2 and 4 harts (the icount run has one hart, T21). Say in the epic's closing commit which ran.

### What is built

1. **Discovery** (`kcore/src/fdt.rs` or a new `kcore/src/iommutree.rs`, host-tested against a `dtc` dump of the real tree, like
   `kcore/tests/aiatree.rs` and `devwin.rs`): the `riscv,iommu` node's `reg` and interrupts, and the PCI host's `iommu-map`
   (device-id range and base). Hostile-input safe like the rest of the reader. Fixture: the tree dumped on 2026-11-25 with
   `iommu-sys=on` (node `iommu@3010000`, sources 36 to 39, `iommu-map = <0 &iommu 0 0x10000>`).
2. **Refusal** (`kernel/src/dtb.rs`, beside the extension check at `:86`): no node, or `iommucore::regs::Caps::check` fails, or the
   device does not accept the mode written (read back `ddtp`), or an IOMMU handed over in a mode other than Off does not go
   Off: a boot-time panic with a fixed text (the lines of IOMMU-SPEC 2.5). **A hand-over mode other than Off is not itself a
   refusal** (OD166 point 11). The required set includes
   `MSI_FLAT` (the 64-byte device context) and, per Q-IOMMU-MSI 1, `MSI_MRIF`: "no MRIF" is its own refusal line, and
   `os-boot-test-iommu-neg` gets a second case on a machine whose IOMMU lacks it if QEMU can make one (IOMMU0 item 13 finds the
   property; if it cannot be switched off on the platform device, the host test of `Caps::check` is the only proof, said plainly). `kcore::isa` is for hart
   extensions and stays as it is.
3. **The driver** (`kernel/src/arch/riscv64/iommu.rs`; every `unsafe` block with its `// SAFETY:`): map the register page (add it
   to the kernel's driven ranges in `kmem.rs` near `:484`, so it is cut from every device window, like the console and the AIA);
   implement `iommucore::Regs` over volatile MMIO and `PhysMem64` over the direct map; allocate the directory root, the command
   queue and the fault queue from boot-time frames (never freed). **First, the hand-over state** (IOMMU-SPEC 2.4 step 1): read
   `ddtp`; if it is not Off, write Off (which denies every transaction and is not a pass-through state: IO2 holds), wait for
   the change as the IOMMU specification requires (the busy bit; bounded by the fence bound; IOMMU0 item 15 says what else),
   read Off back, and disable and read back a command or fault queue found enabled; if it will not go Off, refuse the boot.
   Then, as on a machine handed over Off: program `fctl` (wired interrupts), the queues, then `ddtp` with
   an **empty** directory: from this instant every device id is denied, for DMA and for messages alike (a device's MSI write is a
   reported fault like any other write: there is no context, so no MSI page table). Order matters: queues on before `ddtp` leaves Off, and
   the directory mode is written exactly once; the only other value the kernel ever writes to `ddtp` is Off, at the hand-over. The
   boot line gets `handover=<mode found>` when the mode found was not Off.
4. **Kernel-owned interrupt sources** (`kcore/src/aiatree.rs`, `kernel/src/arch/riscv64/aia.rs`): the IOMMU's own interrupts are
   **wired** (Q-IOMMU-IRQ 3: `fctl.WSI` = 1): the fault-queue and command-queue sources (36 to 39 on `virt`, read from the node)
   are registered as `Kernel` routes with IOMMU4's allocator, so they are **never** in the list `interrupt_create` admits, each
   has an identity the allocator chose (not source + 1), and a hart going offline moves them by the same `move_to` and retire
   steps as any route. This slice adds the two handlers and the registration; the hook, the wider `eie` registers and the move
   protocol are IOMMU4's. (T14 no longer builds such a hook: OD146.)
5. **The fault path**: the handler takes the new lock, drains at most 64 records of the fault queue per interrupt
   (`iommucore::faultq`), counts, prints at most 16 lines per second (IOMMU-SPEC 7.1, 7.2; the line is printed outside the lock from a copied record: `UART` is last in the order, and a print under an
   interrupt-off lock stretches latency), handles the overflow bit, acknowledges `ipsr`. No record is stored for a domain yet
   (there are none: the per-domain ring is IOMMU6). The command queue is polled, not interrupt-driven: that is the spec's
   design (IOMMU-SPEC 6.4), and the interrupt-driven fence is tidy item T274, already recorded.
6. **The lock class** (`klock/src/lib.rs`: `Class::Iommu`, with its row in `klock/tests/order.rs`'s pair table; INVARIANTS.md C1 and
   K5 in place, with the reason for the position, as IOMMU-SPEC 11 fixes it). Only the fault path and the self-test take it here.
7. **PCI windows for the root resource** (`kcore/src/devwin.rs`): `pci-host-ecam-generic` joins `ALLOWED`; its `reg` (ECAM) and
   the memory windows of its `ranges` become device windows; the I/O-port window does not. The test at `kcore/tests/devwin.rs`
   and the suite's refusal list (`suite.rs` near `:13350`, which today asserts the ECAM window is refused) change with it.
   SYSCALLS.md (`vmo_create_physical`) in place. This is safe **only because step 3 comes first in boot order**: with the directory
   empty a bus-mastering PCI device can only fault. A debug assertion ties the two (windows are built after the IOMMU is on).
8. **Statistics** (`debug_stat`, SYSCALLS.md in place): faults taken, faults dropped by rate limit, fault-queue overflows, the
   last fault record (device id, address, cause in three selectors), commands issued, fence timeouts.
9. **Every boot gets the machine option**: `DEFAULT_MACHINE` (`scripts/os_boot_test.py:35`), the Makefile's literal command lines
   (`:412`, `:424`), `scripts/kaslr_check.py`, `bench_release.py`, `entropy_boot_test.py`, `s4a_serial_test.py`, CI workflow
   files, `docs/reference/TEST-TOOLS.md`. The home computer's command lines (`QEMU_FLAGS`, plain `virt`) are **not** touched.
   Host fixtures that embed a machine string or a tree (`kcore/tests/memmap.rs`, `isa_check.rs`, the `fdt_golden` fixtures) are
   regenerated only where the test reads the new node.
10. Reference documents in place: INVARIANTS.md gains IO1 (default deny) and IO2 (no pass-through state is ever written), with
    how each is checked; KERNEL-V1-SPEC is **not** touched here (IOMMU1 did that).

### Invariants, locks, and how it goes wrong (and how it would be seen headlessly)

- Lock order: the fault handler runs in the external-interrupt path, which already holds the big lock (class `OBJ`/`LEGACY`,
  `trap.rs` near `:702`) when it reaches `aia::deliver`; it then takes `IOMMU`, which is later in the order. It takes nothing
  else. Not negotiable: no `VMO`, `AS` or thread call under it.
- A fault storm (a device retrying for ever) must not starve the hart: the handler drains a bounded number of records per
  interrupt and the rate limit bounds the console. Seen as: the dropped counter rising, the suite still finishing.
- `ddtp` written with a mode the device does not support reads back differently: refusal, not a silent Off (Off also denies, so
  the default-deny test alone would pass: the boot line prints the mode read back and the script checks it).
- Forgetting a command line somewhere: that boot stops with "no IOMMU", loudly, in CI's smoke; nothing silent.
- The register page reachable from user mode through a device window would give the IOMMU to a process: `devwin`'s host test
  asserts the page is in no window.
- The 64 GiB direct map (T262's T180 part) covers the registers at `0x301_0000`; the 64-bit PCI window at 16 GiB is only ever
  mapped by user processes.

### Acceptance

The checks in the table pass in the epic's smoke at 1, 2 and 4 harts; the negative control and the missing-IOMMU boot behave as
stated (each run once by hand, and the latter added to the soak list, not to smoke, unless it stays cheap); the feature gate is
green. **Fable review** on its own PR (kernel and security slice), against IOMMU-SPEC 2, 7, 8 and 11 and INVARIANTS.md C1, K5, D1.

PR size: large (driver about 600 lines, discovery and windows about 300, tests and scripts about 600). Shortcuts taken become
tidy items in the same PR (the polled fence is not one of them: it is the spec's choice and T274 holds the alternative).
