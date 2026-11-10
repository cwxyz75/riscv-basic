KERNEL_TARGET := riscv64gc-unknown-none-elf
KERNEL_ELF    := target/$(KERNEL_TARGET)/debug/kernel
QEMU          := qemu-system-riscv64
QEMU_FLAGS    := -machine virt -nographic -bios default

.PHONY: help test fmt fmt-check lint kernel run debug boot-test check

help:
	@echo "make test       host tests for the interpreter crate (no QEMU needed)"
	@echo "make kernel     build the kernel ELF"
	@echo "make run        boot the kernel in QEMU (Ctrl-A then X to exit)"
	@echo "make debug      boot paused with a gdb stub on :1234"
	@echo "make boot-test  boot in QEMU non-interactively and check the output"
	@echo "make check      everything CI runs"

test:
	cargo test -p basic -p console

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

lint:
	cargo clippy -p basic -p console --all-targets -- -D warnings
	cd kernel && cargo clippy -- -D warnings

kernel:
	cd kernel && cargo build

run: kernel
	$(QEMU) $(QEMU_FLAGS) -kernel $(KERNEL_ELF)

debug: kernel
	$(QEMU) $(QEMU_FLAGS) -kernel $(KERNEL_ELF) -s -S

boot-test: kernel
	python3 scripts/console_test.py $(KERNEL_ELF)
	python3 scripts/trap_test.py $(KERNEL_ELF)
	python3 scripts/basic_test.py $(KERNEL_ELF)
	python3 scripts/demo_test.py $(KERNEL_ELF)
	python3 scripts/menu_test.py $(KERNEL_ELF)

check: fmt-check test lint boot-test
