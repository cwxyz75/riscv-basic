//! Behavioural tests: run assembled code in a tiny RV64 emulator (just the
//! integer instructions the tests use) and check the results.

use rv64::emu::Emulator;
use rv64::*;

/// Run `code` (loaded at address 0x1000) until it returns, and return `a0`.
fn run_a0(code: &[u8], max_steps: usize) -> u64 {
    let mut emu = Emulator::new(0x1000, 0x2000);
    emu.write_bytes(0x1000, code).unwrap();
    emu.call(0x1000, &[], max_steps, &mut |target, _| {
        panic!("unexpected call to {target:#x}")
    })
    .unwrap()
}

fn load_constant(value: i64) -> u64 {
    let mut a = Asm::new();
    a.li(A0, value);
    a.ret();
    run_a0(&a.finish().unwrap(), 100)
}

#[test]
fn li_loads_every_kind_of_constant() {
    let mut cases: Vec<i64> = vec![
        0,
        1,
        -1,
        2047,
        2048,
        -2048,
        -2049,
        4095,
        4096,
        0x7ff,
        0x800,
        0xfff,
        0x1000,
        0x7fff_f7ff,
        0x7fff_f800,
        0x7fff_ffff,
        0x8000_0000,
        0xffff_ffff,
        0x1_0000_0000,
        -0x8000_0000,
        -0x8000_0001,
        0x7fff_ffff_ffff_ffff,
        i64::MIN,
        i64::MIN + 1,
        0x0123_4567_89ab_cdef,
        -0x0123_4567_89ab_cdef,
        // The NaN-boxing tags the code generator loads:
        0xFFF9_0000_0000_0000u64 as i64,
        0xFFFA_0000_0000_0000u64 as i64,
        0xFFFB_0000_0000_0000u64 as i64,
        0xFFF9_0000_0000_0001u64 as i64,
        0x7FF8_0000_0000_0000,
    ];
    for f in [
        0.0f64,
        1.0,
        -1.0,
        core::f64::consts::PI,
        1e300,
        -2.5e-10,
        123456789.0,
    ] {
        cases.push(f.to_bits() as i64);
    }
    // A deterministic spread of other values (a simple LCG).
    let mut s: u64 = 0x2545_f491_4f6c_dd1d;
    for _ in 0..20_000 {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        // Vary the magnitude so every code path gets used.
        cases.push((s >> (s >> 58)) as i64);
    }
    for value in cases {
        assert_eq!(load_constant(value), value as u64, "li {value:#x}");
    }
}

#[test]
fn a_backward_branch_makes_a_loop() {
    // sum 10 + 9 + ... + 1
    let mut a = Asm::new();
    a.li(T0, 10);
    a.li(A0, 0);
    let top = a.new_label();
    a.bind(top);
    a.add(A0, A0, T0);
    a.addi(T0, T0, -1);
    a.bnez(T0, top);
    a.ret();
    assert_eq!(run_a0(&a.finish().unwrap(), 1000), 55);
}

#[test]
fn a_forward_branch_skips_code() {
    let mut a = Asm::new();
    let skip = a.new_label();
    a.li(A0, 1);
    a.beqz(ZERO, skip); // always taken
    a.li(A0, 2);
    a.bind(skip);
    a.ret();
    assert_eq!(run_a0(&a.finish().unwrap(), 100), 1);

    // The same, not taken.
    let mut a = Asm::new();
    let skip = a.new_label();
    a.li(A0, 1);
    a.li(T0, 5);
    a.beqz(T0, skip); // not taken
    a.li(A0, 2);
    a.bind(skip);
    a.ret();
    assert_eq!(run_a0(&a.finish().unwrap(), 100), 2);
}

#[test]
fn conditional_branches_cover_all_conditions() {
    // For each condition, compare 3 against 5 and against -5 and see whether the branch is taken.
    let cases = [
        (Cond::Eq, 3i64, 3i64, true),
        (Cond::Eq, 3, 5, false),
        (Cond::Ne, 3, 5, true),
        (Cond::Lt, -5, 3, true),
        (Cond::Lt, 3, -5, false),
        (Cond::Ge, 3, -5, true),
        (Cond::Ge, -5, 3, false),
        (Cond::Ltu, 3, -5, true), // -5 is a huge unsigned number
        (Cond::Geu, -5, 3, true),
    ];
    for (cond, x, y, taken) in cases {
        let mut a = Asm::new();
        let target = a.new_label();
        a.li(T0, x);
        a.li(T1, y);
        a.li(A0, 0);
        a.branch(cond, T0, T1, target);
        a.li(A0, 1); // fall-through marker
        a.bind(target);
        a.ret();
        let result = run_a0(&a.finish().unwrap(), 100);
        assert_eq!(result == 0, taken, "{cond:?} {x} {y}");
    }
}

#[test]
fn branches_reach_farther_than_a_short_branch() {
    // 1500 words between the branch and its target is beyond a plain B-type (+-4 KiB).
    let mut a = Asm::new();
    let far = a.new_label();
    a.li(A0, 7);
    a.beqz(ZERO, far);
    for _ in 0..1500 {
        a.addi(ZERO, ZERO, 0); // nop
    }
    a.li(A0, 9); // skipped
    a.bind(far);
    a.ret();
    assert_eq!(run_a0(&a.finish().unwrap(), 10_000), 7);
}

#[test]
fn jal_saves_the_return_address_and_labels_must_be_bound() {
    let mut a = Asm::new();
    let sub = a.new_label();
    a.li(A0, 0);
    a.mv(T6, RA); // keep our own return address (0)
    a.jal(RA, sub);
    a.addi(A0, A0, 100); // runs after the "call" returns
    a.mv(RA, T6);
    a.ret();
    a.bind(sub);
    a.addi(A0, A0, 1);
    a.ret(); // back to the instruction after the jal
    assert_eq!(run_a0(&a.finish().unwrap(), 100), 101);

    let mut a = Asm::new();
    let never = a.new_label();
    a.j(never);
    assert_eq!(a.finish().unwrap_err(), AsmError::UnboundLabel);

    let mut a = Asm::new();
    let far = a.new_label();
    a.j(far);
    for _ in 0..(1 << 18) + 8 {
        a.addi(ZERO, ZERO, 0);
    }
    a.bind(far);
    assert_eq!(a.finish().unwrap_err(), AsmError::OutOfRange);
}
