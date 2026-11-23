# KERNEL-V1: the kernel that is frozen (specification, v1, 2026-11-18)

Version: v1, 2026-11-18.

This document states what the OS-track kernel is when it is declared finished for a while ("v1"): its targets, its
contract with user programs, its memory model, its scaling, power and hardening properties, and how it is tested. It is
the result of the owner's kernel review of 2026-11-18 and the decisions OD126 to OD143 (`docs/records/decisions.md`).

It **amends** `OS-ARCHITECTURE.md` (sections 1, 4, 5, 6, 7, 9, 11), `SCHED-SMP-SPEC.md` (sections 1.3, 3, 4.4, 4.11,
4.12, 5.1) and `T-SPEC.md` (section 3); each of those carries a dated amendment that points here. Where this document
and an older spec differ, this one wins. What the kernel does today is in `docs/reference/SYSCALLS.md` and
`docs/reference/INVARIANTS.md`; what is left to do is in `work/`.

"Decided" below means the owner said so on 2026-11-18. Everything else is the design chosen here to meet those
decisions; a change to it is a dated amendment at the end.

---

## 1. Targets (Decided)

| Subject | Target |
|---|---|
| Processor | The RVA23 profile (RVA23S64 kernel, RVA23U64 user), **enforced**: the kernel refuses to boot on a hart that lacks a required extension (section 2.1). Also required beyond the profile: AIA (APLIC and IMSIC, message-signalled mode) and Sstc |
| Machine | QEMU `virt` is the only machine tested. Every machine fact comes from the device tree (section 2.3), so a second machine is a matter of drivers, not kernel constants |
| Scale | Up to 64 harts; up to 128 GiB of RAM (Sv39). Hundreds of processes, tens of thousands of threads. Nothing may preclude more harts later |
| Power | No periodic tick. Harts can be taken offline and brought back. Deeper idle states and suspend-to-RAM through firmware. Timer slack. Heterogeneous harts stay out, but hart placement is a replaceable policy |
| Contract | The stable interface for programs is the **system library** the kernel provides, not the syscall instruction or its numbers (section 3.1) |
| Hardening | Address randomisation for programs and for the kernel; a kernel entropy pool; no ambient debug calls. Shadow stacks, landing pads, pointer masking and speculation mitigations have reserved ABI space only (section 7.5) |
| Architectures | RISC-V is the only one implemented. The kernel is organised so that a second one (AArch64 is the one in mind) is an added directory, not a rewrite (section 2.6) |
| Scope of the work | The kernel, plus whatever must change in `rt`, the C library and the servers to keep the system green at every step |

Non-goals of v1: heterogeneous harts, NUMA, physical hotplug, Sv48, large pages for user mappings, an IOMMU, implementing a second
architecture (its place is prepared: section 2.6), a fair scheduling class (SCHED-SMP-SPEC 4.7 stays deferred), splitting the object lock (SCHED-SMP-SPEC
3.13 keeps its trigger), bring-up on a physical board.

---

## 2. Platform

### 2.1 The profile is checked, not assumed

* At boot every hart checks the extensions the kernel relies on and stops with a structured `##PANIC missing
  extension <name>` line if one is absent. The list is one table in the kernel (`REQUIRED_EXTENSIONS`): the RVA23S64
  mandatory set as the kernel uses it (Sv39, Svade, Sstc, Zicbom, Zicboz, Zba, Zbb, Zbs, V, Zicntr), plus AIA. Each
  entry is confirmed against the profile text when it is added (OD47). The source of truth is the device tree's ISA strings, cross-checked by a probe
  where the privileged architecture allows one (`stimecmp` writable, `senvcfg` bits stick, `vlenb` non-zero).
* There are no fallbacks for a required extension: no SBI timer, no polling mode without an interrupt controller, no
  flush-on-every-switch mode for a short ASID field (section 5.4 covers short ASIDs by generations instead).
* The kernel is compiled for the profile's scalar extensions (never F, D or V: FP3 and its vector twin). User
  programs, `rt` and C are compiled for RVA23U64, vector included.
* The test machine is `-cpu rva23s64` everywhere. No test boots another CPU.

### 2.2 Per-thread machine state

* **Floating point** as today (FP1 to FP3).
* **Vector**: the same lazy scheme. A thread starts with `sstatus.VS` Off; its first vector instruction traps, the
  kernel allocates a state buffer of `32 * vlenb` bytes plus the vector CSRs, zeroed, and retries. Dirty state is
  saved on every trap from user mode and restored by token on return, exactly as FP2 says for floating point.
  `vlenb` is read once at boot; harts that disagree are a boot failure. The buffer is kernel memory charged to the
  thread's job (section 4.7).
* **Environment configuration**: `senvcfg` enables `cbo.zero`, `cbo.clean`, `cbo.flush` and `cbo.inval` for user
  mode on every hart (drivers with DMA need the cache-block calls; C libraries built for the profile use
  `cbo.zero`).
* **Instruction cache**: when a frame is first mapped executable in an address space, the kernel executes `fence.i`
  locally and SBI `remote_fence_i` on every other online hart before the mapping is used. (Once per code page;
  QEMU does not need it, hardware does.)

