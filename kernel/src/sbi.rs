//! Calls into OpenSBI, the M-mode firmware under the kernel (decision D2).

use core::arch::asm;

const EID_TIME: usize = 0x5449_4D45; // "TIME"
const FID_SET_TIMER: usize = 0;
const EID_SRST: usize = 0x5352_5354; // "SRST"
const FID_SYSTEM_RESET: usize = 0;
const RESET_TYPE_SHUTDOWN: usize = 0;
const RESET_REASON_NONE: usize = 0;

/// Make the machine raise a supervisor timer interrupt when the `time` counter
/// reaches `deadline` (TIME extension, `sbi_set_timer`).
pub fn set_timer(deadline: u64) {
    // SAFETY: `ecall` from S-mode traps into OpenSBI, which implements the TIME
    // extension; arguments and clobbers follow the SBI calling convention
    // (a7 = extension, a6 = function, a0.. = arguments, a0/a1 = results).
    unsafe {
        asm!(
            "ecall",
            in("a7") EID_TIME,
            in("a6") FID_SET_TIMER,
            inout("a0") deadline as usize => _,
            out("a1") _,
        );
    }
}

/// Ask OpenSBI to power the machine off (System Reset extension).
pub fn shutdown() -> ! {
    // SAFETY: as for `set_timer`, using the SRST extension.
    unsafe {
        asm!(
            "ecall",
            in("a7") EID_SRST,
            in("a6") FID_SYSTEM_RESET,
            inout("a0") RESET_TYPE_SHUTDOWN => _,
            inout("a1") RESET_REASON_NONE => _,
        );
    }
    loop {
        // SAFETY: `wfi` just idles the hart until the next interrupt.
        unsafe { asm!("wfi") }
    }
}
