//! The name rule: hand-derived goldens, and the rule again from `std` (an independent statement of it: `str::from_utf8` and
//! `char::is_control`) on random bytes.
use names::{validate, validate_text, NameError::*, MAX_NAME};

#[test]
fn accepted_names() {
    for n in [
        &b"a"[..],
        b"hello.txt",
        b".hidden",
        b"...",
        b"a b",
        b"caf\xc3\xa9",              // é, precomposed
        b"cafe\xcc\x81",             // e + combining acute: a different name
        b"\xe6\x97\xa5\xe6\x9c\xac", // 日本
        b"\xf0\x9f\x98\x80",         // 😀 (four bytes)
        b"\xc2\xa0",                 // NO-BREAK SPACE (not a control)
        b"\xe2\x80\xae",             // U+202E: allowed (T141 records the look-alike problem)
    ] {
        assert_eq!(validate(n), Ok(()), "{n:?}");
    }
    assert_ne!(
        "caf\u{e9}".as_bytes(),
        "cafe\u{301}".as_bytes(),
        "two different names that look alike"
    );
}

#[test]
fn refused_names() {
    assert_eq!(validate(b""), Err(Empty));
    assert_eq!(validate(b"a/b"), Err(Slash));
    assert_eq!(validate(b"/"), Err(Slash));
    assert_eq!(validate(b"."), Err(Dots));
    assert_eq!(validate(b".."), Err(Dots));
    assert_eq!(validate(b"a\0b"), Err(Nul));
    assert_eq!(validate(b"\0"), Err(Nul));
    assert_eq!(validate(b"a\nb"), Err(Control), "newline");
    assert_eq!(validate(b"\x1b[31m"), Err(Control), "ESC");
    assert_eq!(validate(b"x\x7f"), Err(Control), "DEL");
    assert_eq!(
        validate(b"\xc2\x80"),
        Err(Control),
        "U+0080, the first C1 control"
    );
    assert_eq!(
        validate(b"\xc2\x9f"),
        Err(Control),
        "U+009F, the last C1 control"
    );
    assert_eq!(validate(b"\xc2\xa0"), Ok(()), "U+00A0 is not one");
    assert_eq!(validate(b"\xc0\x80"), Err(NotUtf8), "an overlong NUL");
    assert_eq!(validate(b"\xc0\xaf"), Err(NotUtf8), "an overlong slash");
    assert_eq!(validate(b"\xed\xa0\x80"), Err(NotUtf8), "a surrogate");
    assert_eq!(
        validate(b"\xf4\x90\x80\x80"),
        Err(NotUtf8),
        "above U+10FFFF"
    );
    assert_eq!(validate(b"\x80"), Err(NotUtf8), "a stray continuation byte");
    assert_eq!(validate(b"\xff"), Err(NotUtf8));
    assert_eq!(validate(b"\xe6\x97"), Err(NotUtf8), "a truncated sequence");
    assert_eq!(validate(b"caf\xe9"), Err(NotUtf8), "Latin-1");
}

#[test]
fn the_length_limit_is_in_bytes() {
    assert_eq!(validate(&vec![b'x'; MAX_NAME]), Ok(()));
    assert_eq!(validate(&vec![b'x'; MAX_NAME + 1]), Err(TooLong));
    // 85 three-byte characters are 255 bytes, 86 are 258
    assert_eq!(validate("日".repeat(85).as_bytes()), Ok(()));
    assert_eq!(validate("日".repeat(86).as_bytes()), Err(TooLong));
}

#[test]
fn text_names_may_contain_dots_but_not_controls() {
    assert_eq!(validate_text(b"posix.mode"), Ok(()));
    assert_eq!(validate_text(b"user.\xc3\xa9"), Ok(()));
    assert_eq!(validate_text(b"a\x01"), Err(Control));
    assert_eq!(validate_text(b"\xff"), Err(NotUtf8));
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// The rule as `std` states it.
fn reference(n: &[u8]) -> bool {
    match core::str::from_utf8(n) {
        Ok(s) => {
            !n.is_empty()
                && n.len() <= 255
                && s != "."
                && s != ".."
                && !s.contains('/')
                && !s.chars().any(char::is_control)
        }
        Err(_) => false,
    }
}

#[test]
fn the_rule_matches_std_on_random_bytes() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    // bytes from the interesting places: ASCII, controls, the continuation and lead ranges, and the invalid ones
    let pool: Vec<u8> = (0u8..=0x20)
        .chain([
            0x2e, 0x2f, 0x41, 0x7e, 0x7f, 0x80, 0x9f, 0xa0, 0xbf, 0xc0, 0xc2, 0xc3, 0xdf, 0xe0,
            0xe2, 0xed, 0xef, 0xf0, 0xf4, 0xf5, 0xff,
        ])
        .collect();
    let mut ok = 0;
    for _ in 0..200_000 {
        let len = (rng.next() % 6) as usize;
        let n: Vec<u8> = (0..len)
            .map(|_| pool[(rng.next() % pool.len() as u64) as usize])
            .collect();
        assert_eq!(validate(&n).is_ok(), reference(&n), "{n:?}");
        if reference(&n) {
            ok += 1;
        }
    }
    assert!(ok > 1000, "the generator reaches accepted names ({ok})");
}
