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

#[cfg(test)]
mod tests {
    use super::finisher;

    #[test]
    fn finisher_values_match_the_device_documentation() {
        assert_eq!(finisher::PASS, 0x5555);
        assert_eq!(finisher::fail(1), 0x0001_3333);
        assert_eq!(finisher::fail(0xffff), 0xffff_3333);
    }
}
