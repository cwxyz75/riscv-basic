//! LOGO values and how they print.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Num(f64),
    Word(String),
    List(Vec<Value>),
}

/// A number as C's `%.12g` writes it (see "Printing" in `LOGO.md`).
pub(crate) fn format_number(x: f64) -> String {
    if x == 0.0 {
        return String::from("0");
    }
    if !x.is_finite() {
        return if x.is_nan() {
            String::from("nan")
        } else if x < 0.0 {
            String::from("-inf")
        } else {
            String::from("inf")
        };
    }
    let mut out = String::new();
    if x < 0.0 {
        out.push('-');
    }
    // `{:.11e}` rounds exactly to 12 significant digits: "d.ddddddddddde<exp>".
    let scientific = format!("{:.11e}", x.abs());
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let mut digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
    if !(-4..12).contains(&exponent) {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if exponent < 0 { '-' } else { '+' });
        out.push_str(&format!("{:02}", exponent.abs()));
    } else if exponent >= 0 {
        let whole = exponent as usize + 1;
        if digits.len() <= whole {
            out.push_str(&digits);
            for _ in digits.len()..whole {
                out.push('0');
            }
        } else {
            out.push_str(&digits[..whole]);
            out.push('.');
            out.push_str(&digits[whole..]);
        }
    } else {
        out.push_str("0.");
        for _ in 0..(-exponent - 1) {
            out.push('0');
        }
        out.push_str(&digits);
    }
    out
}

impl Value {
    pub(crate) fn word(text: &str) -> Value {
        Value::Word(String::from(text))
    }

    pub(crate) fn boolean(b: bool) -> Value {
        Value::word(if b { "true" } else { "false" })
    }

    /// As `show` writes it: lists keep their brackets.
    pub(crate) fn show_form(&self) -> String {
        match self {
            Value::Num(n) => format_number(*n),
            Value::Word(w) => w.clone(),
            Value::List(items) => {
                let mut out = String::from("[");
                out.push_str(&join(items));
                out.push(']');
                out
            }
        }
    }

    /// As `print` writes it: a list loses its outer brackets (nested ones stay).
    pub(crate) fn print_form(&self) -> String {
        match self {
            Value::List(items) => join(items),
            other => other.show_form(),
        }
    }

    /// Source text that reads back as this value, so lists can be `run`.
    pub(crate) fn source_form(&self) -> String {
        self.show_form()
    }

    /// `=`: numbers by value, words ignoring case, lists item by item.
    pub(crate) fn equals(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Num(a), Value::Num(b)) => a == b,
            (Value::Word(a), Value::Word(b)) => a.eq_ignore_ascii_case(b),
            (Value::List(a), Value::List(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.equals(y))
            }
            _ => false,
        }
    }
}

fn join(items: &[Value]) -> String {
    let mut out = String::new();
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&item.show_form());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_print_like_percent_g_12() {
        for (x, want) in [
            (3.0, "3"),
            (-2.5, "-2.5"),
            (0.1 + 0.2, "0.3"),
            (1.0 / 3.0, "0.333333333333"),
            (1e12, "1e+12"),
            (123456789012.0, "123456789012"),
            (1.5e-7, "1.5e-07"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (-0.0, "0"),
            (2.4329020081766e18, "2.43290200818e+18"),
        ] {
            assert_eq!(format_number(x), want, "{x}");
        }
    }

    #[test]
    fn lists_print_and_show() {
        let v = Value::List(alloc::vec![
            Value::word("a"),
            Value::List(alloc::vec![Value::Num(1.0), Value::word("b")]),
        ]);
        assert_eq!(v.print_form(), "a [1 b]");
        assert_eq!(v.show_form(), "[a [1 b]]");
        assert!(v.equals(&v.clone()));
        assert!(Value::word("A").equals(&Value::word("a")));
        assert!(!Value::Num(1.0).equals(&Value::word("1")));
    }
}