### 2.3 Discovery: what comes from the device tree

The kernel parses the device tree firmware passes in `a1` (the pointer, not an assumed place in RAM) with
`kcore::fdt`. Nothing in this table is a constant in the kernel or in `abi`:

| Fact | Source | Replaces |
|---|---|---|
| RAM ranges, reserved ranges | `/memory`, `/reserved-memory`, the memory reservation block | `RAM_BASE`, `RAM_SIZE`, "the top 2 MiB holds the tree" |
| Where the kernel was loaded | computed by position-independent early boot code | `KERNEL_PHYS` |
| Timer frequency | `/cpus/timebase-frequency` | `TICK_CYCLES`, the 10 MHz assumption in the latency trace |
| Harts: count, hart ids (any values, need not be dense), ISA strings | `/cpus/cpu@*` | `MAX_HARTS` 8 and the scan of ids 0 to 7 |
| Interrupt controller: APLIC base, number of sources, each hart's IMSIC file | `riscv,aplic`, `riscv,imsics` | `APLIC_BASE`, `MAX_IRQ` |
| Console | the `/chosen/stdout-path` UART (NS16550A); the SBI debug console when there is none | the address `0x1000_0000` |
| Power off and reset | SBI system reset; the test finisher node when present (test builds) | the address `0x10_0000` |
| Device memory a physical VMO may name | every `reg` range of a device node that is not RAM and not a device the kernel drives | `DEVICE_WINDOWS` |
| Cache block size | `riscv,cbom-block-size`, `riscv,cboz-block-size` (exported to user mode in the system library's data page) | none today |
| Entropy | `/chosen/rng-seed`, `/chosen/kaslr-seed` | none today |

A hart has a dense **index** (0 to n-1, in device-tree order, the boot hart first) and a firmware **id**; every
kernel structure uses the index and only the SBI and AIA layers use the id. The limit is 64 harts (masks are one
`u64`); per-hart records, boot stacks and emergency stacks are allocated at boot for the harts found.

### 2.4 Interrupts and timers

* AIA in message-signalled mode is the only interrupt model. Inter-hart interrupts are sent by writing the target
  hart's IMSIC file directly (one reserved interrupt identity), not through SBI.
* Remote TLB flushes stay SBI RFENCE (SCHED-SMP-SPEC X5).
* The timer is `stimecmp`. There is **no tick**: section 5.1.

### 2.5 Limits are derived, not tuned

The constants of `abi::limits` that exist to protect kernel memory become values computed at boot from RAM, and act
only as backstops behind the job quotas (section 4.7):

| Backstop | Value |
|---|---|
| Live kernel objects | RAM bytes / 4096 |
| Queued IPC bytes, whole system | RAM / 32 |
| Processes | RAM / 1 MiB, at most 2^20 |
| User threads | RAM / 64 KiB, at most the kernel-stack window |
| DMA pages, whole system | RAM / 16 |
| Kernel heap | bounded by RAM, not by a fixed window |

Per-object limits that are part of the contract stay constants: 64 KiB and 64 handles per message, 128 messages and
256 KiB queued per channel end, 64 KiB per socket buffer. Per-process ceilings (handles, regions, threads) are the
job's quota, with no constant below it.

### 2.6 The architecture boundary (Decided, OD143)

Everything that depends on the processor architecture lives behind one boundary, so that AArch64 can be added later
without touching the rest of the kernel. Nothing below is an AArch64 implementation; it is how the RISC-V code is
arranged.

* **Where the code lives.** `kernel/src/arch/riscv64/` holds all of it: boot entry and early paging, the trap entry
  and exit assembly, the trap frame, the context switch, the per-hart register (`tp`), control-register access,
  firmware calls (SBI), the timer, inter-hart interrupts, TLB and cache maintenance, FP and vector state save and
  restore, the interrupt controller driver (AIA), idle and suspend entry, and the extension check of 2.1.
  `kernel/src/arch/mod.rs` re-exports the selected architecture as `arch::*`.
* **The interface.** The rest of the kernel uses only the items `arch/mod.rs` documents: a `TrapFrame` type with
  accessors (syscall number and arguments, return values, program counter, the stack pointer, "came from user
  mode"); `Context` and `switch`; `this_cpu()`; `irq_save` and `irq_restore`; `timer_set` and `clock_read`;
  `ipi_send`; `tlb_flush_page`, `tlb_flush_asid`, `tlb_shootdown`, `icache_sync`; `MachineState` save and restore
  for each lazily switched unit; `trait IrqChip` (enable, mask, unmask, claim, route) with the AIA as its one
  implementation; `cpu_idle`, `cpu_suspend`, `cpu_start`, `cpu_stop`, `system_suspend`, `system_reset`; a
  `Fault` decoded from the trap (kind, address, access) so no code outside `arch` reads a cause register.
* **Page tables.** `kcore::pagetable` is generic over a `PageTableFormat` (entry encoding, levels, large-page
  levels, the software bits, how an address-space identifier is installed); Sv39 is its one implementation. The
  address-space code above it names no bit of an entry.
* **Constants that differ by architecture** are in `abi::arch`: the user address range, the stack arena, the layout
  of each `thread_read_state` kind, the raw cause value carried in an exception message. Everything else in `abi`
  is the same on every architecture, including handles, rights, statuses, limits and the call set.
* **Names.** Kernel code outside `arch` says "hart" for a processor as today; nothing outside `arch` names a
  RISC-V register, control register, instruction or firmware call. Exit codes for unhandled faults use the
  exception kind of section 3.6 (`-1000 - kind`), not a RISC-V cause number.
* **What is already shared.** The device tree (section 2.3) is what AArch64 firmware provides too; the pure crates
  have no architecture in them apart from the page-table format; the system library is built per architecture
  from the same call table.
* **The check.** The docs check refuses `asm!`, `global_asm!`, a control-register name or an SBI call outside
  `kernel/src/arch/` and `libsys`. The kernel crate compiles (`cargo check`) for `aarch64-unknown-none` against a
  stub `arch/aarch64/` whose every function is `unimplemented!()`: that proves the boundary is complete without
  implementing anything. The stub carries no behaviour and is never booted.
* A second architecture needs its own profile-floor decision beside OD127 before it is implemented.

---

## 3. The contract with programs

### 3.1 The system library is the boundary (Decided, OD128)

* The kernel image contains a shared object, `libsys.so` (ELF64, position independent, built from `abi`), and maps
  it into every process at `process_start`: text read-execute, one read-only data page (ABI version, timer
  multiplier and shift, page size, cache block sizes, hart count). Its base is passed to the first thread in `a2`.
* Its exported functions (`sys_<name>`, C calling convention) are the whole contract. The syscall instruction, the
  numbers in `a7` and the register convention behind them are private to the kernel and the library and may change
  in any build.
* **Enforced**: an `ecall` from user mode whose address is not inside the calling process's system-library text is
  an illegal-instruction exception (section 3.6). A soak configuration shuffles the syscall numbers at build time to
  prove nothing else depends on them.
* The library is additive: a function is never removed or changed; a call the kernel no longer provides stays as a
  stub returning NOT_SUPPORTED. `sys_abi_version()` returns a number that only grows.
* `sys_clock_get` runs in user mode: it reads the `time` counter and scales it with the data page. It makes no
  kernel entry.
* Programs find the functions by the library's dynamic symbol table. `rt` and the C library do this once at startup;
  a dynamic loader, when one exists, does it the ordinary way. Statically linked programs remain valid: only the
  system library is ever loaded dynamically by the kernel.
* The definition of the contract is a conformance program (`abi-conformance`) that exercises every function, every
  error in the reference and every limit, built only against the library. A kernel is conformant when it passes.

The server protocols (`proto`) are not part of the kernel's contract. They are private between the C library, `rt`
and the servers for as long as programs link those libraries dynamically; that is the follow-on work after v1.

### 3.2 Handles (Decided, OD129)

* A handle is a 64-bit value. Bits 0 to 23 are the table slot plus 1; bits 24 to 63 are the slot's generation. 0 is
  never valid. `BOOTSTRAP_HANDLE` stays 1.
* A process may hold up to 2^24 - 1 handles, bounded in practice by its job's object quota. The table grows on
  demand; it is not sized for the worst case.
* A slot's generation is 40 bits and a slot that exhausts it is retired (H2 unchanged). At a million reuses of one
  slot per second that is 34 years.
* Handle arrays in channel messages and syscall buffers are 8 bytes per handle, little endian. Rights stay 32 bits.

### 3.3 Identifiers and counters never wrap (OD130)

* Every identifier the kernel hands out that is never reused is 64 bits: the object id a program may compare
  (**koid**: a counter from 1, assigned at creation, independent of the registry slot), the thread id, the process
  id (the koid of the process object).
* **CNT1** (invariant): no kernel counter wraps. A counter narrower than 64 bits either retires what it counts (a
  handle slot, a registry slot) or refuses with NO_MEMORY at its limit; none uses wrapping arithmetic. Test builds
  accept a `ff=` boot argument that starts named counters a few steps below their limit, so the refusal path is
  tested in milliseconds.
* ASIDs are not derived from a process id (section 5.4).

### 3.4 Time

* All times are nanoseconds on the monotonic clock since boot; deadlines are absolute; `u64::MAX` means never
  (SCHED-SMP-SPEC 4.11, restated). The word "tick" names nothing in the contract.
* Timer slack is per thread, as SCHED-SMP-SPEC 4.11 defines it.
* CPU time is readable for a thread, a process and a job (`thread_info`, `process_info` and `job_info` selectors),
  in nanoseconds.

### 3.5 Memory calls

All sizes and offsets are bytes and multiples of the page size unless said otherwise.

| Call | Meaning |
|---|---|
| `vmo_create(bytes, job, flags)` | As today, by size in bytes, with no size cap below the user half of the address space. `flags`: RESIZABLE |
| `vmo_set_size(vmo, bytes)` | WRITE, and the VMO was created RESIZABLE. Shrinking frees the pages beyond the new size and removes them from every mapping; a later touch of such a mapping is a fault. Refused for DMA and device VMOs and for a VMO with children |
| `vmo_decommit(vmo, offset, length)` | WRITE. Frees the VMO's own committed pages in the range, removes them from every mapping, credits the job. The range reads as zeros afterwards. Refused for DMA and device VMOs |
| `vmo_create_child(vmo, offset, length, job, flags) -> handle` | READ on the source. `flags` is exactly one of SNAPSHOT or PRIVATE (section 4.3) |
| `process_map(process, vmo, vaddr, vmo_offset, length, flags) -> address` | Adds the flag ANYWHERE: the kernel picks a random free address (`vaddr` must be 0). Returns the address. A region has no size cap. The region remembers the most permissions the VMO handle allowed (`max_perms`) |
| `process_unmap(process, vaddr, length)` | Unmaps every page of the range, splitting regions; holes in the range are not an error |
| `process_protect(process, vaddr, length, flags)` | The range must be fully mapped. New permissions must be within each region's `max_perms` and never both writable and executable (M2 stays absolute). Splits regions. This is how a JIT turns a page from writable to executable (OD133) |
| `process_map_info(process, vaddr, buffer) -> next` | INSPECT. Describes the first region at or after `vaddr`: base, length, permissions, `max_perms`, VMO koid, VMO offset, flags |
| `process_clone_region(source, target, vaddr, mode)` | MANAGE on both. Maps the source's region at `vaddr` into the target at the same address: `mode` SHARE maps the same VMO, SNAPSHOT maps a SNAPSHOT child. The primitive a `fork` shim loops over; no VMO handle is exposed |
| `pager_create() -> pager, channel` and `pager_create_vmo`, `pager_supply`, `pager_fail` | Section 4.4 |

### 3.6 Threads, exceptions and waits

| Call | Meaning |
|---|---|
| `thread_spawn` | The only way to make a thread (`thread_create` is removed). New flag SUSPENDED: the thread exists but does not run until `thread_resume` |
| `thread_suspend(thread)`, `thread_resume(thread)` | MANAGE. A suspend count. A suspended thread stops at its next return to user mode; a thread blocked in a call counts as stopped where it is. The thread object raises the signal SUSPENDED when it has stopped |
| `thread_read_state(thread, kind, buffer, length)`, `thread_write_state(...)` | MANAGE. `kind`: GENERAL (on RISC-V x1 to x31 and pc), FP, VECTOR; the layouts are per architecture (`abi::arch`). Only for a thread that is stopped (suspended, never started, or in an exception); BAD_STATE otherwise. Writing is refused while the thread is inside a blocking call; the pc must be a user address |
| `process_exception_channel(process) -> channel`, `job_exception_channel(job) -> channel` | MANAGE. At most one each. When a thread takes a fault or an illegal instruction the kernel stops it and writes a message to the process's channel: kind, tid, pc, address, and two handles (the thread, and an **exception** object) |
| `exception_resume(exception, how)` | `how`: RETRY (the thread continues from its state, which the handler may have changed), NEXT (offer it to the job's channel, then to each ancestor job's; with none left the process ends as today), KILL. Closing the exception handle is NEXT |
| `port_wait_many(port, buffer, capacity, deadline) -> count` | Up to 64 packets in one call; `port_wait` is the library's wrapper for one |

A process with no exception channel anywhere above it ends as today, with exit code `-1000 - kind` (section 2.6). A
process may serve its own channel from a dedicated thread; that, with suspend and state access, is everything a C
library needs for asynchronous signals and fault handlers, and everything a debugger needs. The kernel has no notion
of signals.

### 3.7 Process start

A program image is a static executable, position independent (`ET_DYN` with relative relocations only, applied by its
own startup code) or fixed (`ET_EXEC`, accepted but placed without randomisation). The first thread starts with `a0`
and `a1` as today and `a2` holding the system library's base. `exec` is composed in user space as spawn-and-replace
(OD134); the kernel has no call that resets a process.

### 3.8 Debug calls

`debug_write` and `debug_stat` need a handle to a **debug resource** that only the kernel-started process holds and
may pass on. A release kernel has no test-only code paths: no self-tests, no fault-injection features, no hook that
steps over a kernel fault.

---

## 4. Memory

### 4.1 Frames

* A buddy allocator over the RAM the device tree describes, orders 0 (4 KiB) to 18 (1 GiB). Its per-frame
  metadata (reference count, order, list links) is an array sized from RAM and taken from boot memory before the
  allocator exists, never from the kernel heap.
* Per-hart caches of free order-0 frames, as SCHED-SMP-SPEC 3.10 specifies; an offline hart holds none.
* Contiguous allocation (DMA) is an ordinary higher-order allocation.
* The frame database's reference-count interface and its invariants (M1, M3) are unchanged.

### 4.2 The kernel's own mappings and allocator

* The direct map uses the largest pages that fit (1 GiB, then 2 MiB, then 4 KiB at the edges). The kernel image
  keeps 4 KiB pages and its section permissions.
* The kernel allocator is a size-class slab allocator over frames (classes 16 bytes to 2 KiB, one frame per slab;
  larger requests are whole frame runs from the buddy allocator, addressed through the direct map), with a per-hart
  magazine per class so the common allocation takes no shared lock. Empty slabs return to the frame allocator. It
  has no fixed window and no boot region beyond what early boot needs before frames exist.
  `linked_list_allocator` is no longer used.
* Allocation failure on a path a process can drive returns NO_MEMORY and never panics (T-SPEC decision 5, kept).

### 4.3 Memory objects

* A VMO's pages are found through a per-VMO page index. Each **family** (a VMO and everything created from it by
  `vmo_create_child`) has one lock; unrelated families do not contend. This replaces the single `VMO` lock class of
  SCHED-SMP-SPEC 3.3 with a class of one instance per family, same position in the order, never two held.
* A VMO knows its mappings (address space, range), so decommit, shrink and copy-on-write can remove or
  write-protect its pages wherever they are mapped, followed by a shootdown (TLB2).
* **SNAPSHOT child** (for `fork`): after the call the source and the child each see the source's contents as they
  were, and each one's later writes are private to it. The child's job must be the source's job, so pages never
  change payer. Implementation: the source's committed pages move to a hidden, immutable node; source and child
  become its leaves; a write copies the page into the writing leaf (or moves it, when no other leaf can see it);
  when a leaf dies and its parent has one leaf left, the two are merged.
* **PRIVATE child** (for a private file mapping): the child sees the parent's pages, including later changes to
  pages the child has not written; the child's writes copy into the child and are charged to the child's job; the
  parent never sees them. Allowed across jobs and on pager-backed parents. The parent's node lives until its last
  child is gone.
* A chain of children is at most 64 deep; deeper is BAD_STATE.
* A copy-on-write page is mapped read-only (a software PTE bit marks it); a write fault on it is resolved by the
  fault path, never seen by the program.
* **COW1** (invariant): a write through one member of a family is never visible through another member that was
  entitled to the old contents. **COW2**: every page of a family is charged to exactly one job, and the charge
  moves only together with the page, within one job.

### 4.4 Pager-backed memory objects (Decided, OD133)

* `pager_create()` returns a pager handle and one end of a channel on which the kernel writes page requests.
  `pager_create_vmo(pager, key, bytes)` makes a VMO whose pages are absent until supplied.
* A fault, a `vmo_read`, a `vmo_commit` or a kernel user-copy that needs an absent page queues a request
  `{key, offset, length}` (one per page range, not one per waiting thread) and **blocks the thread, holding no lock**,
  through the one park protocol. The wait ends when the range is supplied, failed, the pager dies, the thread's
  process is killed or the thread is interrupted.
* `pager_supply(pager, vmo, offset, length, source, source_offset)` moves committed pages out of an anonymous
  source VMO into the pager's VMO (no copy; the charge moves with them to the pager VMO's job) and wakes the
  waiters. `pager_fail(pager, vmo, offset, length, status)` fails the waiters: a fault becomes an exception of kind
  PAGER, a kernel copy returns the status.
