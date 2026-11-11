//! A small RV64 emulator, for testing generated code on the host without a
//! RISC-V machine. It implements every instruction [`crate::Asm`] can emit
//! (RV64I subset, M multiply, D double-precision) and nothing else: an
//! unsupported instruction is an error, never silently ignored.
//!
//! Memory is one flat byte region. A jump to an address outside it is a call to
//! the host: either the halt address (the run is over) or a registered service
//! that the `hook` handles (reading and writing registers), after which
//! execution continues after the call.

use alloc::vec;
use alloc::vec::Vec;

/// Returning to this address (the initial `ra`) ends a run.
pub const HALT: u64 = 0xFFFF_F000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmuError {
    /// Fetching an instruction from outside memory.
    BadFetch(u64),
    /// A load or store outside memory.
    BadAccess(u64),
    /// An instruction word the emulator does not implement.
    Unsupported(u32),
    /// The step limit ran out.
    Timeout,
}

#[derive(Debug, Clone)]
pub struct Emulator {
    /// Integer registers (`x[0]` is always zero).
    pub x: [u64; 32],
    /// Floating-point registers, as raw bits.
    pub f: [u64; 32],
    pc: u64,
    base: u64,
    mem: Vec<u8>,
}

fn sext(value: u64, bits: u32) -> i64 {
    ((value << (64 - bits)) as i64) >> (64 - bits)
}

impl Emulator {
    /// Memory of `size` bytes at address `base`; `sp` starts at the top.
    pub fn new(base: u64, size: usize) -> Self {
        let mut emulator = Emulator {
            x: [0; 32],
            f: [0; 32],
            pc: 0,
            base,
            mem: vec![0; size],
        };
        emulator.x[2] = base + size as u64;
        emulator
    }

    fn index(&self, addr: u64, len: usize) -> Result<usize, EmuError> {
        let offset = addr.wrapping_sub(self.base);
        if offset
            .checked_add(len as u64)
            .is_some_and(|end| end <= self.mem.len() as u64)
        {
            Ok(offset as usize)
        } else {
            Err(EmuError::BadAccess(addr))
        }
    }

