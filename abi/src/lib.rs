//! The contract between the OS-track kernel and everything that talks to it.
//! S0 holds only what the boot test needs; the syscall table arrives with S3 and
//! is specified in `SYSCALLS.md` first.
#![no_std]
#![forbid(unsafe_code)]

/// QEMU `virt`'s test finisher (a SiFive test device): writing a value here ends
/// the emulation with an exit status.
pub mod finisher {
    /// Physical address of the device.
    pub const BASE: usize = 0x10_0000;
    /// Write this to exit QEMU with status 0.
    pub const PASS: u32 = 0x5555;
    /// The value that exits QEMU with status `code` (1 to 0xffff).
    pub const fn fail(code: u16) -> u32 {
        ((code as u32) << 16) | 0x3333
    }
}

/// The kernel's address-space layout (Sv39, kernel in the upper half).
pub mod layout {
    /// Where OpenSBI puts the kernel image in physical memory.
    pub const KERNEL_PHYS: u64 = 0x8020_0000;
    /// Add to a physical address inside the kernel image to get its virtual address.
    pub const KERNEL_OFFSET: u64 = 0xffff_ffff_0000_0000;
    /// Start of the direct map: physical address `pa` is at `DIRECT_BASE + pa`
    /// (RAM outside the kernel image, plus the device pages the kernel uses).
    pub const DIRECT_BASE: u64 = 0xffff_ffc0_0000_0000;
    /// Physical RAM on QEMU's `virt` board starts here.
    pub const RAM_BASE: u64 = 0x8000_0000;
    /// RAM size the kernel assumes (the boot test passes `-m 128M`). Reading it
    /// from the device tree arrives with the device manager (S6).
    pub const RAM_SIZE: u64 = 128 * 1024 * 1024;
    /// First virtual address of the region the self-tests use for lazily
    /// committed memory (an otherwise empty top-level slot in the upper half).
    pub const LAZY_TEST_BASE: u64 = 0xffff_ffd0_0000_0000;
}

#[cfg(test)]
mod tests {
    use super::finisher;

    #[test]
    fn layout_is_consistent() {
        use super::layout::*;
        // The kernel image is reachable at KERNEL_PHYS + KERNEL_OFFSET = 0xffff_ffff_8020_0000.
        assert_eq!(KERNEL_PHYS + KERNEL_OFFSET, 0xffff_ffff_8020_0000);
        // Sv39 upper half: bit 38 and everything above set.
        for va in [DIRECT_BASE, KERNEL_PHYS + KERNEL_OFFSET, LAZY_TEST_BASE] {
            assert_eq!(
                (va as i64) >> 38,
                -1,
                "{va:#x} is not in the Sv39 upper half"
            );
        }
        // The direct map and the lazy test area use top-level slots of their own.
        let slot = |va: u64| (va >> 30) & 0x1ff;
        assert_eq!(slot(DIRECT_BASE), 256);
        assert_eq!(slot(KERNEL_PHYS + KERNEL_OFFSET), 510);
        assert_eq!(slot(LAZY_TEST_BASE), 320);
        assert_eq!(RAM_BASE % 4096, 0);
    }

    #[test]
    fn finisher_values_match_the_device_documentation() {
        assert_eq!(finisher::PASS, 0x5555);
        assert_eq!(finisher::fail(1), 0x0001_3333);
        assert_eq!(finisher::fail(0xffff), 0xffff_3333);
    }
}