* If the pager object is destroyed every absent page of its VMOs fails.
* Requests are kernel messages charged to the pager's job. A full request channel does not lose a request: the
  faulting thread waits for room.
* Dirty-page tracking and writeback are **not part of v1**. Their place is reserved (a request kind and two call
  names) so adding them changes nothing that exists.
* **PG1** (invariant): a thread waiting for a pager holds no kernel lock and can always be killed.

### 4.5 Address spaces and the fault path

* Regions split and merge (unmap and protect of sub-ranges). A region carries a generation counter.
* A fault: under the address-space lock, find the region and take a reference to its VMO, remember the region's
  generation, release the lock; ask the family for the page (which may copy, zero, or wait for a pager); take the
  address-space lock again, and if the generation still matches, map the page. No lock is held while waiting, and
  the address-space lock is never held across the family lock's slow path. This replaces "a fault holds `AS`
  throughout" in SCHED-SMP-SPEC 3.3 and 3.10.
* After resolving a fault the kernel flushes that one page's translation, not the whole TLB.
* **Fault-around**: when a fault is resolved, up to 15 neighbouring pages of the same region that are already
  committed in the VMO (and need no copy) are mapped in the same pass.
* `process_map` with ANYWHERE chooses uniformly among free gaps of the user half using the kernel entropy pool
  (section 7.1).

