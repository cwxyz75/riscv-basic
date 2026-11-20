//! The context switch: the callee-saved state of a thread and the assembly that exchanges two of them (AR1).

use core::arch::global_asm;

/// Callee-saved state, saved and restored by `context_switch`.
#[repr(C)]
#[derive(Default)]
pub struct Context {
    ra: u64,
    sp: u64,
    s: [u64; 12],
}

global_asm!(
    r#"
    .section .text
    .globl context_switch
    .balign 4
context_switch:                  # a0 = old *mut Context, a1 = new *const Context
    sd   ra,   0(a0)
    sd   sp,   8(a0)
    sd   s0,  16(a0)
    sd   s1,  24(a0)
    sd   s2,  32(a0)
    sd   s3,  40(a0)
    sd   s4,  48(a0)
    sd   s5,  56(a0)
    sd   s6,  64(a0)
    sd   s7,  72(a0)
    sd   s8,  80(a0)
    sd   s9,  88(a0)
    sd   s10, 96(a0)
    sd   s11, 104(a0)
    ld   ra,   0(a1)
    ld   sp,   8(a1)
    ld   s0,  16(a1)
    ld   s1,  24(a1)
    ld   s2,  32(a1)
    ld   s3,  40(a1)
    ld   s4,  48(a1)
    ld   s5,  56(a1)
    ld   s6,  64(a1)
    ld   s7,  72(a1)
    ld   s8,  80(a1)
    ld   s9,  88(a1)
    ld   s10, 96(a1)
    ld   s11, 104(a1)
    ret

    .globl thread_trampoline
    .balign 4
thread_trampoline:               # a new thread's first instruction (via `ret` above)
    mv   a0, s0                  # entry
    mv   a1, s1                  # argument
    call thread_bootstrap
    ebreak
"#
);

extern "C" {
    fn context_switch(old: *mut Context, new: *const Context);
    fn thread_trampoline();
}

impl Context {
    /// The context of a thread that has not run: its first switch lands in `thread_bootstrap(entry, arg)`
    /// on the stack whose top is `stack_top`.
    pub fn new(entry: usize, arg: usize, stack_top: usize) -> alloc::boxed::Box<Context> {
        let mut ctx = alloc::boxed::Box::<Context>::default();
        ctx.ra = thread_trampoline as *const () as u64;
        ctx.sp = (stack_top & !15) as u64;
        if let Some(e) = ctx.s.first_mut() {
            *e = entry as u64;
        }
        if let Some(a) = ctx.s.get_mut(1) {
            *a = arg as u64;
        }
        ctx
    }
}

/// Save the current state in `old` and continue from `new`.
///
/// # Safety
/// `old` and `new` are valid contexts; `new` was saved by a switch or made by [`Context::new`], and its stack is live.
pub unsafe fn switch(old: *mut Context, new: *const Context) {
    // SAFETY: the caller guarantees both contexts.
    unsafe { context_switch(old, new) }
}
