//! Tests of the emulator's floating-point, memory, host-call and error paths,
//! against answers worked out independently (IEEE-754 and the RISC-V spec).

use rv64::emu::{EmuError, Emulator};
use rv64::*;

const BASE: u64 = 0x1000;

fn load(build: impl FnOnce(&mut Asm)) -> Emulator {
    let mut a = Asm::new();
    build(&mut a);
    let mut emu = Emulator::new(BASE, 0x4000);
    emu.write_bytes(BASE, &a.finish().unwrap()).unwrap();
    emu
}

fn run(build: impl FnOnce(&mut Asm)) -> u64 {
    load(build)
        .call(BASE, &[], 10_000, &mut |t, _| {
            panic!("unexpected call {t:#x}")
        })
        .unwrap()
}

fn load_f(a: &mut Asm, reg: FReg, value: f64) {
    a.li(T0, value.to_bits() as i64);
    a.fmv_d_x(reg, T0);
}

#[test]
fn double_arithmetic() {
    // The same program the kernel runs on the real machine in its self-test.
    let bits = run(|a| {
        load_f(a, FT0, 1.5);
        load_f(a, FT1, 2.0);
        load_f(a, FT2, 0.25);
        a.fmul_d(FA0, FT0, FT1);
        a.fadd_d(FA0, FA0, FT2);
        a.fmv_x_d(A0, FA0);
        a.ret();
    });
    assert_eq!(f64::from_bits(bits), 3.25);

    let bits = run(|a| {
        load_f(a, FT0, 10.0);
        load_f(a, FT1, 4.0);
        a.fsub_d(FT2, FT0, FT1);
        a.fdiv_d(FA0, FT2, FT1); // (10 - 4) / 4
        a.fmv_x_d(A0, FA0);
        a.ret();
    });
    assert_eq!(f64::from_bits(bits), 1.5);
}

#[test]
fn negation_conversion_and_comparison() {
    let bits = run(|a| {
        load_f(a, FT0, 2.5);
        a.fneg_d(FA0, FT0);
        a.fmv_x_d(A0, FA0);
        a.ret();
    });
    assert_eq!(f64::from_bits(bits), -2.5);

    // fcvt.l.d rounds toward zero; fcvt.d.l converts back.
    for (value, want) in [(7.9, 7i64), (-7.9, -7), (3.0, 3), (0.99, 0)] {
        let got = run(|a| {
            load_f(a, FT0, value);
            a.fcvt_l_d_rtz(A0, FT0);
            a.ret();
        });
        assert_eq!(got as i64, want, "fcvt.l.d {value}");
    }
    let bits = run(|a| {
        a.li(T0, -12);
        a.fcvt_d_l(FA0, T0);
        a.fmv_x_d(A0, FA0);
        a.ret();
    });
    assert_eq!(f64::from_bits(bits), -12.0);

    // feq, flt, fle for (1, 2), (2, 2), (3, 2): packed into three bits each.
    for (x, y, eq, lt, le) in [
        (1.0, 2.0, 0, 1, 1),
        (2.0, 2.0, 1, 0, 1),
        (3.0, 2.0, 0, 0, 0),
    ] {
        let got = run(|a| {
            load_f(a, FT0, x);
            load_f(a, FT1, y);
            a.feq_d(T0, FT0, FT1);
            a.flt_d(T1, FT0, FT1);
            a.fle_d(T2, FT0, FT1);
            a.slli(T1, T1, 1);
            a.slli(T2, T2, 2);
            a.or(A0, T0, T1);
            a.or(A0, A0, T2);
            a.ret();
        });
        assert_eq!(got, eq | lt << 1 | le << 2, "compare {x} {y}");
    }
}

#[test]
fn fclass_names_every_class() {
    let nan = f64::from_bits(0x7ff8_0000_0000_0000);
    let signalling = f64::from_bits(0x7ff0_0000_0000_0001);
    let subnormal = f64::from_bits(1);
    for (value, bit) in [
        (f64::NEG_INFINITY, 0),
        (-1.5, 1),
        (-subnormal, 2),
        (-0.0, 3),
        (0.0, 4),
        (subnormal, 5),
        (1.5, 6),
        (f64::INFINITY, 7),
        (signalling, 8),
        (nan, 9),
    ] {
        let got = run(|a| {
            a.li(T0, value.to_bits() as i64);
            a.fmv_d_x(FT0, T0);
            a.fclass_d(A0, FT0);
            a.ret();
        });
        assert_eq!(got, 1 << bit, "fclass of {value:e}");
    }
}

#[test]
fn memory_round_trips() {
    let got = run(|a| {
        a.addi(SP, SP, -32);
        a.li(T0, 0x1122_3344_5566_7788);
        a.sd(T0, SP, 8);
        a.ld(A0, SP, 8);
        load_f(a, FT0, -6.25);
        a.fsd(FT0, SP, 16);
        a.fld(FT1, SP, 16);
        a.fmv_x_d(T1, FT1);
        a.xor(A0, A0, T1); // both halves of the result survive the xor
        a.addi(SP, SP, 32);
        a.ret();
    });
    assert_eq!(got, 0x1122_3344_5566_7788 ^ (-6.25f64).to_bits());

    // lw sign-extends, lbu zero-extends.
    let got = run(|a| {
        a.addi(SP, SP, -16);
        a.li(T0, 0xFFFF_FFFF_8000_00FFu64 as i64);
        a.sd(T0, SP, 0);
        a.lw(T1, SP, 0);
        a.lbu(T2, SP, 0);
        a.sub(A0, T1, T2);
        a.addi(SP, SP, 16);
        a.ret();
    });
    assert_eq!(got as i64, (0xFFFF_FFFF_8000_00FFu64 as i32 as i64) - 0xFF);
}

#[test]
fn calls_to_host_services_return_to_the_next_instruction() {
    let mut emu = load(|a| {
        a.addi(SP, SP, -16);
        a.sd(RA, SP, 8);
        a.li(T0, 0xF000_0010);
        a.li(A0, 5);
        a.li(A1, 7);
        a.call_reg(T0);
        a.addi(A0, A0, 1); // runs after the host call returns
        a.ld(RA, SP, 8);
        a.addi(SP, SP, 16);
        a.ret();
    });
    let mut seen = Vec::new();
    let result = emu
        .call(BASE, &[], 1000, &mut |target, e| {
            seen.push((target, e.x[10], e.x[11]));
            e.x[10] *= e.x[11]; // the "service": multiply
        })
        .unwrap();
    assert_eq!(seen, vec![(0xF000_0010, 5, 7)]);
    assert_eq!(result, 36);
}

#[test]
fn errors_are_reported() {
    let mut emu = load(|a| {
        a.ld(A0, ZERO, 0); // nothing at address 0
        a.ret();
    });
    assert_eq!(
        emu.call(BASE, &[], 100, &mut |_, _| {}),
        Err(EmuError::BadAccess(0))
    );

    let mut emu = load(|a| a.ecall());
    assert!(matches!(
        emu.call(BASE, &[], 100, &mut |_, _| {}),
        Err(EmuError::Unsupported(_))
    ));

    let mut emu = load(|a| {
        let top = a.new_label();
        a.bind(top);
        a.j(top);
    });
    assert_eq!(
        emu.call(BASE, &[], 100, &mut |_, _| {}),
        Err(EmuError::Timeout)
    );

    let mut emu = Emulator::new(BASE, 0x100);
    assert_eq!(
        emu.call(0x10, &[], 100, &mut |_, _| {}),
        Err(EmuError::BadFetch(0x10))
    );
}
