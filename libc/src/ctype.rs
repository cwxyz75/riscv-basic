//! `<ctype.h>` in the "C" locale (ASCII).

use core::ffi::c_int;

fn b(c: c_int) -> Option<u8> {
    u8::try_from(c).ok()
}

macro_rules! class {
    ($name:ident, $test:expr) => {
        #[no_mangle]
        pub extern "C" fn $name(c: c_int) -> c_int {
            c_int::from(b(c).is_some_and($test))
        }
    };
}

class!(isalpha, |c| c.is_ascii_alphabetic());
class!(isdigit, |c| c.is_ascii_digit());
class!(isalnum, |c| c.is_ascii_alphanumeric());
class!(isspace, |c| matches!(
    c,
    b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'
));
class!(isupper, |c| c.is_ascii_uppercase());
class!(islower, |c| c.is_ascii_lowercase());
class!(isprint, |c| (0x20..0x7f).contains(&c));
class!(isgraph, |c| (0x21..0x7f).contains(&c));
class!(ispunct, |c| c.is_ascii_punctuation());
class!(iscntrl, |c| c < 0x20 || c == 0x7f);
class!(isxdigit, |c| c.is_ascii_hexdigit());

#[no_mangle]
pub extern "C" fn tolower(c: c_int) -> c_int {
    b(c).map_or(c, |x| c_int::from(x.to_ascii_lowercase()))
}

#[no_mangle]
pub extern "C" fn toupper(c: c_int) -> c_int {
    b(c).map_or(c, |x| c_int::from(x.to_ascii_uppercase()))
}
