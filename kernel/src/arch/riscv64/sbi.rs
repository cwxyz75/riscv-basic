//! Firmware (SBI) calls and the secondary harts' entry assembly (AR1).

use core::arch::{asm, global_asm};

pub const EID_HSM: usize = 0x48_534D;
pub const FID_HART_START: usize = 0;
pub const FID_HART_GET_STATUS: usize = 2;
pub const FID_HART_SUSPEND: usize = 3;
/// SBI error: the call is not supported.
pub const SBI_ERR_NOT_SUPPORTED: isize = -2;
pub const EID_RFENCE: usize = 0x5246_4E43;
pub const FID_REMOTE_FENCE_I: usize = 0;
pub const FID_REMOTE_SFENCE_VMA: usize = 1;
pub const EID_IPI: usize = 0x73_5049;
/// The base extension (function 3 probes for another extension) and the legacy console putchar call.
pub const EID_BASE: usize = 0x10;
pub const FID_PROBE_EXTENSION: usize = 3;
pub const EID_LEGACY_PUTCHAR: usize = 1;
/// The debug console extension (DBCN), function write_byte.
pub const EID_DBCN: usize = 0x4442_434E;
pub const FID_DBCN_WRITE_BYTE: usize = 2;
/// The system reset extension (SRST), function system_reset (a0: 0 shutdown, 1 cold reboot; a1: 0 no reason, 1 system failure).
pub const EID_SRST: usize = 0x5352_5354;
pub const FID_SRST_RESET: usize = 0;
/// The system suspend extension (SUSP; PM3).
pub const EID_SUSP: usize = 0x5355_5350;

/// Does the firmware have the system suspend extension? Asked of the base extension's `probe_extension`.
pub fn system_suspend_present() -> bool {
    let probe = sbi(EID_BASE, FID_PROBE_EXTENSION, EID_SUSP, 0, 0);
    probe.error == 0 && probe.value != 0
}

/// `hart_get_status` result for a hart that exists and is waiting to be started.
pub const HSM_STOPPED: usize = 1;

pub struct SbiRet {
    pub error: isize,
    pub value: usize,
}

pub fn sbi(eid: usize, fid: usize, a0: usize, a1: usize, a2: usize) -> SbiRet {
    sbi5(eid, fid, [a0, a1, a2, 0, 0])
}

/// Calls made to the IPI extension: the kernel interrupts harts through the IMSIC (PL6), so this stays 0 (`debug_stat` 106).
static IPI_CALLS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

pub fn ipi_calls() -> usize {
    IPI_CALLS.load(core::sync::atomic::Ordering::Relaxed)
}

pub fn sbi5(eid: usize, fid: usize, a: [usize; 5]) -> SbiRet {
    if eid == EID_IPI {
        IPI_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
    let (error, value): (isize, usize);
    // SAFETY: `ecall` from S-mode into OpenSBI with the standard calling convention (a7 =
    // extension, a6 = function, a0-a4 arguments, results in a0/a1). Callers pass fixed extension and function ids and arguments the kernel derived
    // itself (hart ids, addresses it owns), so firmware sees only requests the kernel means to make. The function itself takes any ids (`hart_start` names a start address): the argument is that every caller passes its own fixed ones (`smp.rs`, the probe, the console and reset paths: KT14).
    unsafe {
        asm!(
            "ecall",
            in("a7") eid,
            in("a6") fid,
            inlateout("a0") a[0] => error,
            inlateout("a1") a[1] => value,
            in("a2") a[2],
            in("a3") a[3],
            in("a4") a[4],
        );
    }
    SbiRet { error, value }
}

/// Whether the firmware has the debug console extension: 0 not asked yet, 1 yes, 2 no (then the legacy console putchar call is used).
static DBCN: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// Write one byte to the firmware's console: SBI DBCN `write_byte` when the firmware has the extension (probed once), else the legacy
/// `console_putchar` call (SBI 0.1, which every firmware still answers). This is the console when the device tree names no UART the kernel
/// can drive. `false` if the firmware refused the byte; the console counts refusals.
pub fn debug_console_write_byte(byte: u8) -> bool {
    use core::sync::atomic::Ordering;
    let mut state = DBCN.load(Ordering::Relaxed);
    if state == 0 {
        // base extension, function 3: probe_extension(DBCN); a non-zero value means present
        let probe = sbi(EID_BASE, FID_PROBE_EXTENSION, EID_DBCN, 0, 0);
        state = if probe.error == 0 && probe.value != 0 {
            1
        } else {
            2
        };
        DBCN.store(state, Ordering::Relaxed);
    }
    if state == 1 {
        sbi(EID_DBCN, FID_DBCN_WRITE_BYTE, usize::from(byte), 0, 0).error == 0
    } else {
        // legacy call 1: console_putchar(a0); it answers in a0 (0 for success)
        sbi(EID_LEGACY_PUTCHAR, 0, usize::from(byte), 0, 0).error == 0
    }
}

/// Ask the firmware to end the run: a shutdown, or a cold reboot (SBI SRST). `failure` is the reason code the firmware may report.
/// Returns only if the firmware refuses (a machine without the extension).
pub fn system_reset(reboot: bool, failure: bool) {
    let _ = sbi(
        EID_SRST,
        FID_SRST_RESET,
        usize::from(reboot),
        usize::from(failure),
        0,
    );
}

global_asm!(
    r#"
    .section .text
    .globl secondary_entry
    .balign 4
secondary_entry:                 # a0 = hart id, a1 = dense hart index; paging off, interrupts off
    mv   s0, a0
    mv   s1, a1
    # The kernel's own table has no identity mapping, so first use the early boot table (still
    # intact in .bss: identity for RAM, plus the upper-half image), then jump to the upper half.
    lla   t1, KERNEL_DELTA        # where the image is linked minus where it is, worked out by the boot hart's `_start`
    ld   s2, 0(t1)               # (read with paging off: no mapping is needed to see it, wherever the image is)
    lla   t2, EARLY_ROOT
    srli t3, t2, 12
    li   t4, 8
    slli t4, t4, 60              # satp.MODE = 8 (Sv39)
    or   t3, t3, t4
    csrw satp, t3
    sfence.vma zero, zero
    lla   t0, 3f
    add  t0, t0, s2
    jr   t0
3:
    lla   t0, SECONDARY_STACK_TOPS
    slli t1, s1, 3
    add  t0, t0, t1
    ld   sp, 0(t0)
    mv   a0, s0
    mv   a1, s1
    call secondary_main
4:
    wfi
    j    4b
"#
);

extern "C" {
    /// The entry point a secondary hart starts at (a0 = hart id, a1 = dense index; paging off).
    pub fn secondary_entry();
}
