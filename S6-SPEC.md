# S6: storage drivers (spec, v1)

Status: v1. S6a (section 3, including interrupt objects) is built; S6b and S6c are not. This file is the written acceptance contract for S6
(CLAUDE.md: every milestone needs a runnable acceptance check, written before the code). Decisions
made here are logged as OD64 and later in `OS-ARCHITECTURE.md` section 17.

## 1. Goal and non-goals

**Goal.** A user-mode virtio-blk driver serves a block protocol over a channel. A pattern written
to the virtual disk reads back identical; the driver can be killed and restarted and the data is
still there. The driver's whole authority is what it is handed: an MMIO window, an interrupt (or
none: see section 4), and DMA buffers.

**Non-goals (S6).** No filesystem on the disk (S7+; memfs stays), no partition tables, no request
queue depth above 1 to start (one request in flight, protocol allows pipelining later), no IOMMU
(DMA is trusted-driver, OD14), no hot-plug, no virtio-pci, no AIA wired interrupts (OD64: the
driver polls there), no change to `consoled` (T14 stays; S6d is optional).

## 2. Slices

- **S6a, device capabilities (kernel).** A *root resource* object; `vmo_create_physical` (MMIO),
  `vmo_create_dma` and `vmo_phys_addr` (DMA), interrupt objects over the PLIC (`interrupt_create`,
  `interrupt_ack`, signal `INTERRUPT`). Host-tested state machines in `kcore`; QEMU-tested with
  a throwaway user test that talks to virtio-blk's registers and takes its interrupt.
- **S6b, the driver and the protocol.** `proto::blk` (pure wire format, host-tested with goldens),
  `blkd` (the driver, `init/src/bin/blkd.rs`), and a boot-test harness that attaches a scratch disk.
  Acceptance in section 7.
- **S6c, device tree and driver manager.** A pure DTB parser (`dtb` module in `kcore` or its own
  crate, `forbid(unsafe_code)`, fuzzed: it must not panic on hostile input) and a `devmgr` user
  program that finds virtio-mmio nodes, probes the device id register and starts the matching
  driver with only its handles. The kernel hands `init` a read-only VMO holding the DTB.

Each slice is one PR, green on `make check`, with a Fable-or-equivalent independent review before
merge for S6a (new kernel authority).

## 3. Device capabilities (S6a)

**Root resource (`KObj::Resource`).** An object whose handle (rights: TRANSFER, DUPLICATE, MANAGE;
no READ/WRITE) `init` obtains with `resource_root()` (syscall 95, refused to every process the
kernel did not start, exactly like `job_root`). Only holders can mint
device objects. `init` gives it to nobody except `devmgr` (S6c); until then the test suite holds it.
It is a capability, not ambient authority: a process without the handle cannot reach device memory
at all.

