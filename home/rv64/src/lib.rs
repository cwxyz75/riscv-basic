//! A small RV64 assembler: just the instructions the LOGO code generator needs
//! (RV64I, the M multiply, the D double-precision floating point, `fence.i`),
//! with labels and a `li` pseudo-instruction that loads any 64-bit constant.
//!
//! Instructions are always emitted in their plain 32-bit form (never the
//! compressed `C` encodings), so offsets and sizes are predictable. The
//! encoders are checked against LLVM's own assembler (`tests/vectors.rs`), and
//! label and `li` behaviour by running the output in a tiny emulator on the
//! host (`tests/emulate.rs`).
//!
//! `no_std`, no `unsafe`.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod emu;

use alloc::vec::Vec;

/// An integer register `x0`..`x31`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reg(pub u8);

/// A floating-point register `f0`..`f31`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FReg(pub u8);

pub const ZERO: Reg = Reg(0);
pub const RA: Reg = Reg(1);
pub const SP: Reg = Reg(2);
pub const T0: Reg = Reg(5);
pub const T1: Reg = Reg(6);
pub const T2: Reg = Reg(7);
pub const S0: Reg = Reg(8);
pub const S1: Reg = Reg(9);
pub const A0: Reg = Reg(10);
pub const A1: Reg = Reg(11);
pub const A2: Reg = Reg(12);
pub const A3: Reg = Reg(13);
pub const A4: Reg = Reg(14);
pub const A5: Reg = Reg(15);
pub const A6: Reg = Reg(16);
pub const A7: Reg = Reg(17);
pub const T3: Reg = Reg(28);
pub const T4: Reg = Reg(29);
pub const T5: Reg = Reg(30);
pub const T6: Reg = Reg(31);

pub const FT0: FReg = FReg(0);
pub const FT1: FReg = FReg(1);
pub const FT2: FReg = FReg(2);
pub const FA0: FReg = FReg(10);
pub const FA1: FReg = FReg(11);
pub const FA2: FReg = FReg(12);

/// A position in the code, to branch to. Create with [`Asm::new_label`], place
/// with [`Asm::bind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Label(usize);

/// The conditions of the conditional branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cond {
    Eq,
    Ne,
    Lt,
    Ge,
    Ltu,
    Geu,
}

impl Cond {
    fn funct3(self) -> u32 {
        match self {
            Cond::Eq => 0b000,
            Cond::Ne => 0b001,
            Cond::Lt => 0b100,
            Cond::Ge => 0b101,
            Cond::Ltu => 0b110,
            Cond::Geu => 0b111,
        }
    }

    fn inverse(self) -> Cond {
        match self {
            Cond::Eq => Cond::Ne,
            Cond::Ne => Cond::Eq,
            Cond::Lt => Cond::Ge,
            Cond::Ge => Cond::Lt,
            Cond::Ltu => Cond::Geu,
            Cond::Geu => Cond::Ltu,
        }
    }
}

/// What can go wrong when the code is finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsmError {
    /// A label was used but never bound.
    UnboundLabel,
    /// A jump is farther than a `jal` can reach (more than 1 MiB).
    OutOfRange,
}

/// A jump to patch once its label's position is known.
#[derive(Debug, Clone, Copy)]
struct Fixup {
    /// Index of the word to patch (a `jal`).
    at: usize,
    label: Label,
    /// The `rd` of the `jal` (`ZERO` for a plain jump).
    rd: Reg,
}

/// The assembler: a growing list of 32-bit instruction words.
#[derive(Debug, Default)]
pub struct Asm {
    words: Vec<u32>,
    labels: Vec<Option<usize>>,
    fixups: Vec<Fixup>,
}

// ---- raw encoders (also used directly by the tests) -------------------------

fn r_type(funct7: u32, rs2: u8, rs1: u8, funct3: u32, rd: u8, opcode: u32) -> u32 {
    funct7 << 25
        | u32::from(rs2) << 20
        | u32::from(rs1) << 15
        | funct3 << 12
        | u32::from(rd) << 7
        | opcode
}

fn i_type(imm: i32, rs1: u8, funct3: u32, rd: u8, opcode: u32) -> u32 {
    assert!(
        (-2048..2048).contains(&imm),
        "12-bit immediate out of range: {imm}"
    );
    ((imm as u32) & 0xfff) << 20 | u32::from(rs1) << 15 | funct3 << 12 | u32::from(rd) << 7 | opcode
}

fn s_type(imm: i32, rs2: u8, rs1: u8, funct3: u32, opcode: u32) -> u32 {
    assert!(
        (-2048..2048).contains(&imm),
        "12-bit immediate out of range: {imm}"
    );
    let imm = imm as u32 & 0xfff;
    (imm >> 5) << 25
        | u32::from(rs2) << 20
        | u32::from(rs1) << 15
        | funct3 << 12
        | (imm & 0x1f) << 7
        | opcode
}

