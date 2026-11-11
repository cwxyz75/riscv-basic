//! Encoder tests against LLVM: every expected word below was produced by
//! assembling the same instruction with LLVM (`rustc` + `global_asm!` with
//! `.option norvc`, disassembled with `llvm-objdump`), not by this crate.

use rv64::*;

fn word(f: impl FnOnce(&mut Asm)) -> u32 {
    let mut a = Asm::new();
    f(&mut a);
    let bytes = a.finish().unwrap();
    assert_eq!(bytes.len(), 4, "one instruction expected");
    u32::from_le_bytes(bytes.try_into().unwrap())
}

#[test]
fn encodings_match_llvm() {
    let cases: Vec<(&str, u32, u32)> = vec![
        ("a.add(A0, A1, A2)", word(|a| a.add(A0, A1, A2)), 0x00c58533),
        ("a.sub(T0, T1, T2)", word(|a| a.sub(T0, T1, T2)), 0x407302b3),
        (
            "a.addi(A0, A0, 42)",
            word(|a| a.addi(A0, A0, 42)),
            0x02a50513,
        ),
        (
            "a.addi(SP, SP, -2048)",
            word(|a| a.addi(SP, SP, -2048)),
            0x80010113,
        ),
        (
            "a.addi(SP, SP, 2047)",
            word(|a| a.addi(SP, SP, 2047)),
            0x7ff10113,
        ),
        (
            "a.addiw(A0, A0, 7)",
            word(|a| a.addiw(A0, A0, 7)),
            0x0075051b,
        ),
        (
            "a.andi(T0, T1, 255)",
            word(|a| a.andi(T0, T1, 255)),
            0x0ff37293,
        ),
        ("a.ori(T0, T1, -1)", word(|a| a.ori(T0, T1, -1)), 0xfff36293),
        ("a.xori(A0, A1, 5)", word(|a| a.xori(A0, A1, 5)), 0x0055c513),
        ("a.slli(A0, A1, 3)", word(|a| a.slli(A0, A1, 3)), 0x00359513),
        (
            "a.slli(T0, T1, 63)",
            word(|a| a.slli(T0, T1, 63)),
            0x03f31293,
        ),
        (
            "a.srli(A0, A1, 48)",
            word(|a| a.srli(A0, A1, 48)),
            0x0305d513,
        ),
        ("a.srai(A0, A1, 1)", word(|a| a.srai(A0, A1, 1)), 0x4015d513),
        ("a.and(A0, A1, A2)", word(|a| a.and(A0, A1, A2)), 0x00c5f533),
        ("a.or(A0, A1, A2)", word(|a| a.or(A0, A1, A2)), 0x00c5e533),
        ("a.xor(A0, A1, A2)", word(|a| a.xor(A0, A1, A2)), 0x00c5c533),
        ("a.mul(A0, A1, A2)", word(|a| a.mul(A0, A1, A2)), 0x02c58533),
        (
            "a.lui(A0, 0x12345)",
            word(|a| a.lui(A0, 0x12345)),
            0x12345537,
        ),
        (
            "a.lui(T0, 0xfffff)",
            word(|a| a.lui(T0, 0xfffff)),
            0xfffff2b7,
        ),
        ("a.auipc(A0, 1)", word(|a| a.auipc(A0, 1)), 0x00001517),
        ("a.ld(A0, SP, 8)", word(|a| a.ld(A0, SP, 8)), 0x00813503),
        ("a.ld(RA, S0, -16)", word(|a| a.ld(RA, S0, -16)), 0xff043083),
        ("a.sd(A0, SP, 16)", word(|a| a.sd(A0, SP, 16)), 0x00a13823),
        ("a.sd(S0, SP, -8)", word(|a| a.sd(S0, SP, -8)), 0xfe813c23),
        ("a.lw(A0, A1, 4)", word(|a| a.lw(A0, A1, 4)), 0x0045a503),
        ("a.lbu(A0, A1, 1)", word(|a| a.lbu(A0, A1, 1)), 0x0015c503),
        ("a.fld(FA0, SP, 8)", word(|a| a.fld(FA0, SP, 8)), 0x00813507),
        (
            "a.fsd(FA1, SP, -8)",
            word(|a| a.fsd(FA1, SP, -8)),
            0xfeb13c27,
        ),
        (
            "a.emit(branch_word(Cond::Eq, A0, A1, 8))",
            word(|a| a.emit(branch_word(Cond::Eq, A0, A1, 8))),
            0x00b50463,
        ),
        (
            "a.emit(branch_word(Cond::Ne, A0, ZERO, 8))",
            word(|a| a.emit(branch_word(Cond::Ne, A0, ZERO, 8))),
            0x00051463,
        ),
        (
            "a.emit(branch_word(Cond::Lt, A0, A1, 8))",
            word(|a| a.emit(branch_word(Cond::Lt, A0, A1, 8))),
            0x00b54463,
        ),
        (
            "a.emit(branch_word(Cond::Ge, A0, A1, 8))",
            word(|a| a.emit(branch_word(Cond::Ge, A0, A1, 8))),
            0x00b55463,
        ),
        (
            "a.emit(branch_word(Cond::Ltu, A0, A1, 8))",
            word(|a| a.emit(branch_word(Cond::Ltu, A0, A1, 8))),
            0x00b56463,
        ),
        (
            "a.emit(branch_word(Cond::Geu, A0, A1, 8))",
            word(|a| a.emit(branch_word(Cond::Geu, A0, A1, 8))),
            0x00b57463,
        ),
        (
            "a.emit(jal_word(RA, 8))",
            word(|a| a.emit(jal_word(RA, 8))),
            0x008000ef,
        ),
        (
            "a.emit(jal_word(ZERO, -4))",
            word(|a| a.emit(jal_word(ZERO, -4))),
            0xffdff06f,
        ),
        ("a.jalr(RA, T0, 0)", word(|a| a.jalr(RA, T0, 0)), 0x000280e7),
        ("a.ret()", word(|a| a.ret()), 0x00008067),
        (
            "a.fadd_d(FA0, FA1, FA2)",
            word(|a| a.fadd_d(FA0, FA1, FA2)),
            0x02c5f553,
        ),
        (
            "a.fsub_d(FT0, FT1, FT2)",
            word(|a| a.fsub_d(FT0, FT1, FT2)),
            0x0a20f053,
        ),
        (
            "a.fmul_d(FT0, FT1, FT2)",
            word(|a| a.fmul_d(FT0, FT1, FT2)),
            0x1220f053,
        ),
        (
            "a.fdiv_d(FT0, FT1, FT2)",
            word(|a| a.fdiv_d(FT0, FT1, FT2)),
            0x1a20f053,
        ),
        (
            "a.fneg_d(FA0, FA1)",
            word(|a| a.fneg_d(FA0, FA1)),
            0x22b59553,
        ),
        (
            "a.fmv_d_x(FT0, T0)",
            word(|a| a.fmv_d_x(FT0, T0)),
            0xf2028053,
        ),
        (
            "a.fmv_x_d(T0, FT0)",
            word(|a| a.fmv_x_d(T0, FT0)),
            0xe20002d3,
        ),
        (
            "a.feq_d(T0, FT0, FT1)",
            word(|a| a.feq_d(T0, FT0, FT1)),
            0xa21022d3,
        ),
        (
            "a.flt_d(T0, FT0, FT1)",
            word(|a| a.flt_d(T0, FT0, FT1)),
            0xa21012d3,
        ),
        (
            "a.fle_d(T0, FT0, FT1)",
            word(|a| a.fle_d(T0, FT0, FT1)),
            0xa21002d3,
        ),
        (
            "a.fclass_d(T4, FT2)",
            word(|a| a.fclass_d(T4, FT2)),
            0xe2011ed3,
        ),
        (
            "a.fcvt_l_d_rtz(A0, FA0)",
            word(|a| a.fcvt_l_d_rtz(A0, FA0)),
            0xc2251553,
        ),
        (
            "a.fcvt_d_l(FA0, A0)",
            word(|a| a.fcvt_d_l(FA0, A0)),
            0xd2257553,
        ),
        ("a.fence_i()", word(|a| a.fence_i()), 0x0000100f),
        ("a.ebreak()", word(|a| a.ebreak()), 0x00100073),
        ("a.ecall()", word(|a| a.ecall()), 0x00000073),
    ];
    for (what, got, want) in cases {
        assert_eq!(got, want, "{what}: got {got:#010x}, LLVM says {want:#010x}");
    }
}
