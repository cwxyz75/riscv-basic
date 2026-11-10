//! The primitives of LOGO v0.2 (the list is in "Reader, evaluation and errors"
//! in `LOGO.md`).

use crate::error::LogoError;
use crate::interp::{Eval, Interpreter, Machine, Signal};
use crate::lex::lex;
use crate::value::Value;
use crate::Io;
use alloc::string::String;
use alloc::vec::Vec;
use turtle::Canvas;

/// The primitive's real name, with its abbreviations resolved.
fn canonical(name: &str) -> &str {
    match name {
        "fd" => "forward",
        "bk" => "back",
        "lt" => "left",
        "rt" => "right",
        "pu" => "penup",
        "pd" => "pendown",
        "cs" => "clearscreen",
        "ht" => "hideturtle",
        "st" => "showturtle",
        "seth" => "setheading",
        "setpc" => "setpencolor",
        "bf" => "butfirst",
        "bl" => "butlast",
        "se" => "sentence",
        "op" => "output",
        other => other,
    }
}

/// How many inputs the primitive `name` takes, or `None` if there is no such
/// primitive.
pub(crate) fn arity(name: &str) -> Option<usize> {
    Some(match canonical(name) {
        "penup" | "pendown" | "home" | "clearscreen" | "clean" | "hideturtle" | "showturtle"
        | "fill" | "pos" | "heading" | "xcor" | "ycor" | "stop" | "bye" => 0,
        "forward" | "back" | "left" | "right" | "setheading" | "setpencolor" | "wait" | "print"
        | "show" | "type" | "sqrt" | "sin" | "cos" | "arctan" | "abs" | "int" | "round"
        | "random" | "first" | "last" | "butfirst" | "butlast" | "count" | "emptyp" | "listp"
        | "wordp" | "numberp" | "not" | "output" | "thing" | "run" => 1,
        "setxy" | "repeat" | "if" | "make" | "fput" | "lput" | "list" | "sentence" | "item"
        | "and" | "or" => 2,
        "ifelse" => 3,
        _ => return None,
    })
}

pub(crate) fn num(value: &Value, name: &str) -> Result<f64, LogoError> {
    match value {
        Value::Num(n) => Ok(*n),
        other => Err(LogoError::DoesntLike(String::from(name), other.clone())),
    }
}

pub(crate) fn truth(value: &Value, name: &str) -> Result<bool, LogoError> {
    match value {
        Value::Word(w) if w.eq_ignore_ascii_case("true") => Ok(true),
        Value::Word(w) if w.eq_ignore_ascii_case("false") => Ok(false),
        other => Err(LogoError::DoesntLike(String::from(name), other.clone())),
    }
}

fn list_of<'a>(value: &'a Value, name: &str) -> Result<&'a [Value], LogoError> {
    match value {
        Value::List(items) => Ok(items),
        other => Err(LogoError::DoesntLike(String::from(name), other.clone())),
    }
}

fn bad(name: &str, value: &Value) -> LogoError {
    LogoError::DoesntLike(String::from(name), value.clone())
}

/// A list's items as source text, for `run` and friends.
pub(crate) fn source_of(items: &[Value]) -> String {
    let mut out = String::new();
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&item.source_form());
    }
    out
}

fn chars_of(word: &str) -> Vec<char> {
    word.chars().collect()
}

fn run_list<I: Io, C: Canvas>(
    it: &mut Interpreter,
    m: &mut Machine<I, C>,
    items: &[Value],
    times: usize,
) -> Result<(), Signal> {
    let tokens = lex(&source_of(items))?;
    for _ in 0..times {
        it.run_tokens(m, &tokens)?;
    }
    Ok(())
}