fn b_type(offset: i32, rs2: u8, rs1: u8, funct3: u32) -> u32 {
    assert!(
        offset % 2 == 0 && (-4096..4096).contains(&offset),
        "branch offset out of range: {offset}"
    );
    let o = offset as u32;
    (o >> 12 & 1) << 31
        | (o >> 5 & 0x3f) << 25
        | u32::from(rs2) << 20
        | u32::from(rs1) << 15
        | funct3 << 12
        | (o >> 1 & 0xf) << 8
        | (o >> 11 & 1) << 7
        | 0b110_0011
}

fn u_type(imm20: u32, rd: u8, opcode: u32) -> u32 {
    (imm20 & 0xf_ffff) << 12 | u32::from(rd) << 7 | opcode
}

fn j_type(offset: i32, rd: u8) -> u32 {
    assert!(
        offset % 2 == 0 && (-(1 << 20)..(1 << 20)).contains(&offset),
        "jump offset out of range: {offset}"
    );
    let o = offset as u32;
    (o >> 20 & 1) << 31
        | (o >> 1 & 0x3ff) << 21
        | (o >> 11 & 1) << 20
        | (o >> 12 & 0xff) << 12
        | u32::from(rd) << 7
        | 0b110_1111
}

/// A conditional branch with a byte offset from the branch itself.
pub fn branch_word(cond: Cond, rs1: Reg, rs2: Reg, offset: i32) -> u32 {
    b_type(offset, rs2.0, rs1.0, cond.funct3())
}

/// `jal rd, offset` with a byte offset from the instruction itself.
pub fn jal_word(rd: Reg, offset: i32) -> u32 {
    j_type(offset, rd.0)
}

impl Asm {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of bytes emitted so far.
    pub fn len_bytes(&self) -> usize {
        self.words.len() * 4
    }

    /// Append one raw instruction word.
    pub fn emit(&mut self, word: u32) {
        self.words.push(word);
    }

    // ---- integer ----------------------------------------------------------

    pub fn add(&mut self, rd: Reg, rs1: Reg, rs2: Reg) {
        self.emit(r_type(0, rs2.0, rs1.0, 0b000, rd.0, 0b011_0011));
    }
    pub fn sub(&mut self, rd: Reg, rs1: Reg, rs2: Reg) {
        self.emit(r_type(0b010_0000, rs2.0, rs1.0, 0b000, rd.0, 0b011_0011));
    }
    pub fn and(&mut self, rd: Reg, rs1: Reg, rs2: Reg) {
        self.emit(r_type(0, rs2.0, rs1.0, 0b111, rd.0, 0b011_0011));
    }
    pub fn or(&mut self, rd: Reg, rs1: Reg, rs2: Reg) {
        self.emit(r_type(0, rs2.0, rs1.0, 0b110, rd.0, 0b011_0011));
    }
    pub fn xor(&mut self, rd: Reg, rs1: Reg, rs2: Reg) {
        self.emit(r_type(0, rs2.0, rs1.0, 0b100, rd.0, 0b011_0011));
    }
    pub fn mul(&mut self, rd: Reg, rs1: Reg, rs2: Reg) {
        self.emit(r_type(0b000_0001, rs2.0, rs1.0, 0b000, rd.0, 0b011_0011));
    }
    pub fn addi(&mut self, rd: Reg, rs1: Reg, imm: i32) {
        self.emit(i_type(imm, rs1.0, 0b000, rd.0, 0b001_0011));
    }
    pub fn addiw(&mut self, rd: Reg, rs1: Reg, imm: i32) {
        self.emit(i_type(imm, rs1.0, 0b000, rd.0, 0b001_1011));
    }
    pub fn andi(&mut self, rd: Reg, rs1: Reg, imm: i32) {
        self.emit(i_type(imm, rs1.0, 0b111, rd.0, 0b001_0011));
    }
    pub fn ori(&mut self, rd: Reg, rs1: Reg, imm: i32) {
        self.emit(i_type(imm, rs1.0, 0b110, rd.0, 0b001_0011));
    }
    pub fn xori(&mut self, rd: Reg, rs1: Reg, imm: i32) {
        self.emit(i_type(imm, rs1.0, 0b100, rd.0, 0b001_0011));
    }
    pub fn slli(&mut self, rd: Reg, rs1: Reg, shamt: u32) {
        assert!(shamt < 64);
        self.emit(shift(0, shamt, rs1.0, 0b001, rd.0));
    }
    pub fn srli(&mut self, rd: Reg, rs1: Reg, shamt: u32) {
        assert!(shamt < 64);
        self.emit(shift(0, shamt, rs1.0, 0b101, rd.0));
    }
    pub fn srai(&mut self, rd: Reg, rs1: Reg, shamt: u32) {
        assert!(shamt < 64);
        self.emit(shift(0b01_0000, shamt, rs1.0, 0b101, rd.0));
    }
    /// `lui rd, imm20`: `rd = sign_extend(imm20 << 12)`.
    pub fn lui(&mut self, rd: Reg, imm20: u32) {
        self.emit(u_type(imm20, rd.0, 0b011_0111));
    }
    pub fn auipc(&mut self, rd: Reg, imm20: u32) {
        self.emit(u_type(imm20, rd.0, 0b001_0111));
    }

