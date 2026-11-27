KERNEL_TARGET := riscv64gc-unknown-none-elf
HOME_ELF      := target/$(KERNEL_TARGET)/debug/home-kernel
QEMU          := qemu-system-riscv64
QEMU_FLAGS    := -machine virt -nographic -bios default

# The disk for SAVE/LOAD: a raw image on a virtio block device. The kernel's
# driver speaks the modern virtio-mmio transport, hence force-legacy=false.
DISK          := disk.img
DISK_FLAGS    := -global virtio-mmio.force-legacy=false -drive file=$(DISK),if=none,format=raw,id=hd0 -device virtio-blk-device,drive=hd0


.PHONY: os-boot-test-sbi-console os-boot-test-entropy os-boot-test-loadaddr os-latency os-bench os-boot-test-overflow fetch-third-party libc-gap
.PHONY: lint-host os-aarch64-check os-models feature build lint-target hooks epic-bisect os-allocfail os-fuzz-host os-coverage os-mutants os-chaos smoke soak check-all overnight-repeat-smp os-boot-test-checked os-kchecked-build kernel-main lint-kernel os-repeat os-boot-test-fail-run os-ktest os-ktest-small os-ktest-4 os-ktest-release os-ktest-release-content os-ktest-shuffle os-ktest-entry-neg kquick os-boot-test-smoke os-boot-test-smoke-a os-boot-test-smoke-b docs-check os-boot-test-smp-neg help disk os-user kernel os-test os-miri os-boot-test os-boot-test-main os-boot-test-fail-demo os-boot-test-rr os-boot-test-release os-boot-test-smp os-boot-test-smp-neg os-boot-test-sched-neg os-boot-test-nodisk os-boot-test-aia os-boot-test-cpu-neg os-boot-test-iommu-neg os-boot-test-iommu-bare os-boot-test-iommu-inval os-boot-test-iommu-msi os-interactive os-run os-release os-release-run os-release-smoke os-serial-test os-auth-test test fmt fmt-check lint home-kernel home-run home-run-logo home-debug home-boot-test check

help:
	@echo "make test       host tests for the home-computer crates (no QEMU needed)"
	@echo "make home-kernel  build the home-computer kernel ELF"
	@echo "make home-run     boot the home computer in QEMU (Ctrl-A then X to exit)"
	@echo "make home-run-logo boot with a graphics window (ramfb); press 2 at the menu for LOGO"
	@echo "make home-debug   boot paused with a gdb stub on :1234"
	@echo "make home-boot-test boot in QEMU non-interactively and check the output"
	@echo "make disk       create disk.img (2 MiB, for SAVE/LOAD) if it does not exist"
	@echo "make os-test     OS track: host tests, plus loom and shuttle (os-miri adds Miri)"
	@echo "make kernel      OS track: build the kernel (with the user programs)"
	@echo "make os-boot-test OS track: boot the kernel headlessly, check its structured output"
	@echo "make hooks      install the commit hook (format, docs check, clippy); once per clone"
	@echo "make feature    the feature gate: what CI runs on a pull request into an epic branch (no QEMU)"
	@echo "make check      the epic gate (smoke): what CI runs on a pull request into main"

test:
	cargo test -p abi -p cfmt -p clockmodel -p data -p ish -p kcore -p ksys -p kfuzz-gen -p klock -p iommucore -p pcicore -p virtio -p netwire -p netcore -p netsim -p nettcp -p rngcore -p sockshim -p dns -p http1 -p supervise -p dhcp -p netring -p proto -p svc -p memfs -p mounts -p pathres -p profile -p fsfmt -p doofs -p doofs-tools -p basic -p basic-script -p cartfs -p console -p turtle -p logo -p rv64
	python3 home/scripts/verify_demo_images.py

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

# Clippy, in two halves: the host crates, then the crates built for the target (the home kernel, the user programs, the OS kernel).
# `lint` is both, and is what the commit hook runs (clippy does the whole of `cargo check`, so the hook needs no separate check).
lint: lint-host lint-target

lint-host:
	cargo clippy -p abi -p cfmt -p clockmodel -p data -p ish -p kcore -p ksys -p kfuzz-gen -p klock -p iommucore -p pcicore -p virtio -p netwire -p netcore -p netsim -p nettcp -p rngcore -p sockshim -p dns -p http1 -p supervise -p dhcp -p netring -p proto -p svc -p memfs -p mounts -p pathres -p profile -p fsfmt -p doofs -p doofs-tools -p basic -p basic-script -p cartfs -p console -p turtle -p logo -p rv64 --all-targets -- -D warnings

lint-target:
	cd home/kernel && cargo clippy -- -D warnings
	cd init && cargo clippy -p rt -p oslibc -p init -- -D warnings
	cd kernel && cargo clippy -- -D warnings

# The kernel crate's own clippy run (the panic-finding lints of KT3 are denied in its syscall, trap, memory and scheduler modules); `lint` runs it too.
lint-kernel:
	cd kernel && cargo clippy -- -D warnings

home-kernel:
	cd home/kernel && cargo build

# Created only if missing (no prerequisites), so your saved files are never wiped.
$(DISK):
	python3 home/scripts/disk.py new $(DISK)

disk: $(DISK)

home-run: home-kernel $(DISK)
	$(QEMU) $(QEMU_FLAGS) $(DISK_FLAGS) -kernel $(HOME_ELF)

# Same machine plus a framebuffer, shown in a window (QEMU picks the display
# backend). The serial console stays in this terminal; Ctrl-A then X quits.
# On macOS the Cocoa window can show the screen magnified 2x and cropped to one
# quadrant on a Retina display; zoom-to-fit scales the whole screen to the window.
ifeq ($(shell uname),Darwin)
LOGO_DISPLAY := -display cocoa,zoom-to-fit=on
endif

home-run-logo: home-kernel $(DISK)
	$(QEMU) -machine virt -bios default $(DISK_FLAGS) -device ramfb $(LOGO_DISPLAY) -serial mon:stdio -kernel $(HOME_ELF)

home-debug: home-kernel $(DISK)
	$(QEMU) $(QEMU_FLAGS) $(DISK_FLAGS) -kernel $(HOME_ELF) -s -S

home-boot-test: home-kernel
	python3 home/scripts/console_test.py $(HOME_ELF)
	python3 home/scripts/trap_test.py $(HOME_ELF)
	python3 home/scripts/basic_test.py $(HOME_ELF)
	python3 home/scripts/demo_test.py $(HOME_ELF)
	python3 home/scripts/menu_test.py $(HOME_ELF)
	python3 home/scripts/logo_test.py $(HOME_ELF)
	python3 home/scripts/jit_test.py $(HOME_ELF)
	python3 home/scripts/logo_diff_test.py $(HOME_ELF)
	python3 home/scripts/disk_test.py $(HOME_ELF)

