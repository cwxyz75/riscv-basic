//! The code generator: IR to RV64 machine code (scheme in "Compilation to RV64"
//! in `LOGO.md`). A template compiler: every temp has a stack slot, each
//! instruction loads its operands, does its work, stores its result. Number
//! literals, negation, `+ - *` on numbers, `Discard`, `Repeat` and `If` are
//! generated inline; everything else is a call through the runtime table.

use super::abi::*;
use super::boxing::{nothing, TAG_NOTHING, TAG_WORD};
use crate::error::LogoError;
use crate::ir::{Instr, Lowered, Temp};
use crate::parse::Infix;
use crate::value::Value;
use alloc::string::String;
use alloc::vec::Vec;
use rv64::*;

/// A compiled unit: machine code plus the pools it indexes into.
#[derive(Debug, Clone)]
pub(crate) struct Compiled {
    pub(crate) code: Vec<u8>,
    /// Stack the code needs (its frame), in bytes.
    pub(crate) frame_bytes: usize,
    /// Names used by `Var`/`Call`/`If`, referenced by index.
    pub(crate) names: Vec<String>,
    /// Word and list literals, referenced by index.
    pub(crate) consts: Vec<Value>,
    /// Errors for `Fail` instructions, referenced by index.
    pub(crate) failures: Vec<LogoError>,
}

/// The `Infix` operation numbers the runtime service understands.
pub(crate) fn op_code(op: Infix) -> u64 {
    match op {
        Infix::Add => 0,
        Infix::Sub => 1,
        Infix::Mul => 2,
        Infix::Div => 3,
        Infix::Eq => 4,
        Infix::Lt => 5,
        Infix::Gt => 6,
    }
}

pub(crate) fn op_from_code(code: u64) -> Option<Infix> {
    Some(match code {
        0 => Infix::Add,
        1 => Infix::Sub,
        2 => Infix::Mul,
        3 => Infix::Div,
        4 => Infix::Eq,
        5 => Infix::Lt,
        6 => Infix::Gt,
        _ => return None,
    })
}

/// `fclass.d` classes that are not finite numbers: -inf, +inf, signalling NaN, quiet NaN.
const NOT_FINITE: i64 = 1 << 0 | 1 << 7 | 1 << 8 | 1 << 9;

pub(crate) fn compile(lowered: &Lowered) -> Compiled {
    let mut scan = Scan::default();
    scan.block(&lowered.block);
    let arg_area = 8 * scan.max_args.max(1);
    let slots = lowered.temps + scan.loops;
    let frame = (arg_area + 8 * slots + 16 + 15) & !15;
    let mut asm = Asm::new();
    let error = asm.new_label();
    let mut g = Gen {
        a: asm,
        names: Vec::new(),
        consts: Vec::new(),
        failures: Vec::new(),
        arg_area,
        temps: lowered.temps,
        frame,
        next_loop: 0,
        error,
    };
    let exit = g.a.new_label();
    g.prologue();
    g.block(&lowered.block);
    g.a.li(A0, 0);
    g.a.j(exit);
    g.a.bind(g.error);
    g.a.li(A0, 1);
    g.a.bind(exit);
    g.epilogue();
    Compiled {
        code: g.a.finish().expect("generated code assembles"),
        frame_bytes: frame,
        names: g.names,
        consts: g.consts,
        failures: g.failures,
    }
}

/// A first pass: how many call inputs and `repeat` loops there can be.
#[derive(Default)]
struct Scan {
    max_args: usize,
    loops: usize,
}

impl Scan {
    fn block(&mut self, instrs: &[Instr]) {
        for instr in instrs {
            match instr {
                Instr::Call { args, .. } => self.max_args = self.max_args.max(args.len()),
                Instr::Repeat { body, .. } => {
                    self.loops += 1;
                    self.block(body);
                }
                Instr::If {
                    then_branch,
                    else_branch,
                    ..
                } => {
                    self.block(then_branch);
                    self.block(else_branch);
                }
                _ => {}
            }
        }
    }
}

struct Gen {
    a: Asm,
    names: Vec<String>,
    consts: Vec<Value>,
    failures: Vec<LogoError>,
    /// Bytes reserved at the bottom of the frame for call arguments.
    arg_area: usize,
    temps: usize,
    frame: usize,
    next_loop: usize,
    /// Where every runtime error exits.
    error: Label,
}

