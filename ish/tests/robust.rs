//! The parser and evaluator never panic or hang on hostile input: random token soup, deep nesting, runaway recursion and
//! loops. (The `es` differential of grammar v1 is gone, SHELL-SPEC SH2b: the independent oracles are in `oracles.rs`.)

use ish::fake::FakeHost;
use ish::Shell;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const TOKENS: &[&str] = &[
    "echo",
    "print",
    "x",
    "=",
    "$x",
    "$#x",
    "$x.y",
    "(",
    ")",
    "[",
    "]",
    "{",
    "}",
    "do",
    "end",
    "else",
    "if",
    "for",
    "in",
    "fn",
    "try",
    "catch",
    "|",
    "||",
    "&&",
    "!",
    ";",
    "\n",
    ">",
    ">>",
    "<",
    ">[2=1]",
    "'a b'",
    "\"q\\n\"",
    "1",
    "2.5",
    "-",
    "set",
    "`t ${x}`",
    "+",
    "*",
    "/",
    "==",
    "<",
    "and",
    "not",
    "@",
    "^",
    ",",
    ":",
    "true",
    "null",
    "*.t",
    "?",
    "??",
    "ok",
    "fail",
    "exit",
    "break",
    "defer",
    "\\\n",
    "#c\n",
    "map",
    "each",
    "take",
    "fold",
    "seq",
    "3",
    "spawn",
    "run",
    "succeeds",
    "with-env",
    "wait",
    "wait-any",
    "poll",
    "kill",
    "parse-args",
    "lossy-lines",
    "slow",
    "hang",
    "exit-with",
    "{}",
    "[]",
    "$env",
    "$env.PATH",
    "$job",
    "$job.status",
    "{ \"timeout\": 5 }",
    "!&&",
    "||",
];

fn soup(r: &mut Rng) -> String {
    let n = 1 + r.below(14);
    let mut s = String::new();
    for _ in 0..n {
        s.push_str(TOKENS[r.below(TOKENS.len())]);
        if r.below(4) != 0 {
            s.push(' ');
        }
    }
    s
}

#[test]
fn parser_and_evaluator_survive_token_soup() {
    let mut r = Rng(0xdead_beef_cafe);
    for _ in 0..20_000 {
        let src = soup(&mut r);
        if let Ok(stmts) = ish::parse(&src) {
            // run it against the fake: it may fail, but it must end and not panic
            let mut sh = Shell::new(FakeHost::new());
            let _ = &stmts;
            let _ = sh.run_script(&src);
        }
    }
}

#[test]
fn arbitrary_characters_do_not_panic_the_parser() {
    let mut r = Rng(99);
    let alphabet: Vec<char> = "ab $#.()[]{}\"'\\|&;<>=!^@*?:,\n\t#éλ0123456789"
        .chars()
        .collect();
    for _ in 0..30_000 {
        let n = r.below(40);
        let src: String = (0..n).map(|_| alphabet[r.below(alphabet.len())]).collect();
        let _ = ish::parse(&src);
    }
}

#[test]
fn deep_nesting_is_an_error_not_a_stack_overflow() {
    for open in ["(", "[", "{", "(echo ", "if true do ", "(slow "] {
        let src = open.repeat(100_000);
        assert!(ish::parse(&src).is_err(), "{open:?}");
    }
    let ok = format!("{}1{}", "(".repeat(30), ")".repeat(30));
    assert!(ish::parse(&ok).is_ok());
    let too = format!("{}1{}", "(".repeat(70), ")".repeat(70));
    assert!(ish::parse(&too).is_err());
}

#[test]
fn runaway_recursion_ends_with_an_error() {
    let mut sh = Shell::new(FakeHost::new());
    let st = sh.run_script("fn f do f end\nf\n");
    assert_eq!(st, 1);
    assert!(
        sh.host.stderr_text().contains("nested too deeply"),
        "{}",
        sh.host.stderr_text()
    );
    // and the shell is still usable
    assert_eq!(sh.run_script("print ok"), 0);
}

#[test]
fn a_hostile_data_expression_is_bounded() {
    let mut sh = Shell::new(FakeHost::new());
    // doubling a list 40 times would be 2^40 elements; `range` and literals are bounded
    let st = sh.run_script("x = (range 5000000)\n");
    assert_ne!(st, 0, "an enormous range must be refused");
    assert_eq!(sh.run_script("print (len (range 100000))"), 0);
}