# --- OS track (S-series; see OS-ARCHITECTURE.md) ---
OS_ELF      := target/$(KERNEL_TARGET)/debug/kernel
OS_FAIL_ELF := target/os-fail/$(KERNEL_TARGET)/debug/kernel
OS_RR_ELF   := target/os-rr/$(KERNEL_TARGET)/debug/kernel
OS_NOSHOOT_ELF := target/os-fault-shootdown/$(KERNEL_TARGET)/debug/kernel
OS_NOSATP_ELF  := target/os-fault-satp/$(KERNEL_TARGET)/debug/kernel
OS_NOCOWPROT_ELF := target/os-fault-cowprot/$(KERNEL_TARGET)/debug/kernel
OS_NODECOMMIT_ELF := target/os-fault-decommit/$(KERNEL_TARGET)/debug/kernel
OS_NOPROTSHOOT_ELF := target/os-fault-protshoot/$(KERNEL_TARGET)/debug/kernel
OS_NOKILL_ELF  := target/os-fault-killipi/$(KERNEL_TARGET)/debug/kernel
OS_NOFPSAVE_ELF := target/os-fault-fpsave/$(KERNEL_TARGET)/debug/kernel
OS_NOCBZE_ELF := target/os-fault-cbze/$(KERNEL_TARGET)/debug/kernel
OS_NOREPEND_ELF := target/os-fault-repend/$(KERNEL_TARGET)/debug/kernel
OS_OVERFLOW_ELF := target/os-overflow/$(KERNEL_TARGET)/debug/kernel
OS_NOUNMASK_ELF := target/os-fault-unmask/$(KERNEL_TARGET)/debug/kernel
OS_VNOUNMASK_ELF := target/os-fault-vunmask/$(KERNEL_TARGET)/debug/kernel
OS_VNOREPEND_ELF := target/os-fault-vrepend/$(KERNEL_TARGET)/debug/kernel
OS_NOMSIRETIRE_ELF := target/os-fault-msiid/$(KERNEL_TARGET)/debug/kernel
OS_NOFORCED_ELF := target/os-fault-forced/$(KERNEL_TARGET)/debug/kernel
OS_NOPOKE_ELF  := target/os-fault-poke/$(KERNEL_TARGET)/debug/kernel
OS_NOPULL_ELF  := target/os-fault-pull/$(KERNEL_TARGET)/debug/kernel
OS_EARLYREAP_ELF := target/os-fault-earlyreap/$(KERNEL_TARGET)/debug/kernel

# The OS kernel, plus a build with `fail-demo` (one test fails on purpose) that the
# boot test uses to prove it can see a failure.
# The user programs (init, crash) are built for the no-FPU user target and embedded in the
# kernel image, so they must exist before the kernel is built.
os-user:
	cd init && cargo build

# Only the normal kernel (what the smoke boot needs); `kernel` adds the fail-demo and round-robin builds.
kernel-main: os-user
	cd kernel && cargo build

kernel: kernel-main
	cd kernel && cargo build --features fail-demo --target-dir ../target/os-fail
	cd kernel && cargo build --features sched-round-robin --target-dir ../target/os-rr

# Host tests of the OS crates: plain, then the same code under the loom and
# shuttle model checkers (separate target dirs: the cfg flags rebuild everything).
os-test:
	cargo test -p abi -p kcore -p ksys -p kfuzz-gen -p klock -p iommucore -p pcicore -p virtio
	$(MAKE) os-models

# AR1 (OD143): the kernel compiles against the AArch64 stub in kernel/src/arch/aarch64 (no code is built or run for it).
os-aarch64-check:
	cd kernel && cargo check --target aarch64-unknown-none

# The loom and shuttle model checkers alone (CI runs them as their own job, parallel to the host tests).
os-models:
	@if cargo test -p kcore --features fault-offline-skip-blocked --test sched_offline --target-dir target/neg-offline >/dev/null 2>&1; then echo 'fault-offline-skip-blocked was not caught by sched_offline'; exit 1; fi
	# SC2's negative control: with no hart ever marked overloaded the masks' acceptance test must fail
	@if cargo test -p kcore --features fault-skip-overloaded --test sched_masks --target-dir target/neg-overloaded >/dev/null 2>&1; then echo 'fault-skip-overloaded was not caught by sched_masks'; exit 1; fi
	# IOMMU7's negative control on the host: basic-translate MSI entries (the device's data word reaches the hart) must fail the vector model
	@if cargo test -p kcore --features fault-iommu-msi-basic --test iodomain_vectors_model --target-dir target/neg-msi >/dev/null 2>&1; then echo 'fault-iommu-msi-basic was not caught by iodomain_vectors_model'; exit 1; fi
	RUSTFLAGS="--cfg loom" cargo test -p klock --release --test spin_concurrency --target-dir target/loom
	RUSTFLAGS="--cfg loom" cargo test -p netring --release --test doorbell_concurrency --target-dir target/loom
	RUSTFLAGS="--cfg shuttle" cargo test -p klock --release --test spin_concurrency --target-dir target/shuttle
	RUSTFLAGS="--cfg shuttle" cargo test -p netring --release --test doorbell_concurrency --target-dir target/shuttle
	RUSTFLAGS="--cfg loom" cargo test -p kcore --release --test latch_concurrency --target-dir target/loom
	RUSTFLAGS="--cfg loom" cargo test -p kcore --release --test ticket_concurrency --target-dir target/loom
	RUSTFLAGS="--cfg loom" cargo test -p kcore --release --test futex_concurrency --test pifutex_concurrency --target-dir target/loom
	RUSTFLAGS="--cfg loom" cargo test -p kcore --release --test park_concurrency --test fp_claim_concurrency --target-dir target/loom
	RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test latch_concurrency --target-dir target/shuttle
	RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test ticket_concurrency --target-dir target/shuttle
	RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test futex_concurrency --test pifutex_concurrency --test fp_claim_concurrency --target-dir target/shuttle
	RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test idle_concurrency --test charge_migrate_concurrency --target-dir target/shuttle
	RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test rq_concurrency --target-dir target/shuttle
	RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test fcache_concurrency --target-dir target/shuttle
	RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test vmofamily_concurrency --target-dir target/shuttle
	RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test pager_concurrency --target-dir target/shuttle
	RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test park_concurrency --target-dir target/shuttle
	RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test iodomain_concurrency --target-dir target/shuttle
	RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test iodomain_vectors_concurrency --target-dir target/shuttle

