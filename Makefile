KERNEL_TARGET := riscv64gc-unknown-none-elf
KERNEL_ELF    := target/$(KERNEL_TARGET)/debug/kernel
QEMU          := qemu-system-riscv64
QEMU_FLAGS    := -machine virt -nographic -bios default

# The disk for SAVE/LOAD: a raw image on a virtio block device. The kernel's
# driver speaks the modern virtio-mmio transport, hence force-legacy=false.
DISK          := disk.img
DISK_FLAGS    := -global virtio-mmio.force-legacy=false -drive file=$(DISK),if=none,format=raw,id=hd0 -device virtio-blk-device,drive=hd0

.PHONY: help disk os-kernel os-test os-miri os-boot-test test fmt fmt-check lint kernel run run-logo debug boot-test check

help:
	@echo "make test       host tests for the interpreter crate (no QEMU needed)"
	@echo "make kernel     build the kernel ELF"
	@echo "make run        boot the kernel in QEMU (Ctrl-A then X to exit)"
	@echo "make run-logo    boot with a graphics window (ramfb); press 2 at the menu for LOGO"
	@echo "make debug      boot paused with a gdb stub on :1234"
	@echo "make boot-test  boot in QEMU non-interactively and check the output"
	@echo "make disk       create disk.img (2 MiB, for SAVE/LOAD) if it does not exist"
	@echo "make os-test     OS track: host tests, plus loom and shuttle (os-miri adds Miri)"
	@echo "make os-boot-test OS track: boot the stub kernel headlessly, check its structured output"
	@echo "make check      everything CI runs"

test:
	cargo test -p abi -p kcore -p basic -p cartfs -p console -p turtle -p logo -p rv64
	python3 scripts/verify_demo_images.py

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

lint:
	cargo clippy -p abi -p kcore -p basic -p cartfs -p console -p turtle -p logo -p rv64 --all-targets -- -D warnings
	cd kernel && cargo clippy -- -D warnings
	cd os-kernel && cargo clippy -- -D warnings

kernel:
	cd kernel && cargo build

# Created only if missing (no prerequisites), so your saved files are never wiped.
$(DISK):
	python3 scripts/disk.py new $(DISK)

disk: $(DISK)

run: kernel $(DISK)
	$(QEMU) $(QEMU_FLAGS) $(DISK_FLAGS) -kernel $(KERNEL_ELF)

# Same machine plus a framebuffer, shown in a window (QEMU picks the display
# backend). The serial console stays in this terminal; Ctrl-A then X quits.
# On macOS the Cocoa window can show the screen magnified 2x and cropped to one
# quadrant on a Retina display; zoom-to-fit scales the whole screen to the window.
ifeq ($(shell uname),Darwin)
LOGO_DISPLAY := -display cocoa,zoom-to-fit=on
endif

run-logo: kernel $(DISK)
	$(QEMU) -machine virt -bios default $(DISK_FLAGS) -device ramfb $(LOGO_DISPLAY) -serial mon:stdio -kernel $(KERNEL_ELF)

debug: kernel $(DISK)
	$(QEMU) $(QEMU_FLAGS) $(DISK_FLAGS) -kernel $(KERNEL_ELF) -s -S

boot-test: kernel
	python3 scripts/console_test.py $(KERNEL_ELF)
	python3 scripts/trap_test.py $(KERNEL_ELF)
	python3 scripts/basic_test.py $(KERNEL_ELF)
	python3 scripts/demo_test.py $(KERNEL_ELF)
	python3 scripts/menu_test.py $(KERNEL_ELF)
	python3 scripts/logo_test.py $(KERNEL_ELF)
	python3 scripts/jit_test.py $(KERNEL_ELF)
	python3 scripts/logo_diff_test.py $(KERNEL_ELF)
	python3 scripts/disk_test.py $(KERNEL_ELF)

# --- OS track (S-series; see OS-ARCHITECTURE.md) ---
OS_ELF      := target/$(KERNEL_TARGET)/debug/os-kernel
OS_FAIL_ELF := target/os-fail/$(KERNEL_TARGET)/debug/os-kernel

# The OS kernel, plus a build with `fail-demo` (one test fails on purpose) that the
# boot test uses to prove it can see a failure.
os-kernel:
	cd os-kernel && cargo build
	cd os-kernel && cargo build --features fail-demo --target-dir ../target/os-fail

# Host tests of the OS crates: plain, then the same code under the loom and
# shuttle model checkers (separate target dirs: the cfg flags rebuild everything).
os-test:
	cargo test -p abi -p kcore
	RUSTFLAGS="--cfg loom" cargo test -p kcore --release --test latch_concurrency --target-dir target/loom
	RUSTFLAGS="--cfg shuttle" cargo test -p kcore --release --test latch_concurrency --target-dir target/shuttle

# Miri needs a nightly toolchain with the miri component (rustup toolchain install
# nightly --profile minimal --component miri); CI runs it, `make check` does not.
os-miri:
	cargo +nightly miri test -p abi -p kcore --target-dir target/miri

os-boot-test: os-kernel
	python3 scripts/os_boot_test.py $(OS_ELF) $(OS_FAIL_ELF)

check: fmt-check test lint boot-test os-test os-boot-test
