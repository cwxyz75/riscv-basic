//! The AArch64 stub (AR1d, KERNEL-V1-SPEC 2.6): every item of the architecture interface is present and every body is
//! `unimplemented!()`. It carries no behaviour and is never booted. Its only job is to make
//! `cargo check --target aarch64-unknown-none` fail when code outside `arch/` reaches for something the RISC-V directory has and
//! this one lacks, which proves the boundary is complete. A real AArch64 port replaces each body and needs its own profile-floor
//! decision beside OD127 first. Rule for every later item: an interface item added to `arch/riscv64` is added here in the same PR.
#![allow(dead_code, unused_variables, clippy::missing_safety_doc)]

pub mod aia {
    pub const PAGE_COUNT: usize = 0;
    pub fn discover(nodes: &[kcore::fdt::Node], ram: &[(u64, u64)]) {
        unimplemented!()
    }
    pub const IPI_BIT: u64 = 0;
    pub fn lists_hart(hart_id: usize) -> bool {
        unimplemented!()
    }
    pub fn send_ipi(hart_id: usize) {
        unimplemented!()
    }
    pub fn retarget_from(from: usize, to: usize) {
        unimplemented!()
    }
    pub fn claim() -> u64 {
        unimplemented!()
    }
    pub fn deliver(mask: u64, fired: impl FnMut(u32) -> bool) -> bool {
        unimplemented!()
    }
    pub fn slots() -> usize {
        unimplemented!()
    }
    pub fn irq_allowed(irq: u32) -> bool {
        unimplemented!()
    }
    pub fn pages() -> impl Iterator<Item = u64> {
        core::iter::empty()
    }
    pub fn stat(which: u64) -> u64 {
        unimplemented!()
    }
    pub fn present() -> bool {
        unimplemented!()
    }
    pub fn init() {
        unimplemented!()
    }
    pub fn init_hart() {
        unimplemented!()
    }
    pub fn enable(irq: u32) {
        unimplemented!()
    }
    pub fn reenable(irq: u32) {
        unimplemented!()
    }
    pub fn disable(irq: u32) {
        unimplemented!()
    }
    pub fn handle(fired: impl FnMut(u32) -> bool) -> bool {
        unimplemented!()
    }
}

pub mod finisher {
    pub fn init(base: Option<u64>) {
        unimplemented!()
    }
    pub fn base() -> Option<u64> {
        unimplemented!()
    }
    pub fn exit(ok: bool) -> ! {
        unimplemented!()
    }
}

pub mod fp {
    use super::trap::TrapFrame;
    pub fn trap_entry(frame: &mut TrapFrame) {
        unimplemented!()
    }
    pub fn return_to_user_held(frame: &mut TrapFrame) {
        unimplemented!()
    }
    pub fn return_to_user(frame: &mut TrapFrame) {
        unimplemented!()
    }
    pub fn first_use(frame: &mut TrapFrame) -> bool {
        unimplemented!()
    }
    pub fn init_boot_hart() {
        unimplemented!()
    }
    pub fn enable_cache_block_ops_or_stop() {
        unimplemented!()
    }
    pub fn check_hart() {
        unimplemented!()
    }
    pub fn thread_gone(tid: u64) {
        unimplemented!()
    }
    pub fn vector_vlenb() -> Option<usize> {
        unimplemented!()
    }
    pub fn read_fp(tid: u64) -> [u64; 33] {
        unimplemented!()
    }
    pub fn write_fp(tid: u64, job: Option<kcore::job::JobId>, w: &[u64; 33]) -> bool {
        unimplemented!()
    }
    pub fn read_vec(tid: u64) -> Option<alloc::vec::Vec<u8>> {
        unimplemented!()
    }
    pub fn write_vec(tid: u64, job: Option<kcore::job::JobId>, bytes: &[u8]) -> bool {
        unimplemented!()
    }
    pub fn apply_written(tid: u64, frame: &mut TrapFrame, fp: bool, vec: bool) {
        unimplemented!()
    }
    pub fn mark_present(tid: u64, frame: &mut TrapFrame) {
        unimplemented!()
    }
    pub fn entry_count() -> usize {
        unimplemented!()
    }
}