### 4.6 The reachability sweep

The cycle sweep of the object registry runs in bounded slices (SCHED-SMP-SPEC 3.12): 256 objects per locked section,
with a sweep epoch that makes a handle transfer during a sweep mark its object. No single section is proportional to
the number of live objects.

### 4.7 Who pays (Decided, OD135)

* Every byte of kernel memory a job causes is charged to it: kernel stacks, page-table pages (exactly, not
  estimated), object bodies, handle-table growth, FP and vector state, queued messages, pager requests, and the
  slabs backing them by size class.
* The job ledger is not consulted on every page fault: an address space holds a small prepaid reservation of pages
  from its job (64), refilled and returned in batches, so the ledger lock is taken once per batch. J1 holds with
  reservations counted as usage.
* When a job is at its limit the calls it makes fail with NO_MEMORY; other jobs are unaffected. When RAM itself is
  exhausted the allocation fails the same way; there is no kernel out-of-memory killer in v1.
* DMA frames follow the policy of S6-SPEC (the supervisor keeps the handle); the kernel does not enforce device
  quiescence until an IOMMU exists (OD136).

---

## 5. Harts, time and scheduling

SCHED-SMP-SPEC remains the design; this section lists what v1 changes or adds.

### 5.1 No tick (OD131)

* There is no periodic interrupt on any hart, busy or idle. Each hart has one timer queue (SCHED-SMP-SPEC 4.11) and
  programs `stimecmp` to its earliest hard expiry, or to never.