impl Gen {
    fn intern(&mut self, name: &str) -> usize {
        if let Some(i) = self.names.iter().position(|n| n == name) {
            return i;
        }
        self.names.push(String::from(name));
        self.names.len() - 1
    }

    fn temp_offset(&self, temp: Temp) -> i32 {
        (self.arg_area + 8 * temp) as i32
    }

    fn loop_offset(&self, index: usize) -> i32 {
        (self.arg_area + 8 * (self.temps + index)) as i32
    }

    // ---- frame access (offsets past 2047 go through t6) ------------------------

    fn load_sp(&mut self, rd: Reg, offset: i32) {
        if offset < 2040 {
            self.a.ld(rd, SP, offset);
        } else {
            self.a.li(T6, i64::from(offset));
            self.a.add(T6, SP, T6);
            self.a.ld(rd, T6, 0);
        }
    }

    fn store_sp(&mut self, rs: Reg, offset: i32) {
        if offset < 2040 {
            self.a.sd(rs, SP, offset);
        } else {
            self.a.li(T6, i64::from(offset));
            self.a.add(T6, SP, T6);
            self.a.sd(rs, T6, 0);
        }
    }

    fn load_temp(&mut self, rd: Reg, temp: Temp) {
        let offset = self.temp_offset(temp);
        self.load_sp(rd, offset);
    }

    fn store_temp(&mut self, rs: Reg, temp: Temp) {
        let offset = self.temp_offset(temp);
        self.store_sp(rs, offset);
    }

    fn prologue(&mut self) {
        let frame = self.frame as i64;
        if frame <= 2047 {
            self.a.addi(SP, SP, -(frame as i32));
        } else {
            self.a.li(T6, frame);
            self.a.sub(SP, SP, T6);
        }
        self.store_sp(RA, self.frame as i32 - 8);
        self.store_sp(S0, self.frame as i32 - 16);
        self.a.mv(S0, A0);
    }

    fn epilogue(&mut self) {
        self.load_sp(RA, self.frame as i32 - 8);
        self.load_sp(S0, self.frame as i32 - 16);
        let frame = self.frame as i64;
        if frame <= 2047 {
            self.a.addi(SP, SP, frame as i32);
        } else {
            self.a.li(T6, frame);
            self.a.add(SP, SP, T6);
        }
        self.a.ret();
    }

    /// Call runtime service `service`: arguments are already in `a1`..; `a0`
    /// is set to `ctx` here.
    fn service(&mut self, service: usize) {
        self.a.mv(A0, S0);
        self.a.ld(T5, S0, 8 * service as i32);
        self.a.call_reg(T5);
    }

    /// After a service: leave through the error exit if it reported an error.
    fn check(&mut self) {
        self.a.bnez(A1, self.error);
    }

    // ---- instructions -------------------------------------------------------------

    fn block(&mut self, instrs: &[Instr]) {
        for instr in instrs {
            self.instr(instr);
        }
    }

