//! The multibyte core against `std`: decoding agrees with `core::str::from_utf8` on random bytes (one byte at a time, so the
//! incremental state is exercised too), encoding with `char::encode_utf8`, and the classes with `std`'s for the ranges they define.
use cfmt::mb::{
    decode, encode, is_alpha, is_digit, is_space, to_lower, to_upper, width, Decoded, State,
};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Decode all of `bytes` the way `mbrtowc` is called in a loop; `None` at the first invalid sequence or a truncated end.
fn decode_all(bytes: &[u8], chunk: usize) -> Option<Vec<u32>> {
    let mut st = State::default();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let end = (i + chunk).min(bytes.len());
        let mut piece = &bytes[i..end];
        loop {
            match decode(&mut st, piece) {
                Decoded::Char(c, used) => {
                    out.push(c);
                    piece = &piece[used..];
                    if piece.is_empty() {
                        break;
                    }
                }
                Decoded::Incomplete => break,
                Decoded::Invalid => return None,
            }
        }
        i = end;
    }
    if st.len != 0 {
        return None;
    }
    Some(out)
}

#[test]
fn decoding_agrees_with_std() {
    let pool: Vec<u8> = [
        0x00, 0x41, 0x7f, 0x80, 0xbf, 0xc0, 0xc1, 0xc2, 0xdf, 0xe0, 0xe1, 0xed, 0xef, 0xf0, 0xf4,
        0xf5, 0xff, 0x9f, 0xa0, 0x8f, 0x90,
    ]
    .to_vec();
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let mut valid = 0;
    for _ in 0..200_000 {
        let n = (rng.next() % 7) as usize;
        let bytes: Vec<u8> = (0..n)
            .map(|_| pool[(rng.next() % pool.len() as u64) as usize])
            .collect();
        let want: Option<Vec<u32>> = core::str::from_utf8(&bytes)
            .ok()
            .map(|s| s.chars().map(u32::from).collect());
        if want.is_some() {
            valid += 1;
        }
        for chunk in [1, 2, 7] {
            assert_eq!(
                decode_all(&bytes, chunk),
                want,
                "{bytes:?} in chunks of {chunk}"
            );
        }
    }
    assert!(valid > 20_000, "the generator reaches valid text ({valid})");
}

#[test]
fn encoding_agrees_with_std() {
    for c in (0u32..0x1_1000).chain([0x10ffff, 0x110000, 0xd800, 0xdfff]) {
        let mut ours = [0u8; 4];
        let mut buf = [0u8; 4];
        match char::from_u32(c) {
            Some(ch) => {
                let n = encode(c, &mut ours).unwrap();
                assert_eq!(&ours[..n], ch.encode_utf8(&mut buf).as_bytes());
            }
            None => assert_eq!(encode(c, &mut ours), None),
        }
    }
}

#[test]
fn classes_widths_and_case() {
    assert!(is_alpha('é' as u32) && is_alpha('日' as u32) && !is_alpha('1' as u32));
    assert!(
        is_digit('7' as u32) && !is_digit('٣' as u32),
        "digit is ASCII only"
    );
    assert!(
        is_space(' ' as u32) && is_space(0x3000) && !is_space(0xa0),
        "no-break space is not a space"
    );
    assert_eq!(to_upper('é' as u32), 'É' as u32);
    assert_eq!(to_lower('Σ' as u32), 'σ' as u32);
    assert_eq!(to_upper('ß' as u32), 'ß' as u32, "one-to-many maps stay");
    assert_eq!(
        (
            width('a' as u32),
            width('日' as u32),
            width(0x301),
            width(0),
            width(7),
            width(0x1f600)
        ),
        (1, 2, 0, 0, -1, 2)
    );
    assert_eq!(width(0xd800), -1);
}
