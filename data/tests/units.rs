//! Behaviour tests for `data`: numbers, JSON details the suite does not pin, CSV and TSV vectors, the access primitives,
//! and property tests (a small xorshift generator, no dependency).

use data::number::{NumError, Number};
use data::*;

fn j(s: &str) -> Result<Value, Error> {
    from_json(s.as_bytes(), &Limits::DEFAULT)
}

fn ok(s: &str) -> Value {
    j(s).unwrap_or_else(|e| panic!("{s}: {e}"))
}

fn text(v: &Value) -> String {
    to_json(v, &Limits::DEFAULT).unwrap()
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

// ---- numbers ----

#[test]
fn numbers_are_canonical_and_exact() {
    for (src, out) in [
        ("0", "0"),
        ("-0", "0"),
        ("0.0", "0"),
        ("1.0", "1"),
        ("1e0", "1"),
        ("1E+2", "100"),
        ("12.50", "12.5"),
        ("-12.50", "-12.5"),
        ("0.001", "0.001"),
        ("1e-3", "0.001"),
        ("15e-1", "1.5"),
        (
            "123456789012345678901234567890",
            "123456789012345678901234567890",
        ),
        ("9223372036854775807", "9223372036854775807"),
        ("-9223372036854775808", "-9223372036854775808"),
        ("9223372036854775808", "9223372036854775808"),
        ("0.1", "0.1"),
        ("1.000000000000000000000000000000000000000000000", "1"),
    ] {
        let n = Number::parse(src.as_bytes()).unwrap_or_else(|e| panic!("{src}: {e:?}"));
        assert_eq!(n.format(), out, "{src}");
        assert_eq!(Number::parse(out.as_bytes()), Ok(n), "{src} round trip");
    }
    // an i64 is always Int, so equal numbers are equal values
    assert_eq!(Number::parse(b"3.0"), Ok(Number::Int(3)));
    assert!(Number::parse(b"0.5").unwrap() != Number::Int(0));
}

#[test]
fn a_tenth_does_not_drift() {
    // there is no double: the decimal keeps its digits
    let n = Number::parse(b"0.30000000000000004").unwrap();
    assert_eq!(n.format(), "0.30000000000000004");
    assert_eq!(n.parts(), (30000000000000004, 17));
}

#[test]
fn number_syntax_errors_and_limits() {
    for bad in [
        "", "-", "+1", "01", "1.", ".5", "1e", "1e+", "1.e3", "--1", "1 ", "0x10", "1e5.5", "١",
    ] {
        assert_eq!(
            Number::parse(bad.as_bytes()),
            Err(NumError::Syntax),
            "{bad:?}"
        );
    }
    // 38 digits fit, 39 do not
    let d38 = "9".repeat(38);
    assert_eq!(Number::parse(d38.as_bytes()).unwrap().format(), d38);
    assert_eq!(
        Number::parse(format!("{d38}9").as_bytes()),
        Err(NumError::TooPrecise)
    );
    // the scale is capped too
    assert!(Number::parse(format!("0.{}1", "0".repeat(37)).as_bytes()).is_ok()); // scale 38
    assert_eq!(
        Number::parse(format!("0.{}1", "0".repeat(38)).as_bytes()),
        Err(NumError::TooPrecise)
    );
    // huge exponents are errors, not hangs or panics
    for e in [
        "1e99999999999999999999",
        "1e-99999999999999999999",
        "5e40",
        "1e38",
        "1e-39",
    ] {
        assert_eq!(
            Number::parse(e.as_bytes()),
            Err(NumError::TooPrecise),
            "{e}"
        );
    }
    assert_eq!(
        Number::parse(b"1e37").unwrap().format(),
        format!("1{}", "0".repeat(37))
    );
    // zero is zero however it is written
    assert_eq!(Number::parse(b"0e99999999").unwrap(), Number::Int(0));
}

#[test]
fn from_parts_enforces_the_caps() {
    assert!(Number::from_parts(1, 38).is_ok());
    assert_eq!(Number::from_parts(1, 39), Err(NumError::TooPrecise));
    assert_eq!(
        Number::from_parts(10i128.pow(38), 0),
        Err(NumError::TooPrecise)
    );
    assert_eq!(
        Number::from_parts(-(10i128.pow(38)), 0),
        Err(NumError::TooPrecise)
    );
    assert_eq!(
        Number::from_parts(10i128.pow(38) - 1, 0).unwrap().format(),
        "9".repeat(38)
    );
    // a trailing zero is dropped before the cap is checked
    assert_eq!(
        Number::from_parts(10, 39),
        Ok(Number::from_parts(1, 38).unwrap())
    );
}

// ---- JSON ----

#[test]
fn json_values_and_order() {
    let v = ok(r#" { "b": [1, 2.5, "x", null, true], "a": {"k": false} } "#);
    assert_eq!(text(&v), r#"{"b":[1,2.5,"x",null,true],"a":{"k":false}}"#); // insertion order kept
    assert_eq!(ok("[]"), Value::List(vec![]));
    assert_eq!(ok("{}"), Value::Map(Map::new()));
    // key order does not matter to equality
    assert_eq!(ok(r#"{"a":1,"b":2}"#), ok(r#"{"b":2,"a":1}"#));
    assert_ne!(ok(r#"{"a":1,"b":2}"#), ok(r#"{"a":1,"b":3}"#));
    // a string is not a one-element list, a number is not a string
    assert_ne!(ok(r#""a""#), ok(r#"["a"]"#));
    assert_ne!(ok("1"), ok(r#""1""#));
}

#[test]
fn json_strings_escape_and_unescape() {
    assert_eq!(ok(r#""é😀\n\/\b\f""#), Value::str("é😀\n/\u{8}\u{c}"));
    assert_eq!(
        text(&Value::str("a\"b\\c\n\t\r\u{8}\u{c}\u{1}\u{1f}é😀\u{7f}")),
        "\"a\\\"b\\\\c\\n\\t\\r\\b\\f\\u0001\\u001fé😀\u{7f}\""
    );
    // NUL survives
    assert_eq!(ok(r#""\u0000""#), Value::str("\0"));
    assert_eq!(text(&Value::str("\0")), r#""\u0000""#);
}

#[test]
fn json_errors_carry_kind_and_position() {
    let e = j("[1,\n 2,\n x]").unwrap_err();
    assert_eq!(
        (e.kind, e.line, e.col, e.offset),
        (ErrorKind::Unexpected, 3, 2, 9)
    );
    assert_eq!(j("").unwrap_err().kind, ErrorKind::UnexpectedEnd);
    assert_eq!(j("[1,").unwrap_err().kind, ErrorKind::UnexpectedEnd);
    assert_eq!(j("1 2").unwrap_err().kind, ErrorKind::TrailingData);
    assert_eq!(j("\"a\nb\"").unwrap_err().kind, ErrorKind::ControlInString);
    assert_eq!(j(r#""\x""#).unwrap_err().kind, ErrorKind::BadEscape);
    assert_eq!(j(r#""\ud800""#).unwrap_err().kind, ErrorKind::LoneSurrogate);
    assert_eq!(
        j(r#"{"a":1,"a":2}"#).unwrap_err().kind,
        ErrorKind::DuplicateKey
    );
    assert_eq!(j("[01]").unwrap_err().kind, ErrorKind::BadNumber);
    assert_eq!(
        j("1e999999999999999999999999").unwrap_err().kind,
        ErrorKind::NumberTooPrecise
    );
    assert_eq!(
        from_json(b"\"\xff\"", &Limits::DEFAULT).unwrap_err().kind,
        ErrorKind::InvalidUtf8
    );
    assert!(
        j("[1,]").is_err()
            && j("{\"a\":1,}").is_err()
            && j("// c\n1").is_err()
            && j("'a'").is_err()
    );
    assert!(j("NaN").is_err() && j("Infinity").is_err() && j("tru").is_err() && j("nul").is_err());
    for ws in ["\u{b}1", "\u{c}1", "1\u{b}", "\u{85}1"] {
        assert!(j(ws).is_err(), "{ws:?}");
    }
    assert_eq!(
        j("\u{a0}1").unwrap_err().kind,
        ErrorKind::Unexpected,
        "a no-break space is not whitespace"
    );
    // display is the shell's message
    assert_eq!(
        j("[x]").unwrap_err().to_string(),
        "unexpected character at line 1 column 2"
    );
}

#[test]
fn json_limits_are_errors_not_panics() {
    let deep = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
    assert!(j(&deep(128)).is_ok());
    assert_eq!(j(&deep(129)).unwrap_err().kind, ErrorKind::TooDeep);
    // a hostile depth does not overflow the stack
    assert_eq!(j(&deep(1_000_000)).unwrap_err().kind, ErrorKind::TooDeep);
    let small = Limits {
        max_depth: 2,
        max_bytes: 100,
        max_nodes: 100,
    };
    assert!(from_json(b"[[1]]", &small).is_ok());
    assert_eq!(
        from_json(b"[[[1]]]", &small).unwrap_err().kind,
        ErrorKind::TooDeep
    );
    assert_eq!(
        from_json(
            b"[1,2,3,4]",
            &Limits {
                max_nodes: 3,
                ..small
            }
        )
        .unwrap_err()
        .kind,
        ErrorKind::TooManyValues
    );
    assert_eq!(
        from_json(
            b"[1,2,3,4]",
            &Limits {
                max_bytes: 5,
                ..small
            }
        )
        .unwrap_err()
        .kind,
        ErrorKind::TooLarge
    );
    // emitting enforces the depth too
    let mut v = Value::int(1);
    for _ in 0..200 {
        v = Value::List(vec![v]);
    }
    assert_eq!(to_json(&v, &Limits::DEFAULT), Err(ErrorKind::TooDeep));
}

#[test]
fn json_pretty_printing() {
    let v = ok(r#"{"a":[1,{"b":null}],"c":[],"d":{}}"#);
    let p = to_json_pretty(&v, 2, &Limits::DEFAULT).unwrap();
    assert_eq!(p, "{\n  \"a\": [\n    1,\n    {\n      \"b\": null\n    }\n  ],\n  \"c\": [],\n  \"d\": {}\n}");
    assert_eq!(ok(&p), v);
}

// ---- CSV and TSV ----

fn csv(s: &str, header: bool) -> Result<Value, TableError> {
    from_csv(
        s.as_bytes(),
        &TableOptions {
            header,
            ..Default::default()
        },
    )
}

fn rows(v: &Value) -> Vec<Vec<String>> {
    let Value::List(rs) = v else { panic!("{v:?}") };
    rs.iter()
        .map(|r| {
            let Value::List(cs) = r else { panic!() };
            cs.iter()
                .map(|c| {
                    if let Value::Str(s) = c {
                        s.clone()
                    } else {
                        panic!()
                    }
                })
                .collect()
        })
        .collect()
}

#[test]
fn csv_rfc4180_vectors() {
    assert_eq!(
        rows(&csv("a,b,c\n1,2,3\n", false).unwrap()),
        [["a", "b", "c"], ["1", "2", "3"]]
    );
    assert_eq!(
        rows(&csv("a,b\r\n1,2", false).unwrap()),
        [["a", "b"], ["1", "2"]]
    ); // CRLF, no final terminator
    assert_eq!(
        rows(&csv("\"a,b\",\"c\"\"d\",\"e\nf\"\n", false).unwrap()),
        [["a,b", "c\"d", "e\nf"]]
    );
    assert_eq!(rows(&csv("a,,\n", false).unwrap()), [["a", "", ""]]); // empty fields
    assert_eq!(rows(&csv("\"\",x\n", false).unwrap()), [["", "x"]]);
    assert_eq!(rows(&csv("\"a\r\nb\"\r\n", false).unwrap()), [["a\r\nb"]]);
    assert_eq!(csv("", false).unwrap(), Value::List(vec![]));
    // a blank line is a record of one empty field
    assert_eq!(rows(&csv("\n", false).unwrap()), [[""]]);
    assert_eq!(
        csv("a,b\n\n", false).unwrap_err().kind,
        TableErrorKind::Ragged
    );
    // spaces are data
    assert_eq!(rows(&csv(" a , b \n", false).unwrap()), [[" a ", " b "]]);
    // semicolons with a delimiter option
    let o = TableOptions {
        delimiter: b';',
        ..Default::default()
    };
    assert_eq!(rows(&from_csv(b"a,b;c\n", &o).unwrap()), [["a,b", "c"]]);
}

#[test]
fn csv_errors() {
    for (s, k) in [
        ("a,\"b\n", TableErrorKind::UnterminatedQuote),
        ("a\"b,c\n", TableErrorKind::QuoteInField),
        ("\"a\"b,c\n", TableErrorKind::TextAfterQuote),
        ("a\rb\n", TableErrorKind::BareCr),
        ("a,b\n1\n", TableErrorKind::Ragged),
        ("a\n1,2\n", TableErrorKind::Ragged),
    ] {
        assert_eq!(csv(s, false).unwrap_err().kind, k, "{s:?}");
    }
    assert_eq!(
        from_csv(b"a,\xff\n", &TableOptions::default())
            .unwrap_err()
            .kind,
        TableErrorKind::InvalidUtf8
    );
    let e = csv("a,b\nc,\"d\n", false).unwrap_err();
    assert_eq!(
        (e.kind, e.line, e.col),
        (TableErrorKind::UnterminatedQuote, 2, 3)
    );
    let small = TableOptions {
        limits: Limits {
            max_nodes: 3,
            ..Limits::DEFAULT
        },
        ..Default::default()
    };
    assert_eq!(
        from_csv(b"a,b\nc,d\n", &small).unwrap_err().kind,
        TableErrorKind::TooManyValues
    );
    let tiny = TableOptions {
        limits: Limits {
            max_bytes: 3,
            ..Limits::DEFAULT
        },
        ..Default::default()
    };
    assert_eq!(
        from_csv(b"a,b\n", &tiny).unwrap_err().kind,
        TableErrorKind::TooLarge
    );
}

#[test]
fn csv_header_gives_maps() {
    let v = csv("name,age\nann,30\nbo,\n", true).unwrap();
    assert_eq!(
        text(&v),
        r#"[{"name":"ann","age":"30"},{"name":"bo","age":""}]"#
    );
    assert_eq!(
        csv("a,a\n1,2\n", true).unwrap_err().kind,
        TableErrorKind::DuplicateHeader
    );
    assert_eq!(
        csv("a,a\n", true).unwrap_err().kind,
        TableErrorKind::DuplicateHeader
    );
    assert_eq!(csv("a,b\n", true).unwrap(), Value::List(vec![])); // a header alone has no rows
    let o = TableOptions {
        header: true,
        ..Default::default()
    };
    assert_eq!(to_csv(&v, &o).unwrap(), "name,age\nann,30\nbo,\n");
}

#[test]
fn csv_emitting_quotes_only_what_needs_it() {
    let t = Value::List(vec![Value::List(vec![
        Value::str("plain"),
        Value::str("a,b"),
        Value::str("q\"q"),
        Value::str("l\nl"),
        Value::str(" sp "),
        Value::int(7),
        Value::Bool(true),
        Value::Num(Number::parse(b"1.50").unwrap()),
    ])]);
    assert_eq!(
        to_csv(&t, &TableOptions::default()).unwrap(),
        "plain,\"a,b\",\"q\"\"q\",\"l\nl\", sp ,7,true,1.5\n"
    );
    let crlf = TableOptions {
        crlf: true,
        ..Default::default()
    };
    assert!(to_csv(&t, &crlf).unwrap().ends_with("\r\n"));
    // a one-column table with an empty cell must not become a blank line
    let one = Value::List(vec![
        Value::List(vec![Value::str("")]),
        Value::List(vec![Value::str("x")]),
    ]);
    let out = to_csv(&one, &TableOptions::default()).unwrap();
    assert_eq!(out, "\"\"\nx\n");
    assert_eq!(rows(&csv(&out, false).unwrap()), [[""], ["x"]]);
    // not tables
    assert_eq!(
        to_csv(&Value::int(1), &TableOptions::default()),
        Err(TableErrorKind::NotATable)
    );
    let nul = Value::List(vec![Value::List(vec![Value::Null])]);
    assert_eq!(
        to_csv(&nul, &TableOptions::default()),
        Err(TableErrorKind::BadCell)
    );
    let ragged = Value::List(vec![Value::List(vec![Value::int(1)]), Value::List(vec![])]);
    assert_eq!(
        to_csv(&ragged, &TableOptions::default()),
        Err(TableErrorKind::NotATable)
    );
}

#[test]
fn tsv_vectors_and_errors() {
    let t = |s: &str| from_tsv(s.as_bytes(), &TableOptions::default());
    assert_eq!(rows(&t("a\tb\n1\t2\n").unwrap()), [["a", "b"], ["1", "2"]]);
    assert_eq!(
        rows(&t("a\\tb\\n\\\\\\r\tx\r\n").unwrap()),
        [["a\tb\n\\\r", "x"]]
    );
    assert_eq!(rows(&t("\t\n").unwrap()), [["", ""]]);
    assert_eq!(t("").unwrap(), Value::List(vec![]));
    assert_eq!(
        rows(&t("\"quotes\" are data\n").unwrap()),
        [["\"quotes\" are data"]]
    );
    assert_eq!(t("a\\x\n").unwrap_err().kind, TableErrorKind::BadEscape);
    assert_eq!(t("a\\").unwrap_err().kind, TableErrorKind::BadEscape);
    assert_eq!(t("a\rb\n").unwrap_err().kind, TableErrorKind::BareCr);
    assert_eq!(t("a\tb\nc\n").unwrap_err().kind, TableErrorKind::Ragged);
    let v = Value::List(vec![Value::List(vec![
        Value::str("t\tn\nr\rb\\"),
        Value::str(""),
    ])]);
    let out = to_tsv(&v, &TableOptions::default()).unwrap();
    assert_eq!(out, "t\\tn\\nr\\rb\\\\\t\n");
    assert_eq!(t(&out).unwrap(), v);
}

// ---- access primitives ----

#[test]
fn access_primitives() {
    let v = ok(r#"{"a":{"b":[10,20,{"c":"x"}]},"n":null,"0":"zero"}"#);
    let path = |p: &[&str]| -> Vec<Key> {
        p.iter()
            .map(|s| s.parse::<usize>().map_or_else(|_| Key::name(s), Key::Index))
            .collect()
    };
    assert_eq!(
        get(&v, &path(&["a", "b", "2", "c"])),
        Some(&Value::str("x"))
    );
    assert_eq!(require(&v, &path(&["a", "q"])), Err(PathError::Missing(1)));
    assert_eq!(
        require(&v, &path(&["a", "b", "9"])),
        Err(PathError::Missing(2))
    );
    assert_eq!(
        require(&v, &path(&["n", "x"])),
        Err(PathError::NotContainer(1))
    );
    assert_eq!(
        require(&v, &[Key::name("a"), Key::name("b"), Key::name("q")]),
        Err(PathError::NotContainer(2))
    );
    // a missing key and a null are never confused
    assert_eq!(get_or(&v, &path(&["n"]), Value::int(5)), Ok(Value::Null));
    assert_eq!(get_or(&v, &path(&["zz"]), Value::int(5)), Ok(Value::int(5)));
    assert_eq!(
        get_or(&v, &path(&["n", "x"]), Value::int(5)),
        Err(PathError::NotContainer(1))
    );
    assert!(has(&v, &path(&["n"])) && !has(&v, &path(&["m"])));
    // an index on a map is the key spelled as that number
    assert_eq!(get(&v, &[Key::Index(0)]), Some(&Value::str("zero")));
    // updates are functional
    let w = set_in(&v, &path(&["a", "b", "0"]), Value::int(11)).unwrap();
    assert_eq!(get(&w, &path(&["a", "b", "0"])), Some(&Value::int(11)));
    assert_eq!(
        get(&v, &path(&["a", "b", "0"])),
        Some(&Value::int(10)),
        "the original is unchanged"
    );
    let w = set_in(&v, &path(&["a", "new"]), Value::Bool(true)).unwrap();
    assert_eq!(keys(get(&w, &path(&["a"])).unwrap()).unwrap(), ["b", "new"]); // appended
    let w = set_in(&v, &path(&["a", "b"]), Value::Null).unwrap();
    assert_eq!(keys(get(&w, &path(&["a"])).unwrap()).unwrap(), ["b"]);
    let w = set_in(&v, &path(&["n"]), Value::int(1)).unwrap();
    assert_eq!(
        keys(&w).unwrap(),
        ["a", "n", "0"],
        "replacing keeps the position"
    );
    assert_eq!(
        set_in(&v, &path(&["x", "y"]), Value::Null),
        Err(PathError::Missing(0))
    );
    assert_eq!(
        set_in(&v, &path(&["a", "b", "7"]), Value::Null),
        Err(PathError::Missing(2))
    );
    assert_eq!(set_in(&v, &[], Value::int(1)), Ok(Value::int(1)));
    let d = del_in(&v, &path(&["a", "b", "1"])).unwrap();
    assert_eq!(
        text(get(&d, &path(&["a", "b"])).unwrap()),
        r#"[10,{"c":"x"}]"#
    );
    let d = del_in(&v, &path(&["n"])).unwrap();
    assert_eq!(keys(&d).unwrap(), ["a", "0"]);
    assert_eq!(del_in(&v, &path(&["nope"])), Err(PathError::Missing(0)));
    assert_eq!(del_in(&v, &[]), Err(PathError::Missing(0)));
    // keys, values, len
    assert_eq!(keys(&Value::int(1)), None);
    assert_eq!(
        values(&ok("[1,2]")),
        Some(vec![Value::int(1), Value::int(2)])
    );
    assert_eq!(values(&ok(r#"{"a":1}"#)), Some(vec![Value::int(1)]));
    assert_eq!(
        (
            len(&ok("[1,2]")),
            len(&ok(r#"{"a":1}"#)),
            len(&Value::str("é😀")),
            len(&Value::int(1))
        ),
        (Some(2), Some(1), Some(2), None)
    );
}

#[test]
fn map_basics() {
    let mut m = Map::new();
    assert!(m.insert("a".into(), Value::int(1)).is_ok());
    assert_eq!(m.insert("a".into(), Value::int(2)), Err(DuplicateKey));
    assert_eq!(m.get("a"), Some(&Value::int(1)));
    assert_eq!(m.without("a").len(), 0);
    assert_eq!(m.without("zz").len(), 1);
    assert_eq!(Value::Map(m).depth(), 1);
    assert_eq!(ok("[[],[[]]]").depth(), 3);
}

// ---- properties ----

fn gen_str(r: &mut Rng) -> String {
    const PIECES: &[&str] = &[
        "a", "Z", "0", " ", ",", "\"", "\\", "\t", "\n", "\r", "é", "😀", "\u{7f}", "\u{1}", "x y",
        "{", "[", "/", "\u{2028}",
    ];
    (0..r.below(6))
        .map(|_| PIECES[r.below(PIECES.len() as u64) as usize])
        .collect()
}

fn gen_num(r: &mut Rng) -> Number {
    match r.below(4) {
        0 => Number::Int(r.next() as i64),
        1 => Number::Int(r.below(2000) as i64 - 1000),
        2 => Number::from_parts((r.next() as i64 as i128) * 1_000_003, r.below(20) as u32).unwrap(),
        _ => Number::from_parts(i128::from(r.next() as i64), r.below(39) as u32).unwrap(),
    }
}

fn gen_value(r: &mut Rng, depth: usize) -> Value {
    match r.below(if depth >= 4 { 4 } else { 6 }) {
        0 => Value::Null,
        1 => Value::Bool(r.below(2) == 1),
        2 => Value::Num(gen_num(r)),
        3 => Value::Str(gen_str(r)),
        4 => Value::List((0..r.below(4)).map(|_| gen_value(r, depth + 1)).collect()),
        _ => {
            let mut m = Map::new();
            for _ in 0..r.below(4) {
                let _ = m.insert(gen_str(r), gen_value(r, depth + 1));
            }
            Value::Map(m)
        }
    }
}

#[test]
fn json_round_trips_any_value() {
    let mut r = Rng(0x9e3779b97f4a7c15);
    for _ in 0..3000 {
        let v = gen_value(&mut r, 0);
        let t = text(&v);
        assert_eq!(from_json(t.as_bytes(), &Limits::DEFAULT).unwrap(), v, "{t}");
        let p = to_json_pretty(&v, 3, &Limits::DEFAULT).unwrap();
        assert_eq!(from_json(p.as_bytes(), &Limits::DEFAULT).unwrap(), v);
    }
}

#[test]
fn numbers_round_trip_through_text() {
    let mut r = Rng(12345);
    for _ in 0..5000 {
        let n = gen_num(&mut r);
        assert_eq!(Number::parse(n.format().as_bytes()), Ok(n), "{n}");
    }
}

fn gen_table(r: &mut Rng) -> Value {
    let cols = 1 + r.below(4) as usize;
    let nrows = r.below(5) as usize;
    Value::List(
        (0..nrows)
            .map(|_| Value::List((0..cols).map(|_| Value::Str(gen_str(r))).collect()))
            .collect(),
    )
}

#[test]
fn csv_and_tsv_round_trip_any_table_of_strings() {
    let mut r = Rng(777);
    for _ in 0..3000 {
        let t = gen_table(&mut r);
        for crlf in [false, true] {
            let o = TableOptions {
                crlf,
                ..Default::default()
            };
            let c = to_csv(&t, &o).unwrap();
            assert_eq!(from_csv(c.as_bytes(), &o).unwrap(), t, "csv {c:?}");
            let s = to_tsv(&t, &o).unwrap();
            assert_eq!(from_tsv(s.as_bytes(), &o).unwrap(), t, "tsv {s:?}");
        }
        // with a header: maps (distinct generated column names)
        if let Value::List(rs) = &t {
            if let Some(Value::List(first)) = rs.first() {
                let names: Vec<String> = (0..first.len())
                    .map(|i| format!("c{i}{}", gen_str(&mut r)))
                    .collect();
                let maps: Vec<Value> = rs
                    .iter()
                    .map(|row| {
                        let Value::List(cs) = row else { unreachable!() };
                        let mut m = Map::new();
                        for (n, c) in names.iter().zip(cs) {
                            m.insert(n.clone(), c.clone()).unwrap();
                        }
                        Value::Map(m)
                    })
                    .collect();
                let hv = Value::List(maps);
                let o = TableOptions {
                    header: true,
                    ..Default::default()
                };
                let c = to_csv(&hv, &o).unwrap();
                assert_eq!(from_csv(c.as_bytes(), &o).unwrap(), hv, "csv header {c:?}");
                let s = to_tsv(&hv, &o).unwrap();
                assert_eq!(from_tsv(s.as_bytes(), &o).unwrap(), hv, "tsv header {s:?}");
            }
        }
    }
}

#[test]
fn no_parser_panics_on_arbitrary_bytes() {
    let mut r = Rng(31337);
    let alphabet: &[u8] = b"[]{}\",:\\u0123456789eE.+-tfnrl \t\r\n\xff\xc3\xa9\x1b/";
    for _ in 0..20_000 {
        let n = r.below(40) as usize;
        let input: Vec<u8> = (0..n)
            .map(|_| alphabet[r.below(alphabet.len() as u64) as usize])
            .collect();
        let _ = from_json(&input, &Limits::DEFAULT);
        let _ = from_csv(&input, &TableOptions::default());
        let _ = from_csv(
            &input,
            &TableOptions {
                header: true,
                ..Default::default()
            },
        );
        let _ = from_tsv(&input, &TableOptions::default());
        let _ = Number::parse(&input);
    }
}

#[test]
fn a_prefix_of_a_document_is_never_the_whole_value() {
    let doc = r#"{"a":[1,2.5e1,"xé😀",{"b":null}],"c":true}"#;
    let whole = ok(doc);
    for n in 0..doc.len() {
        if let Ok(v) = from_json(&doc.as_bytes()[..n], &Limits::DEFAULT) {
            assert_ne!(v, whole, "{n}");
        }
    }
}