# Miri needs a nightly toolchain with the miri component (rustup toolchain install
# nightly --profile minimal --component miri); CI runs it, `make check` does not.
# Only the unit tests and the frame-database model run under Miri: the page-table and
# address-space model tests take minutes there, and kcore forbids unsafe code, so
# Miri has little to find in them.
# Only the unit tests and the frame-database model run under Miri: the page-table and
# address-space model tests are minutes slow there, and kcore forbids unsafe code,
# so Miri has little to find in them (the fake memory in the tests is safe code too).
os-miri:
	cargo +nightly miri test -p abi -p kcore --lib --test frame_model --target-dir target/miri
	# (the allocation-failure sweep starts child processes of itself, which Miri cannot do; the host run and `os-allocfail` cover it)
	cargo +nightly miri test -p ksys --target-dir target/miri -- --skip every_failing_allocation_is_reported_as_no_memory_and_leaves_the_state_clean
	cargo +nightly miri test -p klock --target-dir target/miri
	cargo +nightly miri test -p iommucore --target-dir target/miri

# Tier 1 (smoke, OD83): the whole suite once, on the default (AIA) machine at four harts. The normal
# kernel at four harts is also the fastest full run (35 s: waits overlap).
# The suite is two boots, shards `a` and `b` (OD121), so CI can run them as parallel jobs and each fits its budget with room;
# locally `make os-boot-test-smoke` runs both, one after the other.
os-boot-test-smoke: os-boot-test-smoke-a os-boot-test-smoke-b

os-boot-test-smoke-a: kernel-main
	OS_SHARD=a OS_SMP=4 python3 scripts/os_boot_test.py $(OS_ELF)

os-boot-test-smoke-b: kernel-main
	OS_SHARD=b OS_SMP=4 python3 scripts/os_boot_test.py $(OS_ELF)

# The kernel-only boot (KT1, KERNEL-V1-SPEC 8): the kernel, `init` and the Kernel tests of the suite (`init/ktests.txt`: the PCI disks,
# no network), as shard `k`. `os-ktest` boots the test build at one, two and four harts; `os-ktest-release` the optimised build
# (debug assertions and overflow checks off, no fat LTO so that the build is short) at four. The harness check proves the shard's
# required list holds no System test and fails when a Kernel test is missing.
os-ktest: os-ktest-small os-ktest-4

# one and two harts, without the tests marked `slow` in init/ktests.txt (the harness check first)
os-ktest-small: kernel-main
	OS_SHARD=k python3 scripts/os_boot_test.py --selftest-k
	OS_SHARD=k OS_KFAST=1 OS_SMP=1 python3 scripts/os_boot_test.py $(OS_ELF)
	# HD3: ten boots, at least six distinct slides and four distinct direct-map bases; `kaslr=off` gives one layout
	python3 scripts/kaslr_check.py $(OS_ELF)
	OS_SHARD=k OS_KFAST=1 OS_SMP=2 python3 scripts/os_boot_test.py $(OS_ELF)
	# AB3 (CNT1): every counter starts 16 below 2^32, so the whole shard runs across the old 32-bit boundaries
	OS_BOOT_ARGS="aslr=off ff=tid:0xfffffff0,koid:0xfffffff0,objgen:0xfffffff0,handlegen:0xfffffff0,fptoken:0xfffffff0" OS_SHARD=k OS_KFAST=1 OS_SMP=2 python3 scripts/os_boot_test.py $(OS_ELF)

# four harts, every Kernel test
os-ktest-4: kernel-main
	OS_SHARD=k OS_SMP=4 python3 scripts/os_boot_test.py $(OS_ELF)

# The kernel-only boot at 4 GiB of RAM (T207 PR 4, B1): the Kernel tests at four harts on a kernel built with `latency-trace`; the first test commits a
# 2 GiB VMO whole, unmaps, decommits and kills its last holder, and fails when the longest big-lock hold (debug_stat 14) is over B1's smoke bound (20 ms). The test covers the holds of the calls; the teardown chunks and the drain run under the memory locks without it.
OS_KBIG_ELF := target/os-kbig/$(KERNEL_TARGET)/debug/kernel
os-ktest-big: os-user
	cd kernel && cargo build --features latency-trace --target-dir ../target/os-kbig
	OS_RAM=4G OS_KFAST=1 OS_SHARD=k OS_SMP=4 OS_BOOT_TIMEOUT=300 python3 scripts/os_boot_test.py $(OS_KBIG_ELF)

OS_KREL_ELF := target/os-krelease/$(KERNEL_TARGET)/release/kernel
os-ktest-release:
	cd init && cargo build --release
	cd kernel && OS_USER_PROFILE=release cargo build --release --target-dir ../target/os-krelease
	OS_SHARD=k OS_SMP=4 python3 scripts/os_boot_test.py $(OS_KREL_ELF)

# HD4 part 3: the Kernel tests on a kernel with the release content (no boot self-tests, no scheduler or SMP tests, no `fault-*` features: the `release-suite` feature),
# at four harts, from the separately built suite. The release content itself (no deny-listed symbol) is checked by `os-release-smoke`.
OS_KRC_ELF := target/os-krc/$(KERNEL_TARGET)/release/kernel
os-ktest-release-content:
	cd init && cargo build --release
	python3 scripts/check_pie.py --self-test
	python3 scripts/check_pie.py target/$(KERNEL_TARGET)/release
	# AB7: no program makes a system call of its own (only the test programs `crash` and `suite` plant them)
	python3 scripts/check_ecall.py --self-test
	python3 scripts/check_ecall.py target/$(KERNEL_TARGET)/release
	cd kernel && OS_USER_PROFILE=release cargo build --release --features release-suite --target-dir ../target/os-krc
	OS_RELEASE_SUITE=1 OS_SHARD=k OS_SMP=4 python3 scripts/os_boot_test.py $(OS_KRC_ELF)

# AB8: the kernel-only suite on a build whose syscall numbers are shuffled (`ABI_SHUFFLE_SEED`, `abi::syscall::number`): code that writes a number of its own
# instead of using the table or the library fails. And the negative control: with the entry check off (`fault-skip-entry-check`) the test for a call from
# outside the library must fail. The user programs are built into the shared target directory, so the next ordinary build rebuilds them.
OS_SHUF_ELF := target/os-shuffle/$(KERNEL_TARGET)/release/kernel
OS_NOENTRY_ELF := target/os-fault-entry/$(KERNEL_TARGET)/debug/kernel
os-ktest-shuffle:
	cd init && ABI_SHUFFLE_SEED=ab8-seed cargo build --release
	cd kernel && ABI_SHUFFLE_SEED=ab8-seed OS_USER_PROFILE=release cargo build --release --target-dir ../target/os-shuffle
	OS_SHARD=k OS_SMP=2 python3 scripts/os_boot_test.py $(OS_SHUF_ELF)
os-ktest-entry-neg:
	cd init && cargo build
	cd kernel && cargo build --features fault-skip-entry-check --target-dir ../target/os-fault-entry
	OS_SHARD=k OS_SMP=1 python3 scripts/os_boot_test.py --neg $(OS_NOENTRY_ELF) child_faults_are_contained