pub mod irq {
    pub static WINDOWS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    pub fn save_and_disable() -> bool {
        unimplemented!()
    }
    pub fn restore(was_enabled: bool) {
        unimplemented!()
    }
    pub fn enable() {
        unimplemented!()
    }
    pub fn enabled() -> bool {
        unimplemented!()
    }
    pub fn enable_timer_interrupt() {
        unimplemented!()
    }
    pub fn without<R>(f: impl FnOnce() -> R) -> R {
        unimplemented!()
    }
    pub fn off<R>(f: impl FnOnce() -> R) -> R {
        unimplemented!()
    }
    pub fn window() {
        unimplemented!()
    }
}

pub mod timer {
    pub fn set_frequency(hz: u64) -> bool {
        unimplemented!()
    }
    pub fn cycles_from_ns(ns: u64) -> u64 {
        unimplemented!()
    }
    pub fn timebase() -> u64 {
        unimplemented!()
    }
    pub fn now_ns() -> u64 {
        unimplemented!()
    }
    pub fn clock_params() -> (u64, u64, u64) {
        unimplemented!()
    }
    pub fn now() -> u64 {
        unimplemented!()
    }
    pub fn start() {
        unimplemented!()
    }
    pub fn program(event: Option<u64>) {
        unimplemented!()
    }
}

pub mod trap {
    pub static WINDOW_TRAPS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    use crate::arch::UserTrap;
    use kcore::aspace::{Access, FaultError};

    pub const FRAME_BYTES: u64 = 288;
    pub struct TrapFrame;

    impl TrapFrame {
        pub const ZERO: TrapFrame = TrapFrame;
        pub fn is_from_user(&self) -> bool {
            unimplemented!()
        }
        pub fn is_syscall(&self) -> bool {
            unimplemented!()
        }
        pub fn decode_user(&self) -> UserTrap {
            unimplemented!()
        }
        pub fn skip_trap_instruction(&mut self) {
            unimplemented!()
        }
        pub fn pc(&self) -> u64 {
            unimplemented!()
        }
        pub fn arg(&self, n: usize) -> u64 {
            unimplemented!()
        }
        pub fn from_general(words: &[u64; 32]) -> TrapFrame {
            unimplemented!()
        }
        pub fn general(&self) -> [u64; 32] {
            unimplemented!()
        }
        pub fn set_general(&mut self, words: &[u64; 32]) {
            unimplemented!()
        }
        pub fn set_return(&mut self, a0: u64, a1: Option<u64>, a2: Option<u64>) {
            unimplemented!()
        }
    }

    pub fn init() {
        unimplemented!()
    }

    #[derive(Debug, PartialEq, Eq)]
    pub struct Refused {
        pub access: Access,
        pub addr: u64,
        pub error: FaultError,
    }

    pub fn expect_refused(access: impl FnOnce()) -> Option<Refused> {
        unimplemented!()
    }

    pub fn flush_tlb() {
        unimplemented!()
    }
}

pub mod sbi {
    //! Firmware calls (RISC-V only: `smp.rs` still reaches for them, T179 moves that behind `ipi_send`, `cpu_start`, ...).
    pub const EID_HSM: usize = 0;
    pub const FID_HART_START: usize = 0;
    pub const FID_HART_GET_STATUS: usize = 0;
    pub const FID_HART_SUSPEND: usize = 0;
    pub const SBI_ERR_NOT_SUPPORTED: isize = 0;
    pub const EID_RFENCE: usize = 0;
    pub const FID_REMOTE_FENCE_I: usize = 0;
    pub const FID_REMOTE_SFENCE_VMA: usize = 0;
    pub const EID_IPI: usize = 0;
    pub fn system_suspend_present() -> bool {
        unimplemented!()
    }
    pub fn ipi_calls() -> usize {
        unimplemented!()
    }
    pub const HSM_STOPPED: usize = 0;
    pub struct SbiRet {
        pub error: isize,
        pub value: usize,
    }
    pub fn sbi(eid: usize, fid: usize, a0: usize, a1: usize, a2: usize) -> SbiRet {
        unimplemented!()
    }
    pub fn sbi5(eid: usize, fid: usize, a: [usize; 5]) -> SbiRet {
        unimplemented!()
    }
    pub fn secondary_entry() {
        unimplemented!()
    }
}