* A time slice is a duration: when a hart has a second runnable thread at the running thread's level, the slice end
  is an entry in the queue; otherwise there is none.
* The scheduler's clock is the `time` counter scaled by the device tree's frequency.
* Tasks 13 and 28 of SCHED-SMP-SPEC are one design and are built together.

### 5.2 Sixty-four harts

* No path is proportional to the number of harts while holding a lock, and no periodic path visits every hart.
* Placement uses published per-hart hints and three global masks (`online`, `idle`, `overloaded`: a hart with a
  queued thread it is not running). A wake picks the thread's last hart if it would run there, else an idle hart
  from the mask, else the hart with the lowest published running priority. A hart pulls only when its own queue
  empties or when `overloaded` is non-zero, and then only from the harts in that mask. GP1 is unchanged.
* The scaling checks of SCHED-SMP-SPEC 6.1 are run at 16 and 32 harts as well as 4.

### 5.3 Harts go offline and come back (OD132)

* A hart is Offline, Starting, Online or Draining. `hart_set_online(resource, index, online)` (the root resource)
  changes it; `hart_info(resource, index)` reads the state. The last online hart cannot go offline.
* Going offline: the hart leaves the `online` mask (no wake, push or interrupt is routed to it from then on); the
  records of every thread homed there, blocked ones and their timer entries included, move to other harts; device
  interrupts targeted at it are retargeted; its frame and allocator caches are returned; it flushes its TLB, drains
  its dead list and stops through SBI HSM. A thread pinned to the hart makes the call fail with BAD_STATE.
