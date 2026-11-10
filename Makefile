KERNEL_TARGET := riscv64gc-unknown-none-elf
KERNEL_ELF    := target/$(KERNEL_TARGET)/debug/kernel
QEMU          := qemu-system-riscv64
QEMU_FLAGS    := -machine virt -nographic -bios default

.PHONY: help test fmt fmt-check lint kernel run run-logo debug boot-test check

help:
	@echo "make test       host tests for the interpreter crate (no QEMU needed)"
	@echo "make kernel     build the kernel ELF"
	@echo "make run        boot the kernel in QEMU (Ctrl-A then X to exit)"
	@echo "make run-logo    boot with a graphics window (ramfb); press 2 at the menu for LOGO"
	@echo "make debug      boot paused with a gdb stub on :1234"
	@echo "make boot-test  boot in QEMU non-interactively and check the output"
	@echo "make check      everything CI runs"

test:
	cargo test -p basic -p console -p turtle -p logo -p rv64
	python3 scripts/verify_demo_images.py

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

lint:
	cargo clippy -p basic -p console -p turtle -p logo -p rv64 --all-targets -- -D warnings
	cd kernel && cargo clippy -- -D warnings

kernel:
	cd kernel && cargo build

run: kernel
	$(QEMU) $(QEMU_FLAGS) -kernel $(KERNEL_ELF)

# Same machine plus a framebuffer, shown in a window (QEMU picks the display
# backend). The serial console stays in this terminal; Ctrl-A then X quits.
run-logo: kernel
	$(QEMU) -machine virt -bios default -device ramfb -serial mon:stdio -kernel $(KERNEL_ELF)

debug: kernel
	$(QEMU) $(QEMU_FLAGS) -kernel $(KERNEL_ELF) -s -S

boot-test: kernel
	python3 scripts/console_test.py $(KERNEL_ELF)
	python3 scripts/trap_test.py $(KERNEL_ELF)
	python3 scripts/basic_test.py $(KERNEL_ELF)
	python3 scripts/demo_test.py $(KERNEL_ELF)
	python3 scripts/menu_test.py $(KERNEL_ELF)
	python3 scripts/logo_test.py $(KERNEL_ELF)
	python3 scripts/jit_test.py $(KERNEL_ELF)
	python3 scripts/logo_diff_test.py $(KERNEL_ELF)

check: fmt-check test lint boot-test