pub use cpu::*;

/// The firmware's debug console and reset.
pub fn debug_console_write_byte(byte: u8) -> bool {
    unimplemented!()
}
pub fn system_reset(reboot: bool, failure: bool) {
    unimplemented!()
}

/// Virtual minus physical for the kernel image.
/// (HD3 is RISC-V only for now: no slide, the direct map where `abi::layout` puts it.)
pub fn slide() -> u64 {
    0
}

pub fn direct_shift() -> u64 {
    0
}

pub fn kernel_delta() -> u64 {
    unimplemented!()
}
pub use switch::{switch, Context};
pub use usermode::enter_user_frame_raw as enter_user_frame;

pub mod cpu {
    pub fn aspace_token(root: u64) -> u64 {
        unimplemented!()
    }
    pub fn aspace_token_asid(root: u64, asid: u16) -> u64 {
        unimplemented!()
    }
    pub fn aspace_current() -> u64 {
        unimplemented!()
    }
    /// # Safety
    /// `token` names a live address space's root with an ASID of the current generation; the caller has interrupts off. (The stub does nothing: AR1c.)
    pub unsafe fn aspace_install(token: u64) {
        unimplemented!()
    }
    pub fn tlb_flush_all() {
        unimplemented!()
    }
    pub fn asid_bits() -> u32 {
        unimplemented!()
    }
    /// # Safety
    /// As `aspace_install`, and no flush is done: the ASID's translations are tagged, so the caller has refreshed the tag after any generation turn. (A stub: AR1c.)
    pub unsafe fn aspace_install_noflush(token: u64) {
        unimplemented!()
    }
    pub fn aspace_current_asid() -> u16 {
        unimplemented!()
    }
    pub fn icache_flush_local() {
        unimplemented!()
    }
    pub fn tlb_flush_page(va: u64, asid: u16) {
        unimplemented!()
    }
    pub fn enable_cache_block_ops() -> bool {
        unimplemented!()
    }
    pub fn vlenb_read() -> usize {
        unimplemented!()
    }
    pub fn seed_csr_read() -> usize {
        unimplemented!()
    }
    pub fn cpu_idle() {
        unimplemented!()
    }
    pub fn stack_pointer() -> usize {
        unimplemented!()
    }
    pub fn thread_pointer() -> usize {
        unimplemented!()
    }
    /// # Safety
    /// `p` is the address of this hart's per-CPU record, alive for as long as the hart runs. (A stub: AR1c.)
    pub unsafe fn set_thread_pointer(p: usize) {
        unimplemented!()
    }
    /// # Safety
    /// Interrupts are off on this hart, and no live code of this hart uses the floating-point or vector unit. (A stub: AR1c.)
    pub unsafe fn lazy_units_off() {
        unimplemented!()
    }
    pub fn lazy_units_are_off() -> bool {
        unimplemented!()
    }
}

pub mod switch {
    #[derive(Default)]
    pub struct Context;
    impl Context {
        pub fn new(entry: fn(usize), arg: usize, stack_top: usize) -> alloc::boxed::Box<Context> {
            unimplemented!()
        }
    }
    /// # Safety
    /// `old` is the running thread's context slot and `new` a context that was saved by `switch` or made by `Context::new`, neither borrowed elsewhere; interrupts are off. (A stub: AR1c.)
    pub unsafe fn switch(old: *mut Context, new: *const Context) {
        unimplemented!()
    }
}

pub mod usermode {
    /// # Safety
    /// `frame` is this hart's static entry frame, `token` an address-space token of the thread's process, `kernel_top` the thread's own kernel stack top; interrupts are off and no lock is held. (A stub: AR1c.)
    pub unsafe fn enter_user_frame_raw(
        frame: *const super::trap::TrapFrame,
        token: u64,
        kernel_top: u64,
    ) -> ! {
        unimplemented!()
    }
}