* Coming online is the secondary-hart start path. A hart that returns starts with an empty TLB, so address-space
  `ran_on` masks have its bit cleared when it goes offline.
* Every per-hart loop and mask in the kernel is over online harts.
* **HT1** (invariant): no thread is homed on, no interrupt is routed to and no shootdown waits for a hart that is not
  Online. **HT2**: a thread that could run is never stranded by a hart going offline (ID1 across the transition).

### 5.4 Address-space identifiers

Unique ASIDs while they last, with generations when they run out: an allocator hands out ASIDs from the width the
hart implements (probed); when the space is exhausted the generation advances, every hart flushes at its next
address-space switch, and ASIDs are handed out afresh. TLB1 is restated per generation. There is no mode that
flushes on every switch.

### 5.5 Idle states and suspend

* An idle hart whose next deadline is further away than a threshold (1 ms) suspends through SBI HSM (retentive)
  instead of `wfi`; it resumes on any interrupt as `wfi` does.
* `system_suspend(resource)`: takes every other hart offline, quiesces the interrupt controller, calls SBI system
  suspend, and on resume restores trap, timer, interrupt-controller and address-space state and brings the harts
  back. NOT_SUPPORTED when the firmware has no system-suspend extension. Telling drivers to quiesce their devices
  first is the caller's job.

### 5.6 Placement policy

The choice of hart for a wake is behind one trait in `kcore` with two implementations: **spread** (the default,
section 5.2) and **pack** (prefer the lowest-indexed online hart that is not overloaded, so load consolidates and
other harts stay idle). `system_set_placement(resource, policy)` selects it at run time. A capacity-aware policy for
heterogeneous harts would be a third implementation.

---

## 6. Locking after v1

The order of SCHED-SMP-SPEC 3.3 stands, with these changes: `VMO` is one instance per family (section 4.3); the
address-space lock is not held across a family's slow path (section 4.5); the job ledger is reached through
reservations (section 4.7). The object domain stays one lock, with the trigger of SCHED-SMP-SPEC 3.13 evaluated at
16 harts.

When the legacy lock is gone, the accessors that hand out `&'static mut` to shared state (`kmem::get`,
`user::state`, `thread::kernel`) are gone with it: shared state is reachable only through a lock guard. That is a
condition of the last locking task, not a later clean-up.

---

## 7. Hardening

### 7.1 Kernel entropy

A ChaCha20 generator (`rngcore`) keyed at boot from the device tree's seeds, mixed with the `seed` CSR on harts
that have Zkr and with the `time` counter. It serves address choices inside the kernel only. Without any seed the
kernel boots, does not randomise, and says so in the system library's data page.

### 7.2 Address randomisation for programs (Decided, OD139)

Position-independent images are placed at a random base; the first stack's top, the stack arena's starting hint and
every ANYWHERE mapping are random (section 4.5); the system library's base is random per process. Fixed addresses
remain available to a program that asks for them. Test builds accept `aslr=off`.

### 7.3 Address randomisation for the kernel

