KERNEL_TARGET := riscv64gc-unknown-none-elf
HOME_ELF      := target/$(KERNEL_TARGET)/debug/home-kernel
QEMU          := qemu-system-riscv64
QEMU_FLAGS    := -machine virt -nographic -bios default

# The disk for SAVE/LOAD: a raw image on a virtio block device. The kernel's
# driver speaks the modern virtio-mmio transport, hence force-legacy=false.
DISK          := disk.img
DISK_FLAGS    := -global virtio-mmio.force-legacy=false -drive file=$(DISK),if=none,format=raw,id=hd0 -device virtio-blk-device,drive=hd0

.PHONY: help disk os-user kernel os-test os-miri os-boot-test os-boot-test-main os-boot-test-rr os-boot-test-aia os-boot-test-rva23 os-interactive os-serial-test test fmt fmt-check lint home-kernel home-run home-run-logo home-debug home-boot-test check

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
	@echo "make check      everything CI runs"

test:
	cargo test -p abi -p kcore -p proto -p memfs -p basic -p cartfs -p console -p turtle -p logo -p rv64
	python3 home/scripts/verify_demo_images.py

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

lint:
	cargo clippy -p abi -p kcore -p proto -p memfs -p basic -p cartfs -p console -p turtle -p logo -p rv64 --all-targets -- -D warnings
	cd home/kernel && cargo clippy -- -D warnings
	cd init && cargo clippy -p rt -p init -- -D warnings
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

# The OS kernel, plus a build with `fail-demo` (one test fails on purpose) that the
# boot test uses to prove it can see a failure.
# The user programs (init, crash) are built for the no-FPU user target and embedded in the
# kernel image, so they must exist before the kernel is built.
os-user:
	cd init && cargo build

kernel: os-user
	cd kernel && cargo build
	cd kernel && cargo build --features fail-demo --target-dir ../target/os-fail
	cd kernel && cargo build --features sched-round-robin --target-dir ../target/os-rr

# Host tests of the OS crates: plain, then the same code under the loom and
# shuttle model checkers (separate target dirs: the cfg flags rebuild everything).
os-test:
	cargo test -p abi -p kcore
	RUSTFLAGS="--cfg loom" cargo test -p kcore --release --test latch_concurrency --target-dir target/loom
	RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test latch_concurrency --target-dir target/shuttle

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

os-boot-test: kernel
	python3 scripts/os_boot_test.py $(OS_ELF) $(OS_FAIL_ELF) $(OS_RR_ELF)

# The same, split so CI can run the parts as separate jobs (a slow or flaky one does not hide the
# others): the normal and fail-demo kernels, and the round-robin kernel.
os-boot-test-main: kernel
	python3 scripts/os_boot_test.py $(OS_ELF) $(OS_FAIL_ELF)

os-boot-test-rr: kernel
	python3 scripts/os_boot_test.py --rr $(OS_RR_ELF)

# The same boot test on QEMU's AIA interrupt controller (APLIC + IMSIC) instead of the
# PLIC (OD35). Needs QEMU with `-machine virt,aia=...` (7.2 or later).
os-boot-test-aia: kernel
	OS_MACHINE=virt,aia=aplic-imsic python3 scripts/os_boot_test.py $(OS_ELF)

# The interactive system (S4): the kernel built with the `interactive` feature, and the scripted
# serial check of it.
OS_INTERACTIVE_ELF := target/os-interactive/$(KERNEL_TARGET)/debug/kernel

os-interactive: os-user
	cd kernel && cargo build --features interactive --target-dir ../target/os-interactive

os-serial-test: os-interactive
	python3 scripts/s4a_serial_test.py $(OS_INTERACTIVE_ELF)
	@# the harness must be able to fail: a wrong expectation has to be reported
	@if S4_TIMEOUT=15 python3 scripts/s4a_serial_test.py $(OS_INTERACTIVE_ELF) --break-expectation >/dev/null 2>&1; then echo "the serial test passed a broken expectation"; exit 1; fi

# The reference CPU of the OS track (OD47).
os-boot-test-rva23: kernel
	OS_CPU=rva23s64 python3 scripts/os_boot_test.py $(OS_ELF)

check: fmt-check test lint home-boot-test os-test os-boot-test os-boot-test-aia os-boot-test-rva23 os-serial-test
