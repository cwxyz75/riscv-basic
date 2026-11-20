//! The AArch64 stub (AR1d, KERNEL-V1-SPEC 2.6): every item of the architecture interface is present and every body is
//! `unimplemented!()`. It carries no behaviour and is never booted. Its only job is to make
//! `cargo check --target aarch64-unknown-none` fail when code outside `arch/` reaches for something the RISC-V directory has and
//! this one lacks, which proves the boundary is complete. A real AArch64 port replaces each body and needs its own profile-floor
//! decision beside OD127 first. Rule for every later item: an interface item added to `arch/riscv64` is added here in the same PR.
#![allow(dead_code, unused_variables, clippy::missing_safety_doc)]

pub mod aia {
    pub const APLIC_BASE: u64 = 0;
    pub const PAGE_COUNT: usize = 0;
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
    pub const BASE: u64 = 0;
    pub fn exit(ok: bool) -> ! {
        unimplemented!()
    }
}

pub mod fp {
    use super::trap::TrapFrame;
    pub fn trap_entry(frame: &mut TrapFrame) {
        unimplemented!()
    }
    pub fn return_to_user(frame: &mut TrapFrame) {
        unimplemented!()
    }
    pub fn first_use(frame: &mut TrapFrame) -> bool {
        unimplemented!()
    }
    pub fn thread_gone(tid: u32) {
        unimplemented!()
    }
    pub fn entry_count() -> usize {
        unimplemented!()
    }
}

pub mod irq {
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
}

pub mod timer {
    pub fn timebase() -> u64 {
        unimplemented!()
    }
    pub fn now_ns() -> u64 {
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
    use crate::arch::UserTrap;
    use kcore::aspace::{Access, FaultError};

    pub struct TrapFrame;

    impl TrapFrame {
        pub fn is_syscall(&self) -> bool {
            unimplemented!()
        }
        pub fn decode_user(&self) -> UserTrap {
            unimplemented!()
        }
        pub fn skip_trap_instruction(&mut self) {
            unimplemented!()
        }
        pub fn arg(&self, n: usize) -> u64 {
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
    pub const FID_REMOTE_SFENCE_VMA: usize = 0;
    pub const EID_IPI: usize = 0;
    pub const FID_SEND_IPI: usize = 0;
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
pub use switch::{switch, Context};
pub use usermode::enter_user_raw as enter_user;

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
    pub unsafe fn aspace_install(token: u64) {
        unimplemented!()
    }
    pub fn tlb_flush_all() {
        unimplemented!()
    }
    pub fn tlb_flush_page(va: u64, asid: u16) {
        unimplemented!()
    }
    pub fn cpu_idle() {
        unimplemented!()
    }
    pub fn thread_pointer() -> usize {
        unimplemented!()
    }
    pub unsafe fn set_thread_pointer(p: usize) {
        unimplemented!()
    }
    pub unsafe fn lazy_units_off() {
        unimplemented!()
    }
    pub fn lazy_units_are_off() -> bool {
        unimplemented!()
    }
    pub fn software_interrupt_enable() {
        unimplemented!()
    }
    pub fn software_interrupt_clear() {
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
    pub unsafe fn switch(old: *mut Context, new: *const Context) {
        unimplemented!()
    }
}

pub mod usermode {
    pub unsafe fn enter_user_raw(
        entry: u64,
        sp: u64,
        arg: u64,
        token: u64,
        kernel_top: u64,
        tp: u64,
        a1: u64,
    ) -> ! {
        unimplemented!()
    }
}