The kernel image is position independent and relocates itself at boot to a base chosen at random in 2 MiB steps
within its top-level slots; the direct map's base is chosen at random in 1 GiB steps among the free slots. The
entropy is small (about 9 and 7 bits) and is stated, not hidden.

### 7.4 No panic from user input

Kernel code reachable from a syscall or a user fault contains no `unwrap`, `expect`, unchecked index or unchecked
arithmetic whose failure a program can cause; the release kernel is built with overflow checks. A syscall fuzzer and
an allocation-failure sweep (section 8) are the standing checks.

### 7.5 Reserved, with nothing behind it in v1

* Shadow stacks and landing pads: two `process_map` flag bits, a `thread_read_state` kind and a per-process
  feature word in the start arguments are reserved.
* Pointer masking: user pointers with non-zero upper bits are refused, as today; a per-process flag is reserved to
  turn masking on.
* Speculation: no mitigation. The exposure (every address space maps the direct map) is recorded.

---

## 8. How the kernel is tested

The tiers of `docs/reference/TESTING.md` stand: the gate is tiers 0 and 1. **The soak tier and tier 3 below run only when
the owner asks for them** (OD142): no schedule, no label, nobody else starts them. For the
kernel (`kernel/`, `kcore`, `klock`, `abi`) the regime is:

**Tier 0, host.** The syscall and thread layer builds on the host behind a `Machine` trait and is tested there
against the real code: every argument error, right, quota and accounting rule, under Miri, with coverage measured.
Panic-finding lints (`unwrap_used`, `expect_used`, `indexing_slicing`, `arithmetic_side_effects`,
`undocumented_unsafe_blocks`) are errors in the kernel crates. A grep gate keeps atomics inside the audited layer
(C5). The docs check enforces that every error listed in the syscall reference names a test.

**Tier 1, the gate.** A **kernel-only boot**: the kernel, `init` and the kernel-facing tests of the suite, with no
servers, at one, two and four harts, in the test build and the release build. Counter fast-forward tests (CNT1).
Instruction-count checks: at one hart under icount, the instruction count of the main syscall paths is compared
with a recorded value within 5%.

**Tier 2, soak** (only when the owner asks): as today.

**Tier 3, long runs** (hours not minutes; only when the owner asks; never required to merge; its make targets and
workflow are named `overnight`, since that is when it is meant to be started):

| Check | What it does |
|---|---|
| Schedule perturbation | A kernel feature inserts seeded random delays at lock acquire, release and inter-hart interrupt send; the kernel-only boot runs across at least 200 seeds at 2, 4 and 8 harts |
| Syscall fuzzing | A structure-aware, seeded generator of syscall sequences: on the host against the syscall layer, and in the guest with every audit function run between batches |
| Allocation failure | Fail the Nth kernel allocation, for every N a kernel-only boot reaches; every result is NO_MEMORY or success, never a panic or a leak |
| Mutation testing | Automated mutation of `kcore` and `klock`; every surviving mutant is listed |
| Checked release | The release kernel with debug assertions on, through the whole suite at 4 harts |
| Wide SMP | The kernel-only boot and the scaling benchmarks at 16 and 32 harts |
| Shuffled syscall numbers | The whole suite with the numbers permuted |
| Repeats | Every SMP soak target 20 times; pass rates recorded per target |

**Rules.**

* An intermittent failure of a kernel test is a defect until its cause is known. It gets a work item at once, a
  failing run is captured with QEMU record and replay, and it is not waved through by re-running.
* A change that needs evidence from tier 2 or tier 3 is listed as a run owed to the owner; it is not held back, and
  nobody but the owner starts the run.
* The kernel stack: every boot reports each thread's stack high-water mark and fails above half the stack;
  recursion in kernel code is bounded by a stated depth.
* Every `unsafe` block in the kernel binary is covered by an audit recorded in `docs/records/reviews/`, renewed
  when the block changes.

---

## 9. What "frozen" means

The kernel is declared v1 and left alone when all of these hold:

1. `abi-conformance` passes, and `docs/reference/SYSCALLS.md` describes the system library, not syscall numbers.
2. The legacy lock is gone and the `&'static mut` accessors with it.
3. Every item of section 8 runs, and the soak tier and tier 3 have each been green on three consecutive runs that the
   owner started.
4. An independent review, fresh context, has attacked the whole kernel against this document, `INVARIANTS.md` and
   `SYSCALLS.md`, and every finding is fixed or is a work item.
5. No open work item of risk H names the kernel.

After that a kernel change needs a reason recorded as a decision, and may only add to the system library.

## Amendment 2026-11-20 (PL5a): the device windows fail closed

Section 2.3 lists, as the source of "device memory a physical VMO may name", "every `reg` range of a device node that is not RAM and not a
device the kernel drives". The rule is narrower: the windows come from the device tree, but a node contributes a window only if its
`compatible` is on a list of the devices the system drives (`virtio,mmio` and `google,goldfish-rtc` today), its `status` is not disabled and
every bus above it has an empty `ranges` property; RAM, everything reserved, the console and the ranges of every interrupt controller, timer,
test finisher and power or reset node are cut from every window. A device not on the list is unmappable until someone adds it
(`kcore::devwin::ALLOWED`). The interrupt sources an interrupt object may name follow the same list (PL5c): only the `interrupts` of the
admitted devices whose interrupt parent is the supervisor-domain APLIC (`kcore::aiatree`); an unknown source is refused. Reason: a denylist fails open for any device nobody thought of, and on QEMU `virt` it would have made fw-cfg
(whose DMA interface writes guest RAM), the writable flash banks and the PCI ECAM window mappable by the holder of the resource handle.