    pub fn write_bytes(&mut self, addr: u64, bytes: &[u8]) -> Result<(), EmuError> {
        let i = self.index(addr, bytes.len())?;
        self.mem[i..i + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }

    pub fn write_u64(&mut self, addr: u64, value: u64) -> Result<(), EmuError> {
        self.write_bytes(addr, &value.to_le_bytes())
    }

    pub fn read_u64(&self, addr: u64) -> Result<u64, EmuError> {
        let i = self.index(addr, 8)?;
        Ok(u64::from_le_bytes(self.mem[i..i + 8].try_into().unwrap()))
    }

    fn set(&mut self, rd: usize, value: u64) {
        if rd != 0 {
            self.x[rd] = value;
        }
    }

    /// Call the code at `entry` with integer `args` (in `a0`...), and run until
    /// it returns. Returns `a0`. `hook(target, emulator)` handles calls to
    /// addresses outside memory.
    pub fn call(
        &mut self,
        entry: u64,
        args: &[u64],
        max_steps: usize,
        hook: &mut dyn FnMut(u64, &mut Emulator),
    ) -> Result<u64, EmuError> {
        self.x[1] = HALT;
        for (i, &arg) in args.iter().enumerate() {
            self.x[10 + i] = arg;
        }
        self.pc = entry;
        for _ in 0..max_steps {
            if self.step(hook)? {
                return Ok(self.x[10]);
            }
        }
        Err(EmuError::Timeout)
    }

    /// Execute one instruction; `true` when the run is over.
    fn step(&mut self, hook: &mut dyn FnMut(u64, &mut Emulator)) -> Result<bool, EmuError> {
        let at = self
            .index(self.pc, 4)
            .map_err(|_| EmuError::BadFetch(self.pc))?;
        let w = u32::from_le_bytes(self.mem[at..at + 4].try_into().unwrap());
        let rd = (w >> 7 & 31) as usize;
        let rs1 = (w >> 15 & 31) as usize;
        let rs2 = (w >> 20 & 31) as usize;
        let funct3 = w >> 12 & 7;
        let funct7 = w >> 25;
        let imm_i = (w as i32 >> 20) as i64;
        let mut next = self.pc.wrapping_add(4);
        let a = self.x[rs1];
        let b = self.x[rs2];

        match w & 0x7f {
            0x37 => self.set(rd, (w & 0xffff_f000) as i32 as i64 as u64),
            0x17 => self.set(
                rd,
                self.pc.wrapping_add((w & 0xffff_f000) as i32 as i64 as u64),
            ),
            0x13 => {
                let v = match funct3 {
                    0 => a.wrapping_add(imm_i as u64),
                    1 => a << (w >> 20 & 63),
                    4 => a ^ imm_i as u64,
                    5 if w >> 30 & 1 == 1 => ((a as i64) >> (w >> 20 & 63)) as u64,
                    5 => a >> (w >> 20 & 63),
                    6 => a | imm_i as u64,
                    7 => a & imm_i as u64,
                    _ => return Err(EmuError::Unsupported(w)),
                };
                self.set(rd, v);
            }
            0x1b if funct3 == 0 => {
                self.set(rd, (a as i32).wrapping_add(imm_i as i32) as i64 as u64)
            }
            0x33 => {
                let v = match (funct7, funct3) {
                    (0, 0) => a.wrapping_add(b),
                    (0x20, 0) => a.wrapping_sub(b),
                    (0, 4) => a ^ b,
                    (0, 6) => a | b,
                    (0, 7) => a & b,
                    (1, 0) => a.wrapping_mul(b),
                    _ => return Err(EmuError::Unsupported(w)),
                };
                self.set(rd, v);
            }
            0x03 => {
                let addr = a.wrapping_add(imm_i as u64);
                let v = match funct3 {
                    2 => {
                        let i = self.index(addr, 4)?;
                        i32::from_le_bytes(self.mem[i..i + 4].try_into().unwrap()) as i64 as u64
                    }
                    3 => self.read_u64(addr)?,
                    4 => {
                        let i = self.index(addr, 1)?;
                        u64::from(self.mem[i])
                    }
                    _ => return Err(EmuError::Unsupported(w)),
                };
                self.set(rd, v);
            }
            0x23 if funct3 == 3 => {
                let offset = sext(u64::from(funct7 << 5 | (w >> 7 & 31)), 12);
                self.write_u64(a.wrapping_add(offset as u64), b)?;
            }
            0x07 if funct3 == 3 => {
                let v = self.read_u64(a.wrapping_add(imm_i as u64))?;
                self.f[rd] = v;
            }
            0x27 if funct3 == 3 => {
                let offset = sext(u64::from(funct7 << 5 | (w >> 7 & 31)), 12);
                self.write_u64(a.wrapping_add(offset as u64), self.f[rs2])?;
            }
            0x63 => {
                let off = (w >> 31 & 1) << 12
                    | (w >> 7 & 1) << 11
                    | (w >> 25 & 0x3f) << 5
                    | (w >> 8 & 0xf) << 1;
                let off = sext(u64::from(off), 13);
                let taken = match funct3 {
                    0 => a == b,
                    1 => a != b,
                    4 => (a as i64) < (b as i64),
                    5 => (a as i64) >= (b as i64),
                    6 => a < b,
                    7 => a >= b,
                    _ => return Err(EmuError::Unsupported(w)),
                };
                if taken {
                    next = self.pc.wrapping_add(off as u64);
                }
            }
            0x6f => {
                let off = (w >> 31 & 1) << 20
                    | (w >> 12 & 0xff) << 12
                    | (w >> 20 & 1) << 11
                    | (w >> 21 & 0x3ff) << 1;
                let off = sext(u64::from(off), 21);
                self.set(rd, next);
                next = self.pc.wrapping_add(off as u64);
            }
            0x67 if funct3 == 0 => {
                let target = a.wrapping_add(imm_i as u64) & !1;
                let link = next;
                self.set(rd, link);
                if target == HALT {
                    return Ok(true);
                }
                if self.index(target, 4).is_err() {
                    // A call to a host service: run it, then continue after the call.
                    hook(target, self);
                } else {
                    next = target;
                }
            }
            0x0f if funct3 == 1 => {} // fence.i
            0x53 => self.fp_op(w, rd, rs1, rs2, funct3, funct7)?,
            _ => return Err(EmuError::Unsupported(w)),
        }
        self.pc = next;
        Ok(false)
    }

    fn fp_op(
        &mut self,
        w: u32,
        rd: usize,
        rs1: usize,
        rs2: usize,
        funct3: u32,
        funct7: u32,
    ) -> Result<(), EmuError> {
        let fa = f64::from_bits(self.f[rs1]);
        let fb = f64::from_bits(self.f[rs2]);
        match funct7 {
            0x01 => self.f[rd] = (fa + fb).to_bits(),
            0x05 => self.f[rd] = (fa - fb).to_bits(),
            0x09 => self.f[rd] = (fa * fb).to_bits(),
            0x0d => self.f[rd] = (fa / fb).to_bits(),
            0x11 if funct3 == 1 => {
                // fsgnjn.d: magnitude of rs1, opposite of rs2's sign
                self.f[rd] = (self.f[rs1] & !(1 << 63)) | (!self.f[rs2] & (1 << 63));
            }
            0x51 => {
                let v = match funct3 {
                    2 => fa == fb,
                    1 => fa < fb,
                    0 => fa <= fb,
                    _ => return Err(EmuError::Unsupported(w)),
                };
                if rd != 0 {
                    self.x[rd] = u64::from(v);
                }
            }
            0x61 if rs2 == 2 => {
                let v = fa as i64; // truncates toward zero, as rtz does
                if rd != 0 {
                    self.x[rd] = v as u64;
                }
            }
            0x69 if rs2 == 2 => self.f[rd] = (self.x[rs1] as i64 as f64).to_bits(),
            0x71 if funct3 == 0 => {
                if rd != 0 {
                    self.x[rd] = self.f[rs1];
                }
            }
            0x71 if funct3 == 1 => {
                let bits = self.f[rs1];
                let sign = bits >> 63 == 1;
                let exponent = bits >> 52 & 0x7ff;
                let fraction = bits & ((1 << 52) - 1);
                let class = if exponent == 0x7ff {
                    if fraction == 0 {
                        if sign {
                            0
                        } else {
                            7
                        }
                    } else if fraction >> 51 == 1 {
                        9
                    } else {
                        8
                    }
                } else if exponent == 0 {
                    match (fraction == 0, sign) {
                        (true, true) => 3,
                        (true, false) => 4,
                        (false, true) => 2,
                        (false, false) => 5,
                    }
                } else if sign {
                    1
                } else {
                    6
                };
                if rd != 0 {
                    self.x[rd] = 1 << class;
                }
            }
            0x79 if funct3 == 0 => self.f[rd] = self.x[rs1],
            _ => return Err(EmuError::Unsupported(w)),
        }
        Ok(())
    }
}