    // ---- memory -----------------------------------------------------------

    pub fn ld(&mut self, rd: Reg, base: Reg, offset: i32) {
        self.emit(i_type(offset, base.0, 0b011, rd.0, 0b000_0011));
    }
    pub fn lw(&mut self, rd: Reg, base: Reg, offset: i32) {
        self.emit(i_type(offset, base.0, 0b010, rd.0, 0b000_0011));
    }
    pub fn lbu(&mut self, rd: Reg, base: Reg, offset: i32) {
        self.emit(i_type(offset, base.0, 0b100, rd.0, 0b000_0011));
    }
    pub fn sd(&mut self, src: Reg, base: Reg, offset: i32) {
        self.emit(s_type(offset, src.0, base.0, 0b011, 0b010_0011));
    }
    pub fn fld(&mut self, rd: FReg, base: Reg, offset: i32) {
        self.emit(i_type(offset, base.0, 0b011, rd.0, 0b000_0111));
    }
    pub fn fsd(&mut self, src: FReg, base: Reg, offset: i32) {
        self.emit(s_type(offset, src.0, base.0, 0b011, 0b010_0111));
    }

    // ---- floating point (double) -------------------------------------------

    fn fp(&mut self, funct7: u32, rs2: u8, rs1: u8, funct3: u32, rd: u8) {
        self.emit(r_type(funct7, rs2, rs1, funct3, rd, 0b101_0011));
    }
    pub fn fadd_d(&mut self, rd: FReg, rs1: FReg, rs2: FReg) {
        self.fp(0b000_0001, rs2.0, rs1.0, 0b111, rd.0);
    }
    pub fn fsub_d(&mut self, rd: FReg, rs1: FReg, rs2: FReg) {
        self.fp(0b000_0101, rs2.0, rs1.0, 0b111, rd.0);
    }
    pub fn fmul_d(&mut self, rd: FReg, rs1: FReg, rs2: FReg) {
        self.fp(0b000_1001, rs2.0, rs1.0, 0b111, rd.0);
    }
    pub fn fdiv_d(&mut self, rd: FReg, rs1: FReg, rs2: FReg) {
        self.fp(0b000_1101, rs2.0, rs1.0, 0b111, rd.0);
    }
    /// `fneg.d rd, rs` (`fsgnjn.d rd, rs, rs`).
    pub fn fneg_d(&mut self, rd: FReg, rs: FReg) {
        self.fp(0b001_0001, rs.0, rs.0, 0b001, rd.0);
    }
    /// Move the bits of an integer register into a floating-point register.
    pub fn fmv_d_x(&mut self, rd: FReg, rs: Reg) {
        self.fp(0b111_1001, 0, rs.0, 0b000, rd.0);
    }
    /// Move the bits of a floating-point register into an integer register.
    pub fn fmv_x_d(&mut self, rd: Reg, rs: FReg) {
        self.fp(0b111_0001, 0, rs.0, 0b000, rd.0);
    }
    pub fn feq_d(&mut self, rd: Reg, rs1: FReg, rs2: FReg) {
        self.fp(0b101_0001, rs2.0, rs1.0, 0b010, rd.0);
    }
    pub fn flt_d(&mut self, rd: Reg, rs1: FReg, rs2: FReg) {
        self.fp(0b101_0001, rs2.0, rs1.0, 0b001, rd.0);
    }
    pub fn fle_d(&mut self, rd: Reg, rs1: FReg, rs2: FReg) {
        self.fp(0b101_0001, rs2.0, rs1.0, 0b000, rd.0);
    }
    /// Classify a double into a one-hot mask (bit 0 -inf ... bit 9 quiet NaN).
    pub fn fclass_d(&mut self, rd: Reg, rs: FReg) {
        self.fp(0b111_0001, 0, rs.0, 0b001, rd.0);
    }
    /// Convert a double to a 64-bit integer, rounding toward zero.
    pub fn fcvt_l_d_rtz(&mut self, rd: Reg, rs: FReg) {
        self.fp(0b110_0001, 2, rs.0, 0b001, rd.0);
    }
    /// Convert a 64-bit integer to a double.
    pub fn fcvt_d_l(&mut self, rd: FReg, rs: Reg) {
        self.fp(0b110_1001, 2, rs.0, 0b111, rd.0);
    }

