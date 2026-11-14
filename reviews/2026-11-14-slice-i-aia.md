# Review: slice I, interrupt objects on the AIA (2026-11-14)

Scope: `git diff main...slice-i` (`kernel/src/aia.rs`, trap, user, smp, kmem, the suite, `scripts/os_boot_test.py`).
Method: reading only. Nothing was booted or run; the QEMU behaviour described below is from the AIA
specification and my knowledge of QEMU's APLIC model, not from a run.

## Verdict

No memory-safety or lock-order problem found. The register offsets, `domaincfg` (IE bit 8, DM bit 2),
`sourcecfg` 6 (level high), the target layout (hart index 31:18, id 10:0), `eidelivery`/`eithreshold`/`eie0`
and the `csrrw stopei, x0` claim all match the specification. The trap path takes the big lock before
`aia::handle`, so `irq_fired` and the APLIC writes are serialised. The `setipnum` re-pend is the
mechanism the specification intends for level sources in MSI mode and is sound for lost interrupts.

The real weaknesses are in the acceptance checks (1, 2) and two narrow hardware/race cases (3, 4, 5).

## Findings, ranked

### 1. Test gap (real): nothing checks the `setipnum` re-pend, the one new mechanism that prevents a lost interrupt
`kernel/src/aia.rs:146`; `init/src/bin/suite.rs:7703-7725`.
In MSI mode a level source pends only on a rising edge, and forwarding the message clears the pending bit.
A line still high at `interrupt_ack` never fires again unless `setipnum` is written. The suite test
always quiets the device (`wr(&b, 0x64, 1)`) before acknowledging, so deleting line 146 passes every
test; `blkd` would then hang only under the right timing (a completion arriving between its status read
and its ack). The comment at suite.rs:7703 still says QEMU's PLIC cannot show this (T43); QEMU's APLIC can.
Fix: add a step that acknowledges *without* clearing InterruptStatus and requires a second interrupt
(`claims == 2`) promptly, then quiets the device; add a `fault-aia-no-repend` negative control for it.

### 2. Negative control is coarser than it claims (real, test quality)
`kernel/src/aia.rs:139-142`; `Makefile` `os-boot-test-aia`; `kernel/Cargo.toml:45`.
The fault returns from `enable` once `TAKEN > 0` globally, and `enable` is also the create path.
`blk_round_trip`, `blk_driver_restart` and `devmgr_starts_blkd_for_the_disk` run before
`interrupt_object_fires_and_masks`, so by then `TAKEN > 0` and the test's own `interrupt_create` never
enables the source. The test therefore fails at "the device's interrupt reaches the object", not at
"a later request interrupts again": the control shows "no delivery at all is noticed", not "a missing
unmask on ack is noticed". The earlier block tests also degrade (blkd falls back to noticing `done()`
at its deadline) and may fail; `negative_control` accepts any number of other failures.
Fix: apply the fault only on the ack path (a parameter or a separate `unmask` function), so creation
still works and the test fails at its second request; ideally assert the failing message.

### 3. Duplicate message on ack (real on hardware, benign on QEMU)
`kernel/src/aia.rs:144-146`.
If the line rose while the source was masked, the pending bit is already set: `setienum` forwards it
(clearing pending), then `setipnum` with the line still high pends and forwards a second message. On
QEMU both land in the same `eip` bit before the hart returns from the syscall and merge. On hardware
the second can arrive after the first was taken: one spurious interrupt and an extra `claims` count
("exactly one claim for one request" would be flaky there).
Fix: write `setipnum` *before* `setienum` (target, setipnum, setienum). While masked, pending is set at
most once; the enable then forwards exactly one message. No loss: an edge between the two writes sets
the same bit.

