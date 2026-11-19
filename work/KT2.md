---
id: KT2
kind: slice
title: Flake tooling: capture a failing run with QEMU record and replay; a repeat target with pass rates
area: testing
horizon: near
refs: [KERNEL-V1-SPEC, OD140]
---

**What.** OD140: an intermittent kernel failure is a defect until its cause is known. Give that rule its tools.

**How.** (1) `OS_RECORD=<dir>` on `scripts/os_boot_test.py`: boot with `-icount shift=auto,rr=record,rrfile=...` and keep the file only when the boot fails; `OS_REPLAY=<file>` replays it with `-s -S` available. Record mode is single-threaded TCG, so it is used by the repeat target below and by hand, not in the gate. Say plainly in `TESTING.md` that a race that needs true parallel harts may not reproduce under record. (2) `make os-repeat TARGET=<soak target> N=20`: runs the target N times with `SOAK_JOBS=1`, prints `pass/N` per target, and saves the serial log of every failing run under `target/repeat/`. (3) The soak workflow uploads failing logs as artifacts.

**Acceptance, written first.** The fail-demo kernel under `OS_RECORD` leaves a record file, and `OS_REPLAY` of it reproduces the same failing `##TEST` line. `make os-repeat TARGET=os-boot-test-fail-demo N=3` reports `0/3` with three logs.

**Review.** Default.

## Note (2026-11-18, later the same day)

OD142: the repeat target and record mode are tools. Their acceptance uses only the fail-demo kernel (a few short boots, not a soak target). Running `os-repeat` over a soak target is a soak run and is the owner's to start.