# `make os-repeat TARGET=<target> N=<n>`: run a target N times with SOAK_JOBS=1 and print `pass/N` (KT2, OD140); the serial log of
# every failing run is saved under target/repeat/. Over a soak target this is a soak run, which only the owner starts (OD142).
N ?= 20
# The fail-demo kernel run as if it should pass (the kernel-only boot at one hart): a target that always fails, for the check of the repeat
# tooling (`make os-repeat TARGET=os-boot-test-fail-run N=3` reports 0/3 with three logs).
os-boot-test-fail-run: kernel
	OS_SHARD=k OS_SMP=1 python3 scripts/os_boot_test.py $(OS_FAIL_ELF)

os-repeat:
	python3 scripts/os_repeat.py $(TARGET) $(N)

# One targeted boot for a kernel change: format, the pure kernel crates' host tests, the kernel-only boot at four harts. A local
# experiment (watching an acceptance check fail, then pass), not a routine before a commit or a push (TESTING.md).
kquick: fmt-check kernel-main
	cargo test -p kcore -p ksys -p kfuzz-gen -p klock
	OS_SHARD=k OS_SMP=4 python3 scripts/os_boot_test.py $(OS_ELF)

os-boot-test: kernel
	python3 scripts/os_boot_test.py $(OS_ELF) $(OS_FAIL_ELF) $(OS_RR_ELF)

# The same, split so CI can run the parts as separate jobs (a slow or flaky one does not hide the
# others): the normal and fail-demo kernels, and the round-robin kernel.
os-boot-test-main: kernel
	python3 scripts/os_boot_test.py $(OS_ELF) $(OS_FAIL_ELF)

# Only the fail-demo kernel: the check that the harness still sees a failure (CI runs it when the harness changed; the
# normal kernel was just booted by the smoke run).
# (the deliberate failure is a kernel test that runs in every shard, so the short one is enough to prove the harness sees it)
os-boot-test-fail-demo: kernel
	OS_SHARD=b python3 scripts/os_boot_test.py --fail-demo $(OS_FAIL_ELF)

os-boot-test-rr: kernel
	python3 scripts/os_boot_test.py --rr $(OS_RR_ELF)

# The same boot test on QEMU's AIA interrupt controller (APLIC + IMSIC) instead of the
# PLIC (OD35). Needs QEMU with `-machine virt,aia=...` (7.2 or later).
# The same suite on an optimised build with debug assertions and overflow checks off, which is
# what a real release would be: optimisation and assertions can each hide or expose bugs.
OS_REL_ELF := target/os-release/$(KERNEL_TARGET)/release/kernel
os-boot-test-release:
	cd init && cargo build --release
	cd kernel && OS_USER_PROFILE=release CARGO_PROFILE_RELEASE_LTO=fat cargo build --release --target-dir ../target/os-release
	python3 scripts/os_boot_test.py $(OS_REL_ELF)
	OS_SMP=4 python3 scripts/os_boot_test.py $(OS_REL_ELF)

# The same suite with several harts (S5): the secondary harts must come online, take timer and
# inter-hart interrupts, and leave every other test as it was.
os-boot-test-smp: kernel os-interactive
	OS_SMP=2 python3 scripts/os_boot_test.py $(OS_ELF)
	OS_SMP=4 python3 scripts/os_boot_test.py $(OS_ELF) $(OS_FAIL_ELF) $(OS_RR_ELF)

# The SMP negative controls (each disables one safety mechanism and the guarding test must fail) and the
# serial sessions at four harts. A separate target so CI can run it as its own job: the whole SMP part took
# about 28 minutes, too close to the job's timeout.
# The scheduler's negative controls (L11d, SCHED-SMP-SPEC task 11): each kernel breaks one mechanism and the guarding test must fail (or the
# kernel must stop). Kernel-only boots (shard k): no network. Part of the soak tier (OD142): the owner starts it.
os-boot-test-sched-neg: kernel
	cd kernel && cargo build --features fault-skip-poke --target-dir ../target/os-fault-poke
	OS_SHARD=k OS_SMP=4 OS_BOOT_TIMEOUT=240 python3 scripts/os_boot_test.py --expect-panic "starved thread" $(OS_NOPOKE_ELF)
	cd kernel && cargo build --features fault-skip-pull --target-dir ../target/os-fault-pull
	OS_SHARD=k OS_SMP=4 OS_BOOT_TIMEOUT=240 python3 scripts/os_boot_test.py --expect-panic "starved thread" $(OS_NOPULL_ELF)
	cd kernel && cargo build --features fault-early-reap --target-dir ../target/os-fault-earlyreap
	OS_SHARD=k OS_SMP=4 OS_BOOT_TIMEOUT=240 python3 scripts/os_boot_test.py --neg $(OS_EARLYREAP_ELF) teardown_with_threads_on_other_harts

os-boot-test-smp-neg: kernel os-interactive
	cd kernel && cargo build --features fault-skip-shootdown --target-dir ../target/os-fault-shootdown
	cd kernel && cargo build --features fault-skip-kernel-satp --target-dir ../target/os-fault-satp
	cd kernel && cargo build --features fault-skip-cow-protect --target-dir ../target/os-fault-cowprot
	cd kernel && cargo build --features fault-decommit-no-shootdown --target-dir ../target/os-fault-decommit
	cd kernel && cargo build --features fault-protect-no-shootdown --target-dir ../target/os-fault-protshoot
	OS_SMP=4 OS_BOOT_TIMEOUT=240 python3 scripts/os_boot_test.py --neg $(OS_NOSHOOT_ELF) unmap_while_siblings_touch
	OS_SMP=4 OS_BOOT_TIMEOUT=240 python3 scripts/os_boot_test.py --neg $(OS_NOSATP_ELF) teardown_with_threads_on_other_harts
	OS_SMP=4 OS_BOOT_TIMEOUT=240 python3 scripts/os_boot_test.py --neg $(OS_NOCOWPROT_ELF) copy_on_write_children_from_user_mode
	OS_SMP=4 OS_BOOT_TIMEOUT=240 python3 scripts/os_boot_test.py --neg $(OS_NODECOMMIT_ELF) decommit_while_siblings_touch
	OS_SMP=4 OS_BOOT_TIMEOUT=240 python3 scripts/os_boot_test.py --neg $(OS_NOPROTSHOOT_ELF) protect_while_siblings_write
	cd kernel && cargo build --features fault-skip-kill-ipi --target-dir ../target/os-fault-killipi
	OS_SMP=4 python3 scripts/os_boot_test.py --neg $(OS_NOKILL_ELF) user_threads_run_in_parallel,kill_a_spinner_on_another_hart
	cd kernel && cargo build --features fault-skip-fp-save --target-dir ../target/os-fault-fpsave
	OS_SMP=2 python3 scripts/os_boot_test.py --neg $(OS_NOFPSAVE_ELF) user_threads_run_in_parallel,fp_state_is_private_to_each_thread_and_process
	cd kernel && cargo build --features fault-skip-cbze --target-dir ../target/os-fault-cbze
	OS_SMP=1 python3 scripts/os_boot_test.py --neg $(OS_NOCBZE_ELF) vector_state_is_private_to_each_thread_and_process
	cd kernel && cargo build --features fault-skip-forced-touch --target-dir ../target/os-fault-forced
	OS_SMP=2 python3 scripts/os_boot_test.py --neg $(OS_NOFORCED_ELF) replacing_a_handle_ends_a_siblings_wait
	OS_SMP=4 python3 scripts/s4a_serial_test.py $(OS_REPL_ELF)
	OS_SMP=4 python3 scripts/s4e_shell_test.py $(OS_INTERACTIVE_ELF)