/// Run the primitive `name` on its evaluated inputs.
pub(crate) fn call<I: Io, C: Canvas>(
    it: &mut Interpreter,
    m: &mut Machine<I, C>,
    name: &str,
    args: Vec<Value>,
) -> Eval {
    let a = &args[..];
    let value = |v: Value| Ok(Some(v));
    match canonical(name) {
        // --- turtle ---
        "forward" => it.turtle.forward(m.canvas, num(&a[0], name)?),
        "back" => it.turtle.back(m.canvas, num(&a[0], name)?),
        "left" => it.turtle.left(m.canvas, num(&a[0], name)?),
        "right" => it.turtle.right(m.canvas, num(&a[0], name)?),
        "setheading" => it.turtle.set_heading(m.canvas, num(&a[0], name)?),
        "setxy" => it
            .turtle
            .set_xy(m.canvas, num(&a[0], name)?, num(&a[1], name)?),
        "penup" => it.turtle.pen_up(),
        "pendown" => it.turtle.pen_down(),
        "home" => it.turtle.home(m.canvas),
        "clearscreen" => it.turtle.clear_screen(m.canvas),
        "clean" => it.turtle.clean(m.canvas),
        "hideturtle" => it.turtle.hide(m.canvas),
        "showturtle" => it.turtle.show(m.canvas),
        "fill" => it.turtle.fill(m.canvas),
        "setpencolor" => {
            let color = num(&a[0], name)?;
            if !(0.0..=15.0).contains(&color) || color != libm::floor(color) {
                return Err(bad(name, &a[0]).into());
            }
            it.turtle.set_pen_color(color as u8);
        }
        "pos" => {
            return value(Value::List(alloc::vec![
                Value::Num(it.turtle.x()),
                Value::Num(it.turtle.y())
            ]))
        }
        "heading" => return value(Value::Num(it.turtle.heading())),
        "xcor" => return value(Value::Num(it.turtle.x())),
        "ycor" => return value(Value::Num(it.turtle.y())),

        // --- control ---
        "repeat" => {
            let count = num(&a[0], name)?;
            let items = list_of(&a[1], name)?;
            let times = if count > 0.0 {
                libm::floor(count) as usize
            } else {
                0
            };
            run_list(it, m, items, times)?;
        }
        "if" => {
            if truth(&a[0], name)? {
                run_list(it, m, list_of(&a[1], name)?, 1)?;
            }
        }
        "ifelse" => {
            let chosen = if truth(&a[0], name)? { &a[1] } else { &a[2] };
            run_list(it, m, list_of(chosen, name)?, 1)?;
        }
        "run" => run_list(it, m, list_of(&a[0], name)?, 1)?,
        "stop" => {
            if !it.in_procedure() {
                return Err(LogoError::OnlyInProcedure.into());
            }
            return Err(Signal::Stop);
        }
        "output" => {
            if !it.in_procedure() {
                return Err(LogoError::OnlyInProcedure.into());
            }
            return Err(Signal::Output(a[0].clone()));
        }
        "wait" => {
            let sixtieths = num(&a[0], name)?;
            let until =
                m.io.millis()
                    .saturating_add((sixtieths * 1000.0 / 60.0) as u64);
            while m.io.millis() < until {
                if m.io.break_pressed() {
                    return Err(LogoError::Stopped.into());
                }
            }
        }
        "bye" => m.io.reboot(),

        // --- data and printing ---
        "make" => match &a[0] {
            Value::Word(w) => it.assign(w.to_lowercase(), a[1].clone()),
            other => return Err(bad(name, other).into()),
        },
        "thing" => match &a[0] {
            Value::Word(w) => {
                let key = w.to_lowercase();
                return match it.lookup(&key) {
                    Some(v) => value(v),
                    None => Err(LogoError::NoValue(key).into()),
                };
            }
            other => return Err(bad(name, other).into()),
        },
        "print" => {
            let mut text = a[0].print_form();
            text.push('\n');
            m.io.write_str(&text);
        }
        "show" => {
            let mut text = a[0].show_form();
            text.push('\n');
            m.io.write_str(&text);
        }
        "type" => m.io.write_str(&a[0].print_form()),
        "random" => {
            let n = num(&a[0], name)?;
            if n < 1.0 {
                return Err(bad(name, &a[0]).into());
            }
            return value(Value::Num(it.rng.below(n)));
        }

        // --- arithmetic functions ---
        "sqrt" => {
            let x = num(&a[0], name)?;
            if x < 0.0 {
                return Err(bad(name, &a[0]).into());
            }
            return value(Value::Num(libm::sqrt(x)));
        }
        "sin" => return value(Value::Num(libm::sin(num(&a[0], name)? * DEGREES))),
        "cos" => return value(Value::Num(libm::cos(num(&a[0], name)? * DEGREES))),
        "arctan" => return value(Value::Num(libm::atan(num(&a[0], name)?) / DEGREES)),
        "abs" => return value(Value::Num(num(&a[0], name)?.abs())),
        "int" => return value(Value::Num(libm::trunc(num(&a[0], name)?))),
        "round" => return value(Value::Num(libm::round(num(&a[0], name)?))),
        "and" => return value(Value::boolean(truth(&a[0], name)? & truth(&a[1], name)?)),
        "or" => return value(Value::boolean(truth(&a[0], name)? | truth(&a[1], name)?)),
        "not" => return value(Value::boolean(!truth(&a[0], name)?)),

        // --- words and lists ---
        "first" => return value(first_or_last(&a[0], name, true)?),
        "last" => return value(first_or_last(&a[0], name, false)?),
        "butfirst" => return value(but(&a[0], name, true)?),
        "butlast" => return value(but(&a[0], name, false)?),
        "fput" => {
            let mut items = alloc::vec![a[0].clone()];
            items.extend_from_slice(list_of(&a[1], name)?);
            return value(Value::List(items));
        }
        "lput" => {
            let mut items = list_of(&a[1], name)?.to_vec();
            items.push(a[0].clone());
            return value(Value::List(items));
        }
        "list" => return value(Value::List(alloc::vec![a[0].clone(), a[1].clone()])),
        "sentence" => {
            let mut items = Vec::new();
            for part in a {
                match part {
                    Value::List(inner) => items.extend_from_slice(inner),
                    other => items.push(other.clone()),
                }
            }
            return value(Value::List(items));
        }
        "count" => {
            return value(Value::Num(match &a[0] {
                Value::Word(w) => w.chars().count() as f64,
                Value::List(items) => items.len() as f64,
                other => return Err(bad(name, other).into()),
            }))
        }
        "item" => {
            let n = num(&a[0], name)?;
            let index = if n >= 1.0 && n == libm::floor(n) {
                n as usize - 1
            } else {
                return Err(bad(name, &a[0]).into());
            };
            let found = match &a[1] {
                Value::Word(w) => chars_of(w)
                    .get(index)
                    .map(|c| Value::Word(String::from(*c))),
                Value::List(items) => items.get(index).cloned(),
                other => return Err(bad(name, other).into()),
            };
            return match found {
                Some(v) => value(v),
                None => Err(bad(name, &a[0]).into()),
            };
        }
        "emptyp" => {
            return value(Value::boolean(match &a[0] {
                Value::Word(w) => w.is_empty(),
                Value::List(items) => items.is_empty(),
                Value::Num(_) => false,
            }))
        }
        "listp" => return value(Value::boolean(matches!(a[0], Value::List(_)))),
        "wordp" => return value(Value::boolean(matches!(a[0], Value::Word(_)))),
        "numberp" => return value(Value::boolean(matches!(a[0], Value::Num(_)))),

        _ => return Err(LogoError::DontKnowHow(String::from(name)).into()),
    }
    Ok(None)
}

const DEGREES: f64 = core::f64::consts::PI / 180.0;

fn first_or_last(v: &Value, name: &str, first: bool) -> Result<Value, LogoError> {
    match v {
        Value::Word(w) if !w.is_empty() => {
            let c = if first {
                w.chars().next()
            } else {
                w.chars().last()
            };
            Ok(Value::Word(String::from(c.unwrap_or_default())))
        }
        Value::List(items) if !items.is_empty() => Ok(if first {
            items[0].clone()
        } else {
            items[items.len() - 1].clone()
        }),
        other => Err(bad(name, other)),
    }
}

fn but(v: &Value, name: &str, first: bool) -> Result<Value, LogoError> {
    match v {
        Value::Word(w) if !w.is_empty() => {
            let chars = chars_of(w);
            let rest = if first {
                &chars[1..]
            } else {
                &chars[..chars.len() - 1]
            };
            Ok(Value::Word(rest.iter().collect()))
        }
        Value::List(items) if !items.is_empty() => Ok(Value::List(if first {
            items[1..].to_vec()
        } else {
            items[..items.len() - 1].to_vec()
        })),
        other => Err(bad(name, other)),
    }
}