### 4. Stale message after destroy and re-create on another hart: `MISROUTED` false positive (real, narrow)
`kernel/src/aia.rs:178-180`, `kernel/src/user.rs:1419`.
Hart A has message n pending in its IMSIC but is spinning for the big lock in `trap_handler`. Hart B
destroys the object (`cleanup_dead` masks the APLIC source, but cannot clear A's `eip`) and creates a new
one (`AIMED = B`). A then takes the stale message: it masks the new object's source, fires it (a
tolerated spurious interrupt, recovered by the driver's ack) and counts `MISROUTED`, which the boot
script treats as fatal. Functionally harmless; a possible flake of the acceptance check.
Fix: keep a per-source generation (or "aimed hart at mask time") and count misrouting only when the
taking hart matches neither the current nor the previous aim; or document it in T59.

### 5. `aia=aplic` (APLIC without IMSIC) is taken for the AIA (real, untested configuration)
`kernel/src/aia.rs:78-103`.
Detection looks only for a childless `riscv,aplic` at `0x0d00_0000`. A direct-mode APLIC matches too:
`domaincfg.DM` is then read-only 0 and `init_hart` touches IMSIC registers through `siselect`/`sireg`
that do not exist (illegal instruction at boot, or silently no delivery with `present() == true`, so
drivers wait instead of polling). Nothing reads `domaincfg` back.
Fix: require `msi-parent` on the APLIC node (or a `riscv,imsics` node whose `interrupts-extended` names
S-mode), and read `domaincfg` back to confirm DM stuck; otherwise report "none".

### 6. Hart id used as IMSIC hart index (known, T59)
`kernel/src/aia.rs:134,144`. Correct on QEMU only. Already recorded; also `hart << 18` silently
truncates ids above 16383. No action beyond T59.

## Nits

- `INVARIANTS.md:77` (D5) was not updated: still "masked from the claim", "QEMU's PLIC is edge-like",
  and cites T43, which this slice moved to Resolved. CLAUDE.md requires the invariant file to follow.
  Stale PLIC text also at `OS-ARCHITECTURE.md:126,484`, `init/src/bin/blkd.rs:6`,
  `init/src/bin/suite.rs:7642,7703-7705`.
- `device_interrupts_arrive_where_asked` is not in `REQUIRED` (`scripts/os_boot_test.py:86`); it is
  enforced only indirectly by the regex on its output line. Add it.
- `kernel/src/aia.rs:135-143`: the `// SAFETY:` comment is separated from its `unsafe` block by the
  `AIMED` store and the fault `cfg`; move it next to the block.
- `SOURCES` (`aia.rs:45,91`) is stored and never read.
- `init_hart` does not clear stale `eip` bits left by firmware; harmless (one unowned claim that masks
  an already-masked source), but a `csrc` of `eip0` would make the start state exact.
- `kcore/src/ipc.rs:1110` doc says the return value tells the caller to mask; `handle` now masks
  unconditionally before asking. Update the comment.
- The "off the boot hart > 0" requirement at `-smp 2` depends on where the scheduler runs the drivers;
  plausible but not guaranteed. Watch for flakes.
- `kmem` maps the four APLIC pages on plain `virt` too, where nothing is at that address; never
  touched (`present()` is false), so harmless.

## Checked and found sound

- Re-aiming on ack: the source is always masked when `enable` runs (fired implies masked), so the
  target write cannot race a forward; no message can be in flight to the old hart except case 4.
- Object destroyed with its message pending: taken later, source already masked, `irq_fired` finds no
  object. Same as the old unowned-source behaviour.
- Source pending before the object existed: `setienum` forwards a latched pending bit and `setipnum`
  covers a line that is simply high. The "tolerate one spurious interrupt" contract is kept.
- Secondary harts: `PRESENT` is set before `start_secondaries`; a source is only ever aimed at a hart
  by a thread running on it, after `init_hart`; external interrupts take the big lock even before the
  scheduler is ready.
- `fired` cannot be false while the source is masked with a live object: every take sets `fired`.
- MMIO: offsets up to `0x3020` are inside the four mapped pages; the selftest count follows.