# The same suite with no disk attached (neither PCI disk): the device manager must find no block device on either bus and start nothing.
os-boot-test-nodisk: kernel
	OS_DISK=0 python3 scripts/os_boot_test.py $(OS_ELF)

# The AIA is the default machine (OD75): this adds two harts and the negative control for the acknowledgement.
# The plain board has only a PLIC, which the kernel no longer drives: no interrupt controller, drivers poll.
# A kernel thread overflows its stack on purpose: the guard page and the emergency stack must produce a
# `##PANIC kernel stack overflow` report (slice T).
os-boot-test-overflow: kernel-main
	cd kernel && cargo build --features stack-overflow-demo --target-dir ../target/os-overflow
	python3 scripts/os_boot_test.py --expect-panic "kernel stack overflow" $(OS_OVERFLOW_ELF)

# Benchmarks (slice B, SCHED-SMP-SPEC 6.1; soak tier): the suite's `##BENCH` lines at full length at one and four
# harts, compared with the checked-in baseline (a 3x slowdown fails). The numbers are QEMU debug-build costs on whatever
# machine runs them: look at the printed ratios, and update `bench/baseline.tsv` on purpose (and the table in
# TESTING.md) when a change is meant to move them.
# (3x on the machine that made the baseline; CI runners are several times slower, so soak.yml sets BENCH_RATIO=30: the futex pair numbers swing by 30 times between runs with where the pair is placed, T231, T232)
BENCH_RATIO ?= 3
os-bench: kernel-main
	rm -f target/bench.tsv
	OS_BENCH=target/bench.tsv OS_BENCH_FULL=1 OS_SMP=1 python3 scripts/os_boot_test.py $(OS_ELF)
	OS_BENCH=target/bench.tsv OS_BENCH_FULL=1 OS_SMP=4 python3 scripts/os_boot_test.py $(OS_ELF)
	python3 scripts/bench_compare.py target/bench.tsv bench/baseline.tsv --max-ratio $(BENCH_RATIO) --scaling-floor bench/scaling.tsv

# KT5: the guest's retired-instruction counts of the main syscall paths (`##BENCH count_<name> <n> instr` lines of the suite's
# `instruction_counts_report`), booted at one hart under `-icount shift=0`, where they are deterministic. bench/counts.tsv holds the recorded
# values; a rise of more than 25 % fails (slack for runners and for unrelated changes that move the counts, T223), a fall of more than 5 % is printed (record it: `make os-count-record`).
# No disks (OS_DISK=0): under `sleep=off` guest time does not bound the host's disk I/O, so the block tests cannot be judged there (the
# harness refuses the combination); the disk path through the IOMMU is the other Kernel boots' (os-ktest-small, os-ktest-4).
os-count: kernel-main
	rm -f target/count.tsv
	OS_SHARD=k OS_KFAST=1 OS_SMP=1 OS_DISK=0 OS_ICOUNT="shift=0,sleep=off" OS_BENCH=target/count.tsv OS_BENCH_FULL=1 python3 scripts/os_boot_test.py $(OS_ELF)
	python3 scripts/bench_compare.py target/count.tsv bench/counts.tsv --prefix count_ --max-ratio 1.25

os-count-record: kernel-main
	rm -f target/count.tsv
	OS_SHARD=k OS_KFAST=1 OS_SMP=1 OS_DISK=0 OS_ICOUNT="shift=0,sleep=off" OS_BENCH=target/count.tsv OS_BENCH_FULL=1 python3 scripts/os_boot_test.py $(OS_ELF)
	awk -F'\t' '$$2 ~ /^count_/' target/count.tsv > bench/counts.tsv

# S9 Q30 (soak tier): the bulk TCP tests (fetch, httpd, echo) with the guest's network behind the lossy relay
# (scripts/net_lossy_relay.py: loss 2%, duplication 1%, reordering 5%, delay 5 ms on TCP frames) for each seed, at two and four
# harts. LOSSY_SEEDS is a list ("1 2 3 ... 20" for the full run). The capture is checked as ever.
LOSSY_SEEDS ?= 1 2 3
os-boot-test-lossy: kernel-main
	python3 scripts/net_lossy_relay.py --selftest
	for smp in 2 4; do for seed in $(LOSSY_SEEDS); do \
	  echo "lossy relay: seed $$seed at -smp $$smp"; \
	  OS_SMP=$$smp OS_NET_LOSSY=$$seed python3 scripts/os_boot_test.py $(OS_ELF) || exit 1; \
	done; done

# The latency trace (slice B task 2; soak tier): a kernel built with `latency-trace` runs the suite at four harts and
# prints the longest interrupts-off span (with its site) and the big-lock holds and waits per syscall; the test
# `the_latency_trace_can_see_a_long_section` proves the instrumentation sees a 5 ms section within 20%.
OS_LATENCY_ELF := target/os-latency/$(KERNEL_TARGET)/debug/kernel
# (also the benchmarks of SCHED-SMP-SPEC 6.1 that need the trace: `irq_off_max`, `bkl_profile` and the `rt_latency` probe, as `##BENCH`
# lines at one and four harts, and the suite's `pair_scaling`, compared with `bench/latency.tsv`; the numbers are the machine's, so the
# threshold is generous like `os-bench`'s)
os-latency: kernel-main
	cd kernel && cargo build --features latency-trace --target-dir ../target/os-latency
	rm -f target/latency.tsv
	OS_BENCH=target/latency.tsv OS_BENCH_FULL=1 OS_SHOW=latency: OS_SMP=4 python3 scripts/os_boot_test.py $(OS_LATENCY_ELF)
	python3 scripts/bench_compare.py target/latency.tsv bench/latency.tsv --max-ratio $(BENCH_RATIO) --scaling-floor bench/scaling-latency.tsv

