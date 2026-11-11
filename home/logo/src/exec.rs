//! The IR interpreter: runs lowered code against the same runtime (primitives,
//! variables, procedures, turtle) as the AST interpreter. It must behave
//! identically (see "The IR" in `LOGO.md`).

use crate::error::LogoError;
use crate::interp::{infix, Interpreter, Machine, Signal};
use crate::ir::{Instr, Lowered, Temp};
use crate::prims::{num, truth};
use crate::value::Value;
use crate::Io;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use turtle::Canvas;

/// One temp: a value or nothing, and the name of what produced it.
#[derive(Debug, Clone, Default)]
struct Slot {
    value: Option<Value>,
    producer: String,
}

/// Run a lowered line.
pub(crate) fn run<I: Io, C: Canvas>(
    it: &mut Interpreter,
    m: &mut Machine<I, C>,
    lowered: &Lowered,
) -> Result<(), Signal> {
    let mut slots = vec![Slot::default(); lowered.temps];
    block(it, m, &lowered.block, &mut slots)
}

fn block<I: Io, C: Canvas>(
    it: &mut Interpreter,
    m: &mut Machine<I, C>,
    instrs: &[Instr],
    slots: &mut [Slot],
) -> Result<(), Signal> {
    for instr in instrs {
        step(it, m, instr, slots)?;
    }
    Ok(())
}

/// The value in `temp`, for an instruction named `consumer`.
fn input(slots: &[Slot], temp: Temp, consumer: &str) -> Result<Value, Signal> {
    match &slots[temp].value {
        Some(value) => Ok(value.clone()),
        None => {
            Err(LogoError::DidntOutput(slots[temp].producer.clone(), String::from(consumer)).into())
        }
    }
}

fn set(slots: &mut [Slot], temp: Temp, value: Option<Value>, producer: &str) {
    slots[temp] = Slot {
        value,
        producer: String::from(producer),
    };
}

fn step<I: Io, C: Canvas>(
    it: &mut Interpreter,
    m: &mut Machine<I, C>,
    instr: &Instr,
    slots: &mut [Slot],
) -> Result<(), Signal> {
    match instr {
        Instr::Const { dst, value } => set(slots, *dst, Some(value.clone()), ""),
        Instr::Var { dst, name } => match it.lookup(name) {
            Some(value) => set(slots, *dst, Some(value), ""),
            None => return Err(LogoError::NoValue(name.clone()).into()),
        },
        Instr::Neg { dst, src } => {
            let result = match input(slots, *src, "-")? {
                Value::Num(n) => Value::Num(-n),
                other => return Err(LogoError::DoesntLike(String::from("-"), other).into()),
            };
            set(slots, *dst, Some(result), "");
        }
        Instr::Infix { dst, op, a, b } => {
            let x = input(slots, *a, op.symbol())?;
            let y = input(slots, *b, op.symbol())?;
            set(slots, *dst, Some(infix(*op, x, y)?), "");
        }
        Instr::Call { dst, name, args } => {
            let mut values = Vec::with_capacity(args.len());
            for arg in args {
                values.push(input(slots, *arg, name)?);
            }
            let result = it.call(m, name, values)?;
            set(slots, *dst, result, name);
        }
        Instr::Discard { src } => {
            if let Some(value) = &slots[*src].value {
                return Err(LogoError::DontSayWhatToDo(value.clone()).into());
            }
        }
        Instr::Repeat { dst, count, body } => {
            if m.io.break_pressed() {
                return Err(LogoError::Stopped.into());
            }
            let n = num(&input(slots, *count, "repeat")?, "repeat")?;
            let times = if n > 0.0 { libm::floor(n) as usize } else { 0 };
            for _ in 0..times {
                block(it, m, body, slots)?;
            }
            set(slots, *dst, None, "repeat");
        }
        Instr::If {
            dst,
            name,
            cond,
            then_branch,
            else_branch,
        } => {
            if m.io.break_pressed() {
                return Err(LogoError::Stopped.into());
            }
            let chosen = if truth(&input(slots, *cond, name)?, name)? {
                then_branch
            } else {
                else_branch
            };
            block(it, m, chosen, slots)?;
            set(slots, *dst, None, name);
        }
        Instr::Fail(error) => return Err(error.clone().into()),
    }
    Ok(())
}