    // ---- control ------------------------------------------------------------

    /// `jalr rd, offset(rs1)`.
    pub fn jalr(&mut self, rd: Reg, rs1: Reg, offset: i32) {
        self.emit(i_type(offset, rs1.0, 0b000, rd.0, 0b110_0111));
    }
    /// Return: `jalr zero, 0(ra)`.
    pub fn ret(&mut self) {
        self.jalr(ZERO, RA, 0);
    }
    /// Call the function whose address is in `rs`: `jalr ra, 0(rs)`.
    pub fn call_reg(&mut self, rs: Reg) {
        self.jalr(RA, rs, 0);
    }
    pub fn fence_i(&mut self) {
        self.emit(0x0000_100f);
    }
    pub fn ebreak(&mut self) {
        self.emit(0x0010_0073);
    }
    pub fn ecall(&mut self) {
        self.emit(0x0000_0073);
    }

    // ---- labels and jumps -----------------------------------------------------

    pub fn new_label(&mut self) -> Label {
        self.labels.push(None);
        Label(self.labels.len() - 1)
    }

    /// Place `label` at the current position.
    pub fn bind(&mut self, label: Label) {
        self.labels[label.0] = Some(self.words.len());
    }

    /// Jump to `label`, saving the return address in `rd` (`ZERO` to discard).
    pub fn jal(&mut self, rd: Reg, label: Label) {
        self.fixups.push(Fixup {
            at: self.words.len(),
            label,
            rd,
        });
        self.emit(0); // patched in `finish`
    }

    /// Unconditional jump to `label`.
    pub fn j(&mut self, label: Label) {
        self.jal(ZERO, label);
    }

    /// Branch to `label` if `cond` holds for `rs1` and `rs2`. Emitted as the
    /// opposite short branch over a `jal`, so the target may be up to 1 MiB away.
    pub fn branch(&mut self, cond: Cond, rs1: Reg, rs2: Reg, label: Label) {
        self.emit(branch_word(cond.inverse(), rs1, rs2, 8));
        self.j(label);
    }
    pub fn beqz(&mut self, rs: Reg, label: Label) {
        self.branch(Cond::Eq, rs, ZERO, label);
    }
    pub fn bnez(&mut self, rs: Reg, label: Label) {
        self.branch(Cond::Ne, rs, ZERO, label);
    }

    // ---- pseudo-instructions ----------------------------------------------------

    /// `mv rd, rs`.
    pub fn mv(&mut self, rd: Reg, rs: Reg) {
        self.addi(rd, rs, 0);
    }

    /// Load any 64-bit constant into `rd`.
    pub fn li(&mut self, rd: Reg, value: i64) {
        if (-2048..2048).contains(&value) {
            self.addi(rd, ZERO, value as i32);
        } else if i32::try_from(value).is_ok() {
            let value = value as i32;
            let hi = ((i64::from(value) + 0x800) >> 12) as u32;
            let lo = (value << 20) >> 20; // sign-extended low 12 bits
            self.lui(rd, hi);
            if lo != 0 {
                self.addiw(rd, rd, lo);
            }
        } else {
            let lo = (value << 52) >> 52;
            // Exact: divisible by 4096. (Wrapping: for values near i64::MAX the
            // subtraction overflows, and the result is only ever used mod 2^64.)
            let mut hi = value.wrapping_sub(lo) >> 12;
            let shift = hi.trailing_zeros();
            hi >>= shift;
            self.li(rd, hi);
            self.slli(rd, rd, 12 + shift);
            if lo != 0 {
                self.addi(rd, rd, lo as i32);
            }
        }
    }

    /// Resolve the labels and return the machine code as little-endian bytes.
    pub fn finish(mut self) -> Result<Vec<u8>, AsmError> {
        for fixup in core::mem::take(&mut self.fixups) {
            let target = self.labels[fixup.label.0].ok_or(AsmError::UnboundLabel)?;
            let offset = (target as i64 - fixup.at as i64) * 4;
            if !(-(1 << 20)..(1 << 20)).contains(&offset) {
                return Err(AsmError::OutOfRange);
            }
            self.words[fixup.at] = j_type(offset as i32, fixup.rd.0);
        }
        let mut bytes = Vec::with_capacity(self.words.len() * 4);
        for word in &self.words {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        Ok(bytes)
    }
}

/// The shift-immediate format: 6-bit shift amount.
fn shift(funct6: u32, shamt: u32, rs1: u8, funct3: u32, rd: u8) -> u32 {
    funct6 << 26
        | shamt << 20
        | u32::from(rs1) << 15
        | funct3 << 12
        | u32::from(rd) << 7
        | 0b001_0011
}