os-boot-test-aia: kernel
	OS_SMP=2 python3 scripts/os_boot_test.py $(OS_ELF)
	cd kernel && cargo build --features fault-aia-no-unmask --target-dir ../target/os-fault-unmask
	python3 scripts/os_boot_test.py --neg $(OS_NOUNMASK_ELF) interrupt_object_fires_and_masks
	cd kernel && cargo build --features fault-aia-no-repend --target-dir ../target/os-fault-repend
	python3 scripts/os_boot_test.py --neg $(OS_NOREPEND_ELF) interrupt_object_fires_and_masks
	# (IOMMU4, IO12: an interrupt identity freed at the move instead of after its hart retired it)
	cd kernel && cargo build --features fault-msiid-free-at-move --target-dir ../target/os-fault-msiid
	OS_SHARD=k OS_SMP=4 python3 scripts/os_boot_test.py --neg $(OS_NOMSIRETIRE_ELF) interrupt_moves_do_not_lose_or_leak
	# (IOMMU9: the same two controls for an MSI-X vector, D5 through the MRIF)
	cd kernel && cargo build --features fault-vector-no-unmask --target-dir ../target/os-fault-vunmask
	OS_SHARD=k python3 scripts/os_boot_test.py --neg $(OS_VNOUNMASK_ELF) pci_virtio_blk_interrupts_through_msix
	cd kernel && cargo build --features fault-vector-no-repend --target-dir ../target/os-fault-vrepend
	OS_SHARD=k python3 scripts/os_boot_test.py --neg $(OS_VNOREPEND_ELF) pci_virtio_blk_interrupts_through_msix

# The interactive system (S4): the kernel built with the `interactive` feature, and the scripted
# serial check of it.
OS_INTERACTIVE_ELF := target/os-interactive/$(KERNEL_TARGET)/debug/kernel
OS_REPL_ELF := target/os-repl/$(KERNEL_TARGET)/debug/kernel

os-interactive: os-user
	cd kernel && cargo build --features interactive --target-dir ../target/os-interactive
	cd kernel && cargo build --features repl --target-dir ../target/os-repl

# Boot the OS to its shell in this terminal (type `basic`; Ctrl-A then X leaves QEMU).
os-run: os-interactive
	qemu-system-riscv64 -machine virt,aia=aplic-imsic,iommu-sys=on -cpu rva23s64 -bios default -m 256M -nographic -serial mon:stdio -monitor none -kernel $(OS_INTERACTIVE_ELF)

# The system as it is meant to be used (T48): optimised kernel and programs, no boot self-tests, no
# test banners. `make os-release-run` boots it in this terminal; `os-release-smoke` boots it
# headlessly to the shell and a BASIC session so it cannot rot.
OS_RELEASE_ELF := target/os-release-run/$(KERNEL_TARGET)/release/kernel

os-release:
	cd init && cargo build --release
	cd kernel && OS_USER_PROFILE=release CARGO_PROFILE_RELEASE_LTO=fat cargo build --release --features release --target-dir ../target/os-release-run

os-release-run: os-release
	qemu-system-riscv64 -machine virt,aia=aplic-imsic,iommu-sys=on -cpu rva23s64 -bios default -m 256M -nographic -serial mon:stdio -monitor none -kernel $(OS_RELEASE_ELF)

os-release-smoke: os-release
	python3 scripts/release_symbols.py --self-test
	python3 scripts/release_symbols.py $(OS_RELEASE_ELF)
	python3 scripts/release_smoke_test.py $(OS_RELEASE_ELF)
	@if S4_TIMEOUT=15 python3 scripts/release_smoke_test.py $(OS_RELEASE_ELF) --break-expectation >/dev/null 2>&1; then echo "the release smoke test passed a broken expectation"; exit 1; fi

os-serial-test: os-interactive
	python3 scripts/s4a_serial_test.py $(OS_REPL_ELF)
	python3 scripts/s4d_console_test.py $(OS_REPL_ELF)
	python3 scripts/s4e_shell_test.py $(OS_INTERACTIVE_ELF)
	python3 scripts/uv5_sessions_test.py $(OS_REPL_ELF)
	$(MAKE) os-auth-test
	python3 scripts/uv7_detach_test.py $(OS_REPL_ELF)
	@# the harness must be able to fail: a wrong expectation has to be reported
	@if S4_TIMEOUT=15 python3 scripts/s4a_serial_test.py $(OS_REPL_ELF) --break-expectation >/dev/null 2>&1; then echo "the serial test passed a broken expectation"; exit 1; fi
	@if S4_TIMEOUT=15 python3 scripts/s4d_console_test.py $(OS_REPL_ELF) --break-expectation >/dev/null 2>&1; then echo "the console test passed a broken expectation"; exit 1; fi
	@if S4_TIMEOUT=15 python3 scripts/uv5_sessions_test.py $(OS_REPL_ELF) --break-expectation >/dev/null 2>&1; then echo "the sessions test passed a broken expectation"; exit 1; fi
	@if python3 scripts/uv7_detach_test.py $(OS_REPL_ELF) --break-expectation >/dev/null 2>&1; then echo "the detach test passed a broken expectation"; exit 1; fi
	@if S4_TIMEOUT=15 python3 scripts/s4e_shell_test.py $(OS_INTERACTIVE_ELF) --break-expectation >/dev/null 2>&1; then echo "the shell test passed a broken expectation"; exit 1; fi

# PL4c: the image loaded at other physical addresses boots the kernel-only shard: 6 MiB above the default (the same gigabyte, a different
# 2 MiB page) and at 3 GiB + 2 MiB on a 2 GiB machine (another gigabyte, so the early table's identity slot is not the default one). The
# link address does not change; the early boot code works out the difference. The harness reads each ELF's load address itself and refuses
# a build that did not take the address it was asked for.
os-boot-test-loadaddr: os-user
	cd kernel && KERNEL_LMA=0x80600000 cargo build --target-dir ../target/os-loadaddr
	OS_LOAD=0x80600000 OS_SHARD=k OS_SMP=4 python3 scripts/os_boot_test.py target/os-loadaddr/riscv64gc-unknown-none-elf/debug/kernel
	cd kernel && KERNEL_LMA=0xc0200000 cargo build --target-dir ../target/os-loadaddr2
	OS_LOAD=0xc0200000 OS_RAM=2G OS_SHARD=k OS_SMP=4 python3 scripts/os_boot_test.py target/os-loadaddr2/riscv64gc-unknown-none-elf/debug/kernel

# PL5b: all output through the firmware's debug console (SBI DBCN) instead of the UART the tree names: the fallback path for a machine
# whose tree names no 16550, exercised here on one that has it.
os-boot-test-sbi-console: kernel
	OS_BOOT_ARGS=console=sbi OS_SHARD=k OS_SMP=4 python3 scripts/os_boot_test.py $(OS_ELF)

# HD1: the entropy pool differs at every boot; a tree without a seed boots unseeded and passes (needs dtc).
os-boot-test-entropy: kernel
	python3 scripts/entropy_boot_test.py $(OS_ELF)

# The reference CPU of the OS track (OD47).
# PL1 (OD47, OD127): a CPU without the profile must end the boot in `##PANIC missing extension` (QEMU's plain rv64 has no V, Zicond or Svade).
os-boot-test-cpu-neg: kernel
	OS_CPU=rv64 OS_BOOT_TIMEOUT=60 python3 scripts/os_boot_test.py --expect-panic "missing extension" $(OS_ELF)