**Physical VMOs.** `vmo_create_physical(resource, pa, size) -> vmo`. The range must lie inside one of
the *device windows* the kernel allows (QEMU `virt`: virtio-mmio `0x1000_1000..0x1000_9000`; the
list is a constant in `abi::layout`) and be page aligned; RAM is never allowed (that is what DMA
VMOs are for). The VMO has fixed frames that are not in the frame database, are never freed, are
never zero-filled, never charged to a job's page limit (the *object* is charged as one object),
and may be mapped READ/WRITE (never EXECUTE, which the aspace already forbids for VMOs). Rights:
READ, WRITE, MAP, DUPLICATE, TRANSFER. Mapped device memory is accessed with ordinary loads and
stores (RISC-V PMAs: QEMU's MMIO is naturally strongly ordered per device); drivers add `fence`
where virtio requires ordering between normal memory and the doorbell.

**DMA VMOs.** `vmo_create_dma(resource, pages) -> vmo`: `pages` physically contiguous frames
(at most `MAX_DMA_PAGES` = 64 per object, 4 MiB total across the system), committed and zeroed at
creation, charged to the *caller's job* pages like any VMO, freed with the VMO. The extra right
`DMA` on the handle gates `vmo_phys_addr(vmo) -> pa`; a plain duplicate without `DMA` cannot
learn physical addresses. Mappable READ/WRITE. A DMA VMO that is unmapped and closed while the
device may still write to it is the driver's bug (trusted-driver DMA, OD14); the kernel frees the
frames when the last handle and mapping go.

**Interrupt objects.** `interrupt_create(resource, irq) -> handle` (rights: WAIT, INSPECT, DUPLICATE,
TRANSFER, MANAGE for ack) for a PLIC source number `irq` in `1..=MAX_IRQ` (QEMU `virt` virtio-mmio
slots are 1..=8; the allowed set is a constant). One object per source at a time (second create is
BAD_STATE until the first object is destroyed). The kernel enables the source at the PLIC for hart
0's S-mode context at creation (priority 1, threshold 0) and masks it again on destroy. When it
fires: the trap handler *claims* it, **masks the source**, and sets the object's signal
`INTERRUPT` (bit 5, `abi::signals::INTERRUPT`, included in `WAITABLE`) and wakes waiters. A driver
`object_wait`s for `INTERRUPT`, services the device, then calls `interrupt_ack(handle)`: clears the
signal, completes the claim and **unmasks** the source. Level-triggered devices therefore cannot
storm. The state machine (Idle, Pending-until-acked, Destroyed) is host-tested in `kcore`
(`kcore/tests/interrupt_obj.rs`; the source-to-object map lives in the kernel); a destroyed object's pending interrupt is completed and the source masked (nothing is
left enabled with nobody listening).

**Unsupported controllers.** On a machine whose wired interrupts go through AIA (APLIC + IMSIC;
`-machine virt,aia=aplic-imsic`), `interrupt_create` returns NOT_SUPPORTED (the kernel detects the
PLIC by reading its configured source registers' presence from the device tree in S6c; before
S6c, by a probe of the PLIC's priority register which is absent under AIA, kept in one function
`irq::controller()`). Drivers must then fall back to **polling** (section 4). *Decision OD64.* The
AIA job in CI keeps running the whole suite; the block tests run in polling mode there.

**Authority and invariants.** New rights live in `abi::rights` (`DMA`, reuse `MANAGE` for ack).
`INVARIANTS.md` gets: D1 a physical VMO never names RAM and the kernel never reads or writes it; D2 a
physical address is only learnable through a DMA-right handle; D3 device objects are minted only
with the root resource; D4 DMA frames are freed exactly once (S6a); D5 a masked interrupt stays
masked until acked or destroyed (S6a-2). `SYSCALLS.md` documents
every new call with preconditions, rights and errors in order.

## 4. The driver (S6b)

`blkd` receives at start: its bootstrap channel (to the launcher/devmgr), the root-resource-minted
handles *only* (MMIO VMO for its slot, an interrupt handle or none, a DMA VMO with DMA right).
Nothing else (no resource, no job beyond its own).

- **Transport.** Modern virtio-mmio (version 2): negotiate `VIRTIO_F_VERSION_1`; one virtqueue with a
  fixed small size (`QUEUE_SIZE` 8), descriptors, avail and used rings in the DMA VMO's first page,
  request header/status and a bounce area in the rest. No indirect descriptors, no event idx.
- **Waiting.** With an interrupt handle: `object_wait(INTERRUPT)` then `interrupt_ack`. Without
  (AIA): poll the used ring, yielding between looks via `object_wait` on an event with a 1-tick
  deadline. Both paths are the same code behind one `Wait` trait.
- **Request path.** One request at a time: header, data (bounce area, at most `MAX_IO` = 16 sectors
  = 8 KiB per request), status byte. Reads copy bounce -> client buffer, writes the reverse.
- **Kill safety (T42).** A killed driver must not leave its device writing into freed RAM. Before the
  restart acceptance S6b makes DMA VMO frames survive their holder's death until the device is reset
  (design chosen at S6b: the DMA VMO's frames are quarantined by the kernel, and the driver manager
  releases them after resetting the virtio device through its own MMIO VMO).
