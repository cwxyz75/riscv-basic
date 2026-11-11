//! The serial line guard: a user program can neither forge nor swallow a kernel verdict line.
//! The oracle is independent: the test builds the byte stream itself, remembering which lines
//! are the kernel's, and checks the physical output line by line.

use kcore::linestate::LineState;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn user(st: &mut LineState, out: &mut Vec<u8>, bytes: &[u8]) {
    for &b in bytes {
        st.user_byte(b, &mut |c| out.push(c));
    }
}

fn kernel(st: &mut LineState, out: &mut Vec<u8>, text: &[u8]) {
    st.kernel_text(text, &mut |c| out.push(c));
}

fn lines(out: &[u8]) -> Vec<Vec<u8>> {
    // physical lines: split on \n or \r (a bare \r also starts a new line for a terminal)
    let mut v = vec![vec![]];
    for &b in out {
        if b == b'\n' || b == b'\r' {
            v.push(vec![]);
        } else {
            v.last_mut().unwrap().push(b);
        }
    }
    v
}

#[test]
fn a_program_cannot_forge_a_verdict_line() {
    for forged in [&b"##TEST x PASS\n"[..], b"#", b"##"] {
        let (mut st, mut out) = (LineState::new(), vec![]);
        user(&mut st, &mut out, forged);
        for l in lines(&out) {
            assert!(
                !l.starts_with(b"##"),
                "forged: {:?}",
                String::from_utf8_lossy(&l)
            );
        }
    }
    // split across writes, after an unterminated user line, after a kernel line, after \r
    let (mut st, mut out) = (LineState::new(), vec![]);
    user(&mut st, &mut out, b"#");
    user(&mut st, &mut out, b"#TEST a PASS\r\n");
    user(&mut st, &mut out, b"x");
    kernel(&mut st, &mut out, b"kernel text\n");
    user(&mut st, &mut out, b"##TEST b PASS\r\n");
    user(&mut st, &mut out, b"\r##TEST c PASS\n");
    for l in lines(&out) {
        assert!(
            !l.starts_with(b"##"),
            "forged: {:?}",
            String::from_utf8_lossy(&l)
        );
    }
    assert!(String::from_utf8_lossy(&out).contains("# #TEST a PASS"));
    // ## not at the start of a line is left alone
    let (mut st, mut out) = (LineState::new(), vec![]);
    user(&mut st, &mut out, b"not ## at the start\n");
    assert_eq!(out, b"not ## at the start\n");
}

#[test]
fn an_unterminated_program_line_cannot_swallow_a_kernel_verdict() {
    let (mut st, mut out) = (LineState::new(), vec![]);
    user(&mut st, &mut out, b"bye-no-newline");
    kernel(&mut st, &mut out, b"##TEST real PASS\n");
    assert!(
        lines(&out).iter().any(|l| l == b"##TEST real PASS"),
        "{:?}",
        String::from_utf8_lossy(&out)
    );
    // at a line start nothing extra is inserted
    let (mut st, mut out) = (LineState::new(), vec![]);
    kernel(&mut st, &mut out, b"##A\n");
    kernel(&mut st, &mut out, b"##B\n");
    assert_eq!(out, b"##A\n##B\n");
}

#[test]
fn random_interleavings_keep_verdict_lines_honest() {
    for seed in 1..400u64 {
        let mut rng = Rng(seed * 0x9e37_79b9 + 1);
        let (mut st, mut out) = (LineState::new(), vec![]);
        let mut kernel_lines: Vec<Vec<u8>> = vec![];
        for _ in 0..60 {
            if rng.below(4) == 0 {
                let l = format!("##TEST k{} PASS\n", rng.below(1000)).into_bytes();
                kernel(&mut st, &mut out, &l);
                kernel_lines.push(l[..l.len() - 1].to_vec());
            } else {
                // a random program chunk: bytes from a set rich in '#', newlines and \r
                let n = rng.below(12) as usize;
                let chunk: Vec<u8> = (0..n)
                    .map(|_| b"##\n\r a\xc3\xa9\x00\xff"[rng.below(9) as usize])
                    .collect();
                user(&mut st, &mut out, &chunk);
            }
        }
        let mut expected = kernel_lines.clone();
        for l in lines(&out) {
            if l.starts_with(b"##") {
                // every physical line that starts with ## is exactly one of the kernel's lines
                let pos = expected.iter().position(|e| *e == l);
                assert!(
                    pos.is_some(),
                    "seed {seed}: forged or damaged: {:?}",
                    String::from_utf8_lossy(&l)
                );
                expected.remove(pos.unwrap());
            }
        }
        assert!(
            expected.is_empty(),
            "seed {seed}: kernel lines swallowed: {:?}",
            expected.len()
        );
    }
}