# IOMMU5 (IOMMU-SPEC 2.5): the machine without its platform IOMMU (no `iommu-sys=on`) must end in `##PANIC ... no IOMMU`.
os-boot-test-iommu-neg: kernel-main
	OS_MACHINE=virt,aia=aplic-imsic OS_PCI_TEST=0 OS_BOOT_TIMEOUT=60 python3 scripts/os_boot_test.py --expect-panic "no IOMMU" $(OS_ELF)

# IOMMU5 negative control (TESTING.md rule 9): a kernel that writes `ddtp` Bare (no translation, no isolation) must fail
# `iommu_is_on_and_denies_by_default`. Kernel-only boot.
OS_IOMMUBARE_ELF := target/os-fault-iommubare/$(KERNEL_TARGET)/debug/kernel
os-boot-test-iommu-bare: kernel-main
	cd kernel && cargo build --features fault-iommu-bare --target-dir ../target/os-fault-iommubare
	OS_SHARD=k OS_SMP=2 OS_BOOT_TIMEOUT=240 python3 scripts/os_boot_test.py --neg $(OS_IOMMUBARE_ELF) iommu_is_on_and_denies_by_default

# IOMMU6 negative control (TESTING.md rule 9): a kernel whose unmap clears the leaves and frees without the invalidation and fence
# must fail `iodomain_unmap_stops_the_device` (the device still reaches the frame through the IOMMU's cache). Kernel-only boot.
OS_IOMMUINVAL_ELF := target/os-fault-iommuinval/$(KERNEL_TARGET)/debug/kernel
os-boot-test-iommu-inval: kernel-main
	cd kernel && cargo build --features fault-iommu-skip-inval --target-dir ../target/os-fault-iommuinval
	OS_SHARD=k OS_SMP=2 OS_BOOT_TIMEOUT=240 python3 scripts/os_boot_test.py --neg $(OS_IOMMUINVAL_ELF) iodomain_unmap_stops_the_device
	# (IOMMU12: the hostile-driver matrix's row that uses its own unmapped mapping is a second test the control must fail)
	OS_SHARD=k OS_SMP=2 OS_BOOT_TIMEOUT=240 python3 scripts/os_boot_test.py --neg $(OS_IOMMUINVAL_ELF) hostile_dma_after_its_own_unmap

# IOMMU7 negative control (TESTING.md rule 9): a kernel that writes basic-translate MSI entries (the device's own data word reaches the
# hart's file) must fail `msi_forgery_faults` (IO11). Kernel-only boot.
OS_IOMMUMSI_ELF := target/os-fault-iommumsi/$(KERNEL_TARGET)/debug/kernel
os-boot-test-iommu-msi: kernel-main
	cd kernel && cargo build --features fault-iommu-msi-basic --target-dir ../target/os-fault-iommumsi
	OS_SHARD=k OS_SMP=2 OS_BOOT_TIMEOUT=240 python3 scripts/os_boot_test.py --neg $(OS_IOMMUMSI_ELF) msi_forgery_faults

# Download third-party sources into third_party/fetched/ (ignored by git; checksums pinned in
# third_party/fetch-manifest.txt). Used by the libc gap survey (UV0-GAP-SURVEY.md); nothing here builds them.
fetch-third-party:
	bash scripts/fetch_third_party.sh

# The libc gap survey (UV0): compile real sources syntax-only against libc/include and list what is undeclared.
# Output in target/libc-gap/. `make libc-gap LIBC_GAP_ARGS="kilo tcc --check"` runs a subset as a gate.
libc-gap: fetch-third-party
	python3 -I scripts/libc_gap_survey.py $(LIBC_GAP_ARGS)

# Contract-document checks (work item T55): known-stale statements and the decision log's bookkeeping.
docs-check:
	python3 scripts/check_docs.py
	python3 scripts/bench_compare.py --self-test
	# IOMMU-SPEC amendment 2026-11-27 part 2 (L7, OD175): the boot harness refuses a device that reads memory at a physical address
	python3 scripts/refused_devices.py --self-test
	python3 -I scripts/net_pcap_check.py net/tests/data/slirp-probe.pcap --self-test
	python3 -I scripts/net_pcap_check.py net/tests/data/slirp-tcp-probe.pcap --self-test

# Everything that is compiled for the target, built and not run: the OS kernel with its user programs, and the home computer's kernel.
# (The optimised build is `os-release`; CI builds it in its own job.)
build: kernel-main home-kernel
	python3 scripts/check_vector_codegen.py

# The commit hook (scripts/hooks/pre-commit): format, the docs check and clippy, on every commit; no tests. Once per clone
# (worktrees share the setting).
hooks:
	git config core.hooksPath scripts/hooks
	@echo "commit hook installed (scripts/hooks/pre-commit)"

# Which commit of an epic broke it? Bisects from the epic's fork point on origin/main to HEAD, one run of TARGET per step
# (default: the kernel-only boot at four harts). `make epic-bisect TARGET=os-boot-test-smoke-a`.
epic-bisect:
	sh scripts/epic_bisect.sh $(or $(TARGET),os-ktest-4)

# Tier 0, the feature gate (OD144, TESTING.md): what a pull request into an epic branch must pass. Format, the docs check, clippy,
# the host tests, the model checkers, and a build of everything for the target. Nothing here starts QEMU (the docs check refuses it).
# CI runs it (the `host`, `models` and `build` jobs); it is not a local routine.
FEATURE_TARGETS := fmt-check docs-check lint test os-models os-aarch64-check build os-release
feature: $(FEATURE_TARGETS)

# Tier 1, the epic gate (OD144, TESTING.md): what an epic branch must pass before it merges into main. The feature gate, then the
# boots: the suite on the default machine at four harts, the kernel-only boots, the instruction counts, the home computer's boot,
# the release smoke.
SMOKE_TARGETS := $(FEATURE_TARGETS) home-boot-test os-boot-test-smoke os-ktest os-ktest-big os-ktest-release os-ktest-release-content os-ktest-entry-neg os-ktest-shuffle os-count os-release-smoke
smoke: $(SMOKE_TARGETS)

# Tier 2 (soak): the full matrix, every negative control, the serial sessions. Never required, and run ONLY when the
# owner asks for it (OD142): not nightly, not on a label, not by an agent on its own judgement. The boots are independent (each makes its own disk), so they run
# in parallel (SOAK_JOBS, default 3; the tests follow the host clock, so do not oversubscribe a small machine).
SOAK_TARGETS := os-bench os-latency os-boot-test os-boot-test-overflow os-boot-test-smp os-boot-test-smp-neg os-boot-test-sched-neg os-boot-test-nodisk os-boot-test-aia os-boot-test-cpu-neg os-boot-test-iommu-neg os-boot-test-iommu-bare os-boot-test-iommu-inval os-boot-test-iommu-msi os-boot-test-loadaddr os-boot-test-sbi-console os-boot-test-entropy os-boot-test-lossy os-serial-test
SOAK_JOBS ?= 3
soak:
	$(MAKE) -j$(SOAK_JOBS) $(SOAK_TARGETS)