## Amendment 2026-11-23 (HD2): the loader relocates

Section 3.7 said a static-PIE's startup code relocates itself. The loader does it instead. A static-PIE's `R_RISCV_RELATIVE` relocations may name any 8-byte word inside the file bytes of any of its segments (not only the writable ones); the loader (the kernel's own, and `rt::spawn`, which share `kcore::elf`) writes `base + addend` into its copy of those bytes before it maps them, so the mappings keep their permissions (read-only data is never writable) and a program carries no relocation code. The dynamic section may hold `DT_TEXTREL` and `DT_DEBUG` beside the `DT_RELA` family. Reason: the prebuilt `core` library is not position independent (absolute words in read-only data), and a self-relocating startup would have to run before anything that reads those words. Placement is as in 3.7: the image's span is placed at a random base in the mapping arena, the first thread's stack at a random place in the stack arena (a guard page below and above), the heap (a runtime matter) at a random page of the 4 GiB window below the mapping arena, and `thread_spawn`'s stack search starts at a random page of the arena's lower half. `aslr=off` (test builds) makes every random placement the first fit.

## Amendment 2026-11-23 (HD3): what 7.3 became

The image is a static-PIE that `_start` relocates in place (the relocations are applied to the image where it stands in memory, with paging off, adding the slide). The slide is a multiple of 2 MiB below the 1 GiB slot of the image (the early table's `EARLY_L1`), so its entropy is 511 minus the image's size in 2 MiB pages choices (about 9 bits for today's image). The direct map is moved up by 0 to 64 whole gigabytes (65 positions: about 6 bits), not "among the free slots": the early table maps the 64 GiB of physical space the kernel supports before the device tree's memory is read, so its 64 slots must all fit between slot 256 and the kernel-stack window at slot 384; the self-test window moved to slot 448 to make the room. The choice is made in assembly by a scanner of the tree's own (`/chosen/kaslr-seed`, else `/chosen/rng-seed`), and `kcore::kaslr` is the model the boot self-test checks it against. The values are `kmem::direct_base()` and `arch::slide()`; `debug_stat` 129 and 130 report them (test builds and release alike: they are counters, not addresses). A fatal trap prints `sepc` minus the slide in a test build and only the cause in a release build.

## Amendment 2026-11-23 (AB7): how the system library is built and mapped

Section 3.1's library is built as follows. `libsys.so` is generated from `abi::syscall::TABLE` (one `sys_<name>` per call, the name the constant's in lower
case) and linked by the kernel's build script, which runs `rustc` for the user target with a linker script of its own: one read-and-execute segment holds the headers,
`.dynsym`, `.hash`, `.dynstr`, the code and `.dynamic`, and one read-only segment of one page with no file bytes is the data page. The object has no relocation of any
kind (the build checks the section types and the dynamic tags), and `sys_raw` takes the call number in `a6` for a number the table does not name (the boot-time
suite's bad-number tests). The kernel makes two shared VMOs of it (code and data page, charged to the root job) the first time a process starts, and maps both
into every process at `process_start` (and at load, for a kernel-started process) at a random place of the mapping arena, the page tables charged to the
process's job like a `process_map`'s; the process record keeps the base and the text range. Every thread of the process starts with the base in `a2`. The data
page holds nine words (`libsys::page`): the ABI version, the clock's multiplier and shift, the page size, the two cache-block sizes, the harts online, flags and the
counter's value at the clock's zero. `sys_clock_get` is code in the library that reads `time` and the page (the kernel sets `scounteren.TM`), so `clock_get`
no longer enters the kernel (`debug_stat` 131 counts the calls the kernel still answers). `rt` reads the library's `.dynsym` once in `_start`; every call goes
through the table that gives, so a program has no `ecall` of its own (`scripts/check_ecall.py`; `crash` and `suite` plant theirs on purpose). The kernel's own `clock_get`,
`port_wait` and the other old shapes stay until AB8 takes the numbers private.

## Amendment 2026-11-23 (AB8): entry enforcement and private numbers

The trap handler admits a user `ecall` only when its `sepc` lies in the library's text range of the process the hart runs (INVARIANTS ABI1); any other is an
illegal-instruction exception at that pc. The check is two compares against per-hart words written where the hart's current process is, and it also guards the
calls handled with no big kernel lock. Programs no longer contain an `ecall` at all except `crash`'s deliberate one (`check_ecall.py`). The call numbers are
produced by `abi::syscall::number`, which is the identity unless `ABI_SHUFFLE_SEED` is set when the workspace is built, and then a seed-chosen bijection on
`0..LIMIT`: `libsys`, the kernel's dispatch and `rt` all take their numbers from it, and a program that wrote a literal number fails the suite.
