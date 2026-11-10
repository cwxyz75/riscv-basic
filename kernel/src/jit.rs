//! Running generated machine code: copy it into RAM, make the instruction
//! stream see it (`fence.i`), and call it. Everything that executes generated
//! code lives here, so the rest of the kernel and the `logo`/`rv64` crates stay
//! free of `unsafe`.
//!
//! The kernel runs with paging off and all of RAM executable, so generated code
//! in the heap can run directly. The most recently loaded code's address range
//! is remembered so the trap handler can dump it if something goes wrong.

use alloc::vec::Vec;
use core::arch::asm;
use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};
use rv64::{Asm, Cond, A0, FA0, FT0, FT1, FT2, T0, T1};

static REGION_START: AtomicUsize = AtomicUsize::new(0);
static REGION_LEN: AtomicUsize = AtomicUsize::new(0);

/// Machine code loaded into memory. Words (not bytes) so it is 4-aligned.
pub struct Code {
    words: Vec<u32>,
}

impl Code {
    /// Copy `bytes` (little-endian instruction words) into fresh memory and
    /// synchronise the instruction fetch with it.
    pub fn load(bytes: &[u8]) -> Code {
        let (chunks, _) = bytes.as_chunks::<4>();
        let words: Vec<u32> = chunks.iter().map(|c| u32::from_le_bytes(*c)).collect();
        REGION_START.store(words.as_ptr() as usize, Ordering::Relaxed);
        REGION_LEN.store(words.len() * 4, Ordering::Relaxed);
        // SAFETY: `fence.i` only orders instruction fetches after our stores to
        // the buffer above; it has no other effect.
        unsafe { asm!("fence.i") };
        Code { words }
    }

    /// Call the code as an `extern "C" fn() -> u64`.
    ///
    /// The caller must have loaded code that is a valid function with that
    /// signature (it returns with `ret` and clobbers only caller-saved
    /// registers, or saves and restores the others).
    pub fn call(&self) -> u64 {
        // SAFETY: `words` holds instruction words for a function following the
        // C calling convention (guaranteed by the caller, see above); it is
        // 4-byte aligned and stays alive for the duration of the call.
        let function: extern "C" fn() -> u64 = unsafe { core::mem::transmute(self.words.as_ptr()) };
        function()
    }
}

/// The address range `(start, length_in_bytes)` of the most recently loaded
/// code, if any.
pub fn region() -> Option<(usize, usize)> {
    let len = REGION_LEN.load(Ordering::Relaxed);
    (len != 0).then(|| (REGION_START.load(Ordering::Relaxed), len))
}

fn run(build: impl FnOnce(&mut Asm)) -> u64 {
    let mut asm = Asm::new();
    build(&mut asm);
    let bytes = asm.finish().expect("self-test code assembles");
    Code::load(&bytes).call()
}

/// Run the JIT self-tests on the real machine and report each result. They
/// cover the pieces the LOGO compiler relies on: loading a constant, double
/// arithmetic, loops with labels and branches, and 64-bit constants (the
/// NaN-boxing tags).
pub fn self_test(out: &mut impl Write) {
    let mut passed = 0;
    let mut report = |out: &mut dyn Write, name: &str, got: u64, want: u64| {
        let ok = got == want;
        passed += usize::from(ok);
        let _ = writeln!(
            out,
            "jit: {name} -> {got:#x} {}",
            if ok { "ok" } else { "FAILED" }
        );
    };

    // The smoke test: emit `li a0, 42; ret`, fence.i, call it.
    let got = run(|a| {
        a.li(A0, 42);
        a.ret();
    });
    report(out, "li a0,42; ret", got, 42);

    // Double arithmetic: 1.5 * 2.0 + 0.25, through the FPU.
    let got = run(|a| {
        for (reg, value) in [(FT0, 1.5f64), (FT1, 2.0), (FT2, 0.25)] {
            a.li(T0, value.to_bits() as i64);
            a.fmv_d_x(reg, T0);
        }
        a.fmul_d(FA0, FT0, FT1);
        a.fadd_d(FA0, FA0, FT2);
        a.fmv_x_d(A0, FA0);
        a.ret();
    });
    report(out, "1.5 * 2 + 0.25 (FPU)", got, 3.25f64.to_bits());

    // A loop with a backward branch: 10 + 9 + ... + 1.
    let got = run(|a| {
        a.li(T0, 10);
        a.li(A0, 0);
        let top = a.new_label();
        a.bind(top);
        a.add(A0, A0, T0);
        a.addi(T0, T0, -1);
        a.bnez(T0, top);
        a.ret();
    });
    report(out, "sum 1..10 (loop)", got, 55);

    // A forward conditional branch and signed/unsigned compares.
    let got = run(|a| {
        a.li(T0, -5);
        a.li(T1, 3);
        a.li(A0, 0);
        let skip = a.new_label();
        a.branch(Cond::Lt, T0, T1, skip); // taken (signed -5 < 3)
        a.li(A0, 99);
        a.bind(skip);
        let skip2 = a.new_label();
        a.branch(Cond::Ltu, T0, T1, skip2); // not taken (-5 is huge unsigned)
        a.addi(A0, A0, 7);
        a.bind(skip2);
        a.ret();
    });
    report(out, "signed/unsigned branches", got, 7);

    // A NaN-boxing style tag, and the top bits of a double.
    let tag = 0xFFF9_0000_0000_0007u64;
    let got = run(|a| {
        a.li(A0, tag as i64);
        a.srli(T0, A0, 48);
        a.addi(T0, T0, 1);
        a.slli(T0, T0, 48);
        a.li(T1, 7);
        a.add(A0, T0, T1);
        a.ret();
    });
    report(out, "64-bit tag arithmetic", got, 0xFFFA_0000_0000_0007);

    let _ = writeln!(out, "jit self-test: {passed}/5 ok");
}

/// Deliberately fault inside generated code, to show what the trap handler
/// prints about it. Never returns (a fatal trap powers the machine off).
pub fn crash_demo() {
    run(|a| {
        a.li(T0, 0x1234_5678); // an address with nothing mapped behind it
        a.ld(A0, T0, 0); // load access fault
        a.ret();
    });
}

/// Print the generated code the fault at `pc` happened in, if it did: every
/// word, with the faulting one marked.
pub fn dump_for_fault(out: &mut impl Write, pc: usize) {
    let Some((start, len)) = region() else { return };
    if pc < start || pc >= start + len {
        return;
    }
    let _ = writeln!(
        out,
        "JIT code at {start:#x} ({len} bytes), fault at +{:#x}:",
        pc - start
    );
    for offset in (0..len).step_by(4) {
        // SAFETY: `offset` is inside the region that `Code::load` recorded, and
        // generated code stays loaded (the `Code` is dropped only after the
        // call returns, and a fatal trap never returns).
        let word = unsafe { core::ptr::read((start + offset) as *const u32) };
        let marker = if start + offset == pc {
            " <-- fault"
        } else {
            ""
        };
        let _ = writeln!(out, "  +{offset:#04x}: {word:08x}{marker}");
    }
}
