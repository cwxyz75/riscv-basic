KERNEL_TARGET := riscv64gc-unknown-none-elf
KERNEL_ELF    := target/$(KERNEL_TARGET)/debug/kernel
QEMU          := qemu-system-riscv64
QEMU_FLAGS    := -machine virt -nographic -bios default

# The disk for SAVE/LOAD: a raw image on a virtio block device. The kernel's
# driver speaks the modern virtio-mmio transport, hence force-legacy=false.
DISK          := disk.img
DISK_FLAGS    := -global virtio-mmio.force-legacy=false -drive file=$(DISK),if=none,format=raw,id=hd0 -device virtio-blk-device,drive=hd0

.PHONY: help disk test fmt fmt-check lint kernel run run-logo debug boot-test check

help:
	@echo "make test       host tests for the interpreter crate (no QEMU needed)"
	@echo "make kernel     build the kernel ELF"
	@echo "make run        boot the kernel in QEMU (Ctrl-A then X to exit)"
	@echo "make run-logo    boot with a graphics window (ramfb); press 2 at the menu for LOGO"
	@echo "make debug      boot paused with a gdb stub on :1234"
	@echo "make boot-test  boot in QEMU non-interactively and check the output"
	@echo "make disk       create disk.img (2 MiB, for SAVE/LOAD) if it does not exist"
	@echo "make check      everything CI runs"

test:
	cargo test -p basic -p cartfs -p console -p turtle -p logo -p rv64
	python3 scripts/verify_demo_images.py

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

lint:
	cargo clippy -p basic -p cartfs -p console -p turtle -p logo -p rv64 --all-targets -- -D warnings
	cd kernel && cargo clippy -- -D warnings

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

check: fmt-check test lint boot-test
