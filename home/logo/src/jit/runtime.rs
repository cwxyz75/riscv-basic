//! The runtime services behind the table in `ctx`: what generated code calls
//! for everything it does not do inline. They are thin: each unboxes its inputs
//! and uses the same functions the interpreters use, so behaviour (and every
//! error message) is shared by construction.

use super::boxing::{nothing, Boxes, Unboxed};
use super::codegen::{op_from_code, Compiled};
use super::{ExecuteFn, JitHost, Ret};
use crate::error::LogoError;
use crate::interp::{infix, Interpreter, Machine, Signal};
use crate::prims::{num, truth};
use crate::value::Value;
use crate::Io;
use alloc::string::String;
use alloc::vec::Vec;
use turtle::Canvas;

/// Run one compiled unit with `execute`, returning what the interpreters would.
pub(crate) fn run_unit<I: Io, C: Canvas>(
    it: &mut Interpreter,
    m: &mut Machine<I, C>,
    unit: &Compiled,
    execute: ExecuteFn,
) -> Result<(), Signal> {
    let mut runtime = Runtime {
        it,
        m,
        unit,
        boxes: Boxes::new(),
        pending: None,
    };
    let status = execute(&unit.code, unit.frame_bytes, &mut runtime);
    match (status, runtime.pending.take()) {
        (0, None) => Ok(()),
        (_, Some(signal)) => Err(signal),
        // A non-zero status with nothing pending cannot happen.
        (_, None) => Err(LogoError::DontKnowHow(String::from("(internal error)")).into()),
    }
}

struct Runtime<'a, 'm, I: Io, C: Canvas> {
    it: &'a mut Interpreter,
    m: &'a mut Machine<'m, I, C>,
    unit: &'a Compiled,
    boxes: Boxes,
    /// The error or `stop`/`output` that ended the run, if any.
    pending: Option<Signal>,
}

impl<I: Io, C: Canvas> Runtime<'_, '_, I, C> {
    /// Record why the run is stopping and tell the generated code to bail out.
    fn raise(&mut self, signal: impl Into<Signal>) -> Ret {
        self.pending = Some(signal.into());
        Ret { value: 0, err: 1 }
    }

    fn ok(value: u64) -> Ret {
        Ret { value, err: 0 }
    }

    fn name(&self, index: u64) -> &str {
        &self.unit.names[index as usize]
    }

    /// The value a temp holds, for an instruction named `consumer`.
    fn input(&self, bits: u64, consumer: &str) -> Result<Value, Signal> {
        match self.boxes.get(bits) {
            Unboxed::Value(value) => Ok(value),
            Unboxed::Nothing(producer) => Err(LogoError::DidntOutput(
                self.unit.names[producer].clone(),
                String::from(consumer),
            )
            .into()),
        }
    }

    fn boxed(&mut self, value: &Value) -> Ret {
        match self.boxes.put(value) {
            Ok(bits) => Self::ok(bits),
            Err(error) => self.raise(error),
        }
    }
}

impl<I: Io, C: Canvas> JitHost for Runtime<'_, '_, I, C> {
    fn var(&mut self, name: u64) -> Ret {
        let name = String::from(self.name(name));
        match self.it.lookup(&name) {
            Some(value) => self.boxed(&value),
            None => self.raise(LogoError::NoValue(name)),
        }
    }

    fn constant(&mut self, index: u64) -> Ret {
        let value = self.unit.consts[index as usize].clone();
        self.boxed(&value)
    }

    fn neg(&mut self, bits: u64) -> Ret {
        match self.input(bits, "-") {
            Ok(Value::Num(n)) => self.boxed(&Value::Num(-n)),
            Ok(other) => self.raise(LogoError::DoesntLike(String::from("-"), other)),
            Err(signal) => self.raise(signal),
        }
    }

    fn infix(&mut self, op: u64, a: u64, b: u64) -> Ret {
        let Some(op) = op_from_code(op) else {
            return self.raise(LogoError::DontKnowHow(String::from("(bad operator)")));
        };
        let x = match self.input(a, op.symbol()) {
            Ok(v) => v,
            Err(signal) => return self.raise(signal),
        };
        let y = match self.input(b, op.symbol()) {
            Ok(v) => v,
            Err(signal) => return self.raise(signal),
        };
        match infix(op, x, y) {
            Ok(result) => self.boxed(&result),
            Err(error) => self.raise(error),
        }
    }

    fn call(&mut self, name_index: u64, args: &[u64]) -> Ret {
        let name = String::from(self.name(name_index));
        let mut values = Vec::with_capacity(args.len());
        for &bits in args {
            match self.input(bits, &name) {
                Ok(value) => values.push(value),
                Err(signal) => return self.raise(signal),
            }
        }
        match self.it.call(self.m, &name, values) {
            Ok(Some(value)) => self.boxed(&value),
            Ok(None) => Self::ok(nothing(name_index as usize)),
            Err(signal) => self.raise(signal),
        }
    }

    fn discard(&mut self, bits: u64) -> Ret {
        match self.boxes.get(bits) {
            Unboxed::Value(value) => self.raise(LogoError::DontSayWhatToDo(value)),
            Unboxed::Nothing(_) => Self::ok(0),
        }
    }

    fn repeat_count(&mut self, bits: u64) -> Ret {
        if self.m.io.break_pressed() {
            return self.raise(LogoError::Stopped);
        }
        let count = match self
            .input(bits, "repeat")
            .and_then(|v| num(&v, "repeat").map_err(Signal::from))
        {
            Ok(n) => n,
            Err(signal) => return self.raise(signal),
        };
        Self::ok(if count > 0.0 {
            libm::floor(count) as u64
        } else {
            0
        })
    }

    fn truth(&mut self, bits: u64, name: u64) -> Ret {
        if self.m.io.break_pressed() {
            return self.raise(LogoError::Stopped);
        }
        let name = String::from(self.name(name));
        match self
            .input(bits, &name)
            .and_then(|v| truth(&v, &name).map_err(Signal::from))
        {
            Ok(b) => Self::ok(u64::from(b)),
            Err(signal) => self.raise(signal),
        }
    }

    fn fail(&mut self, index: u64) -> Ret {
        let error = self.unit.failures[index as usize].clone();
        self.raise(error)
    }
}