- **Failure.** A device error status is returned as I/O error; a device that does not complete
  within 2 seconds is a timeout error and the driver exits (so a supervisor can restart it); every
  malformed client request is refused with an error, never a driver crash.

## 5. The block protocol (S6b; `proto::blk`)

Channel messages, request/response, like the memfs protocol (`proto`): all integers little-endian.
Requests carry a transaction id the reply echoes.

| Op | Request | Reply |
|---|---|---|
| `Info` | | `sectors: u64`, `sector_size: u32` (512), `read_only: u8`, `max_io: u32` |
| `Read` | `sector: u64`, `count: u32` | status, then `count * 512` bytes (<= `MAX_IO`) |
| `Write` | `sector: u64`, `count: u32`, data | status |
| `Flush` | | status |

Status: `Ok`, `BadRange`, `TooBig`, `ReadOnly`, `IoError`, `Timeout`, `BadRequest`. Bulk transfers
through messages (small, <= 8 KiB) for S6; shared-buffer bulk transfer is a later optimisation
(OS-ARCHITECTURE section 11, "moving data") recorded in TIDY-UP.

## 6. Test hardware

The boot harness (`scripts/os_boot_test.py`) creates a scratch raw image (16 MiB, deterministic
non-zero pattern in sector 0 only so a "reads zeroes" bug is visible) and passes
`-global virtio-mmio.force-legacy=false -drive file=IMG,if=none,format=raw,id=hd0 -device
virtio-blk-device,drive=hd0` on every run (every existing test must keep passing with the device
present: the kernel does not touch it). `OS_DISK=0` leaves it off.

## 7. Acceptance (runnable checks, written before the code)

Checks live in the boot suite (`init/src/bin/suite.rs`) and are in `REQUIRED`; host checks in `kcore`
and `proto`.

*S6a (kernel authority)*
1. `kcore` model tests of the interrupt object state machine, and of physical/DMA VMO accounting
   against an independent reference (frames freed exactly once; physical VMOs never free).
2. `device_caps_need_the_resource`: every new call refuses a missing/wrong/under-righted handle;
   `vmo_create_physical` refuses RAM, misaligned and out-of-window ranges; `vmo_phys_addr` refuses
   a handle without DMA; DMA creation refuses over-limit and charges the job; a closed DMA VMO's
   frames come back (exact frame count).
3. `mmio_reads_the_virtio_magic`: map the physical VMO of the first virtio-mmio slot, read the
   magic `0x74726976` and device id.
4. `interrupt_object_fires_and_masks` (PLIC machines): provoke a virtio-blk interrupt, observe
   INTERRUPT, check no second interrupt arrives before ack (the source is masked), ack, provoke
   another. On AIA: `interrupt_create` is NOT_SUPPORTED.
5. `kill_a_driver_with_a_pending_interrupt`: no leak, nothing left enabled.

*S6b (driver)*
6. `proto::blk` golden tests (encode/decode, malformed input never panics).
7. `blk_round_trip`: write a 64 KiB deterministic pattern in 8 KiB requests, read it back, compare
   byte for byte and checksum; `Info` reports the image size.
8. `blk_bad_requests`: ranges past the end, oversize, count 0, wrong lengths: refused with the
   right status, driver still serves afterwards.
9. `blk_driver_restart`: write a pattern, **kill the driver**, start a new one, read the pattern
   back identical; the clients' old channel reports PEER_CLOSED; no frame, object or job leaks
   (exact).
10. SMP: the same on `-smp 4`; the AIA job runs it in polling mode.

*S6c (manager)*
11. DTB parser goldens from a real QEMU dtb (`-machine virt,dumpdtb`) and a fuzz/property test
    (random mutations: no panic, no out-of-bounds).
12. `devmgr_starts_blkd_for_the_disk`: the manager finds the virtio-blk node and the driver
    round-trips a pattern; with no disk attached it finds no block device and starts nothing.

## 8. Open questions

None blocking. Chosen defaults to revisit later: polling fallback under AIA (a real APLIC driver is
TIDY-UP work), one request in flight, message-copied bulk data.