    fn instr(&mut self, instr: &Instr) {
        match instr {
            Instr::Const { dst, value } => match value {
                Value::Num(n) => {
                    self.a.li(T0, n.to_bits() as i64);
                    self.store_temp(T0, *dst);
                }
                other => {
                    self.consts.push(other.clone());
                    self.a.li(A1, (self.consts.len() - 1) as i64);
                    self.service(SVC_CONST);
                    self.check();
                    self.store_temp(A0, *dst);
                }
            },
            Instr::Var { dst, name } => {
                let name = self.intern(name);
                self.a.li(A1, name as i64);
                self.service(SVC_VAR);
                self.check();
                self.store_temp(A0, *dst);
            }
            Instr::Neg { dst, src } => self.neg(*dst, *src),
            Instr::Infix { dst, op, a, b } => self.infix(*dst, *op, *a, *b),
            Instr::Call { dst, name, args } => {
                for (i, arg) in args.iter().enumerate() {
                    self.load_temp(T0, *arg);
                    self.a.sd(T0, SP, 8 * i as i32);
                }
                let name = self.intern(name);
                self.a.li(A1, name as i64);
                self.a.li(A2, args.len() as i64);
                self.a.mv(A3, SP);
                self.service(SVC_CALL);
                self.check();
                self.store_temp(A0, *dst);
            }
            Instr::Discard { src } => {
                let done = self.a.new_label();
                self.load_temp(T0, *src);
                self.a.srli(T1, T0, 48);
                self.a.li(T2, TAG_NOTHING as i64);
                self.a.branch(Cond::Eq, T1, T2, done);
                self.a.mv(A1, T0);
                self.service(SVC_DISCARD);
                self.check();
                self.a.bind(done);
            }
            Instr::Repeat { dst, count, body } => {
                let slot = self.loop_offset(self.next_loop);
                self.next_loop += 1;
                self.load_temp(A1, *count);
                self.service(SVC_REPEAT_COUNT);
                self.check();
                self.store_sp(A0, slot);
                let top = self.a.new_label();
                let end = self.a.new_label();
                self.a.bind(top);
                self.load_sp(T0, slot);
                self.a.beqz(T0, end);
                self.a.addi(T0, T0, -1);
                self.store_sp(T0, slot);
                self.block(body);
                self.a.j(top);
                self.a.bind(end);
                let producer = self.intern("repeat");
                self.a.li(T0, nothing(producer) as i64);
                self.store_temp(T0, *dst);
            }
            Instr::If {
                dst,
                name,
                cond,
                then_branch,
                else_branch,
            } => {
                let name = self.intern(name);
                self.load_temp(A1, *cond);
                self.a.li(A2, name as i64);
                self.service(SVC_TRUTH);
                self.check();
                let otherwise = self.a.new_label();
                let end = self.a.new_label();
                self.a.beqz(A0, otherwise);
                self.block(then_branch);
                self.a.j(end);
                self.a.bind(otherwise);
                self.block(else_branch);
                self.a.bind(end);
                self.a.li(T0, nothing(name) as i64);
                self.store_temp(T0, *dst);
            }
            Instr::Fail(error) => {
                self.failures.push(error.clone());
                self.a.li(A1, (self.failures.len() - 1) as i64);
                self.service(SVC_FAIL);
                self.check();
            }
        }
    }

    /// Negation: flip the sign bit of a number; anything else is an error (the
    /// runtime says which).
    fn neg(&mut self, dst: Temp, src: Temp) {
        let slow = self.a.new_label();
        let done = self.a.new_label();
        self.load_temp(T0, src);
        self.a.li(T3, TAG_WORD as i64);
        self.a.srli(T2, T0, 48);
        self.a.branch(Cond::Geu, T2, T3, slow);
        self.a.li(T1, i64::MIN);
        self.a.xor(T0, T0, T1);
        self.store_temp(T0, dst);
        self.a.j(done);
        self.a.bind(slow);
        self.load_temp(A1, src);
        self.service(SVC_NEG);
        self.check();
        self.store_temp(A0, dst);
        self.a.bind(done);
    }

    /// An infix operator. `+ - *` on two numbers whose result is finite are done
    /// in the FPU right here; everything else (other operators, other types,
    /// overflow) goes to the runtime, which also produces the errors.
    fn infix(&mut self, dst: Temp, op: Infix, a: Temp, b: Temp) {
        let slow = self.a.new_label();
        let done = self.a.new_label();
        if matches!(op, Infix::Add | Infix::Sub | Infix::Mul) {
            self.load_temp(T0, a);
            self.load_temp(T1, b);
            self.a.li(T3, TAG_WORD as i64);
            self.a.srli(T2, T0, 48);
            self.a.branch(Cond::Geu, T2, T3, slow);
            self.a.srli(T2, T1, 48);
            self.a.branch(Cond::Geu, T2, T3, slow);
            self.a.fmv_d_x(FT0, T0);
            self.a.fmv_d_x(FT1, T1);
            match op {
                Infix::Add => self.a.fadd_d(FT2, FT0, FT1),
                Infix::Sub => self.a.fsub_d(FT2, FT0, FT1),
                _ => self.a.fmul_d(FT2, FT0, FT1),
            }
            self.a.fclass_d(T4, FT2);
            self.a.li(T2, NOT_FINITE);
            self.a.and(T4, T4, T2);
            self.a.bnez(T4, slow);
            self.a.fmv_x_d(T0, FT2);
            self.store_temp(T0, dst);
            self.a.j(done);
        }
        self.a.bind(slow);
        self.a.li(A1, op_code(op) as i64);
        self.load_temp(A2, a);
        self.load_temp(A3, b);
        self.service(SVC_INFIX);
        self.check();
        self.store_temp(A0, dst);
        self.a.bind(done);
    }
}