# Tier 3 (overnight): hours-long runs, started ONLY by the owner (OD142, KT6): `overnight.yml` has a manual trigger and nothing else.
# Each KT item that adds a long run adds its target here, and `overnight.yml` runs it as a job. Never required to merge.
OVERNIGHT_TARGETS := overnight-repeat-smp os-boot-test-checked os-chaos os-mutants os-coverage os-fuzz-host os-allocfail
# 20 consecutive boots of the smp suite: the intermittent-failure hunt of OD140 (a red run is a work item the same day).
overnight-repeat-smp:
	$(MAKE) os-repeat TARGET=os-boot-test-smp N=20

# KT12: the release kernel with the debug assertions on, through the whole suite at four harts and the kernel-only shard at one, two and eight.
OS_KCHK_ELF := target/os-kchecked/$(KERNEL_TARGET)/release-checked/kernel
os-boot-test-checked:
	$(MAKE) os-kchecked-build
	OS_SHARD= OS_SMP=4 python3 scripts/os_boot_test.py $(OS_KCHK_ELF)
	OS_SHARD=k OS_SMP=1 python3 scripts/os_boot_test.py $(OS_KCHK_ELF)
	OS_SHARD=k OS_SMP=2 python3 scripts/os_boot_test.py $(OS_KCHK_ELF)
	OS_SHARD=k OS_SMP=8 python3 scripts/os_boot_test.py $(OS_KCHK_ELF)
os-kchecked-build:
	cd init && cargo build --release
	cd kernel && OS_USER_PROFILE=release cargo build --profile release-checked --target-dir ../target/os-kchecked

# KT7: schedule perturbation. The kernel-only shard on a `chaos` kernel, one boot per seed and hart count (`SEEDS="1..200"` by default);
# a failure prints its seed and keeps its log under target/chaos/. Overnight.
CHAOS_SEEDS ?= 1..200
OS_CHAOS_ELF := target/os-chaos/$(KERNEL_TARGET)/debug/kernel
os-chaos:
	cd init && cargo build
	cd kernel && cargo build --features chaos --target-dir ../target/os-chaos
	python3 scripts/chaos.py $(OS_CHAOS_ELF) "$(CHAOS_SEEDS)"

# KT10: mutation testing of kcore, klock, iommucore, pcicore and virtio with their host tests as the oracle (needs `cargo install cargo-mutants --locked --version 25.3.1`).
# `make os-mutants` runs the three crates; `make os-mutants PKG=kcore SHARD=3/8` one slice, which is how overnight.yml fits the job budget.
# Fails on a surviving mutant that is not in mutants-accepted.txt with a reason. Hours: the owner starts it (OD142).
os-mutants:
ifeq ($(PKG),)
	python3 scripts/mutants.py klock
	python3 scripts/mutants.py iommucore
	python3 scripts/mutants.py pcicore
	python3 scripts/mutants.py virtio
	python3 scripts/mutants.py kcore
else
	python3 scripts/mutants.py $(PKG) $(if $(SHARD),--shard $(SHARD),)
endif

# KT11: line and region coverage of the pure kernel crates and the syscall layer (needs `rustup component add llvm-tools-preview` and
# `cargo install cargo-llvm-cov --locked --version 0.9.2`). The summary goes to the job summary, the HTML report to target/cov/html (an
# artifact in CI). Not a gate. (0.6.16 cannot read the coverage data of rustc 1.100 and reports 0 percent everywhere: found by the first overnight
# run, 2026-11-21; with 0.9.2 the host tests of the four crates cover 90.4 percent of regions and 91.3 percent of lines.)
os-coverage:
	cargo llvm-cov -p kcore -p klock -p abi -p ksys -p iommucore --summary-only
	cargo llvm-cov report --html --output-dir target/cov

# KT8: the host fuzzer of the syscall layer (`ksys/tests/fuzz.rs`, sequences from `kfuzz-gen`, audits after every call) over many seeds. A failing
# seed is named and replays with `KFUZZ_SEEDS=<seed> cargo test -p ksys --test fuzz`. `cargo test` runs the same test with 16 short sequences.
FUZZ_SEEDS ?= 0..20000
FUZZ_CALLS ?= 600
os-fuzz-host:
	KFUZZ_SEEDS="$(FUZZ_SEEDS)" KFUZZ_CALLS=$(FUZZ_CALLS) cargo test --profile release-checked -p ksys --test fuzz sequences

# KT9: the allocation-failure sweep of the syscall layer in both modes (one allocation fails; every allocation from the N-th on fails). `cargo test`
# runs the first mode only. A failing case replays with `KALLOC_CHILD=<scenario>:<n>[:persist] cargo test -p ksys --test allocfail child_entry -- --exact --nocapture`.
os-allocfail: os-user
	KALLOC_PERSIST=1 cargo test --profile release-checked -p ksys --test allocfail every -- --nocapture
	# the guest half: a kernel whose heap fails the N-th allocation of an armed thread, and the suite's `kernel_allocation_failure_sweep` over six scenarios
	cd kernel && cargo build --features fault-alloc --target-dir ../target/os-fault-alloc
	OS_REQUIRE_LINE="alloc sweep: map and touch" OS_BOOT_ARGS="kaslr=off aslr=off" OS_SHARD=k OS_SMP=1 OS_BOOT_TIMEOUT=300 python3 scripts/os_boot_test.py target/os-fault-alloc/$(KERNEL_TARGET)/debug/kernel

# `check` is the epic gate, tiers 0 and 1.
check: smoke

# Everything: smoke, then soak.
check-all: smoke soak

# UV5a: `authd` and the login policy, on three boot images (tests/uv5a overlays): no profile at all, and seed credentials.
OS_AUTH_EMPTY_ELF := target/os-auth-empty/$(KERNEL_TARGET)/debug/kernel
OS_AUTH_SEED_ELF := target/os-auth-seed/$(KERNEL_TARGET)/debug/kernel

os-auth-test: os-user
	cd kernel && OS_BOOTFS_OVERLAY=$(CURDIR)/tests/uv5a/empty cargo build --features interactive --target-dir ../target/os-auth-empty
	cd kernel && OS_BOOTFS_OVERLAY=$(CURDIR)/tests/uv5a/seed cargo build --features interactive --target-dir ../target/os-auth-seed
	python3 scripts/uv5a_auth_test.py $(OS_AUTH_EMPTY_ELF) $(OS_AUTH_SEED_ELF)
	@if S4_TIMEOUT=15 python3 scripts/uv5a_auth_test.py $(OS_AUTH_EMPTY_ELF) $(OS_AUTH_SEED_ELF) --break-expectation >/dev/null 2>&1; then echo "the auth test passed a broken expectation"; exit 1; fi
