//! Number output, as `PRINT` writes it (rule in "Number output" in `BASIC.md`).

use alloc::format;
use alloc::string::String;

/// Format a finite number the way `PRINT` shows it: a sign column (space or
/// `-`), the value rounded to 9 significant digits, and a trailing space.
pub(crate) fn format_number(value: f64) -> String {
    let mut out = String::new();
    out.push(if value < 0.0 { '-' } else { ' ' });
    let magnitude = value.abs();
    if magnitude == 0.0 {
        out.push_str("0 ");
        return out;
    }

    // `{:.8e}` rounds exactly to 9 significant digits: "d.dddddddde<exp>".
    let scientific = format!("{magnitude:.8e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let mut digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }

    if (-2..=8).contains(&exponent) {
        if exponent >= 0 {
            let integer_len = exponent as usize + 1;
            if digits.len() <= integer_len {
                out.push_str(&digits);
                for _ in digits.len()..integer_len {
                    out.push('0');
                }
            } else {
                out.push_str(&digits[..integer_len]);
                out.push('.');
                out.push_str(&digits[integer_len..]);
            }
        } else {
            out.push_str("0.");
            for _ in 0..(-exponent - 1) {
                out.push('0');
            }
            out.push_str(&digits);
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('E');
        out.push(if exponent < 0 { '-' } else { '+' });
        out.push_str(&format!("{:02}", exponent.abs()));
    }
    out.push(' ');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_examples() {
        assert_eq!(format_number(3.0), " 3 ");
        assert_eq!(format_number(-2.5), "-2.5 ");
        assert_eq!(format_number(1.0 / 3.0), " 0.333333333 ");
        assert_eq!(format_number(1e9), " 1E+09 ");
    }

    #[test]
    fn zero_has_no_sign() {
        assert_eq!(format_number(0.0), " 0 ");
        assert_eq!(format_number(-0.0), " 0 ");
    }

    #[test]
    fn range_boundaries() {
        assert_eq!(format_number(0.01), " 0.01 ");
        assert_eq!(format_number(0.0099), " 9.9E-03 ");
        assert_eq!(format_number(999_999_999.0), " 999999999 ");
        // Rounds up into the next decade, so it is scientific.
        assert_eq!(format_number(999_999_999.6), " 1E+09 ");
    }

    #[test]
    fn rounds_to_nine_significant_digits() {
        assert_eq!(format_number(0.1 + 0.2), " 0.3 ");
        assert_eq!(format_number(1234567890.0), " 1.23456789E+09 ");
        assert_eq!(format_number(-1.5e20), "-1.5E+20 ");
    }
}
