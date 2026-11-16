use mounts::*;

const U1: &str = "3f2a9c1e-0000-4000-8000-000000000001";
const U2: &str = "00112233445566778899aabbccddeeff";

fn text(lines: &[&str]) -> Vec<u8> {
    lines.join("\n").into_bytes()
}

#[test]
fn a_table_parses() {
    let t = parse(&text(&[
        "# the volumes this machine knows",
        "",
        &format!("{U1}  data"),
        &format!("{U2}\tscratch  ro  sub=tmp/work   # a comment"),
    ]))
    .unwrap();
    assert_eq!(t.len(), 2);
    assert_eq!(t[0].name, "data");
    assert_eq!(t[0].uuid[0], 0x3f);
    assert_eq!(t[0].uuid[15], 1);
    assert!(!t[0].readonly && t[0].sub.is_none());
    assert_eq!(t[1].name, "scratch");
    assert!(t[1].readonly);
    assert_eq!(t[1].sub.as_deref(), Some("tmp/work"));
    assert_eq!(t[1].uuid, parse_uuid(U2).unwrap());
    assert_eq!(parse(b"").unwrap(), vec![]);
    assert_eq!(parse(b"# only a comment\n\n   \n").unwrap(), vec![]);
}

#[test]
fn every_kind_of_bad_line_is_refused_with_its_line_number() {
    let cases: &[(&[&str], usize, ErrorKind)] = &[
        (&["# ok", "onlyonefield"], 2, ErrorKind::MissingField),
        (&["zzzz data"], 1, ErrorKind::BadUuid),
        (
            &["3f2a9c1e00004000800000000000000 data"],
            1,
            ErrorKind::BadUuid,
        ),
        (
            &["3f2a9c1e000040008000000000000001ff data"],
            1,
            ErrorKind::BadUuid,
        ),
        (&[&format!("{U1} ")], 1, ErrorKind::MissingField),
        (&[&format!("{U1} .hidden")], 1, ErrorKind::BadName),
        (&[&format!("{U1} a/b")], 1, ErrorKind::BadName),
        (
            &[&format!("{U1} {}", "x".repeat(33))],
            1,
            ErrorKind::BadName,
        ),
        (&[&format!("{U1} data rw")], 1, ErrorKind::BadOption),
        (&[&format!("{U1} data sub=../x")], 1, ErrorKind::BadOption),
        (&[&format!("{U1} data sub=/abs")], 1, ErrorKind::BadOption),
        (&[&format!("{U1} data sub=a//b")], 1, ErrorKind::BadOption),
        (
            &[&format!("{U1} data sub=a sub=b")],
            1,
            ErrorKind::BadOption,
        ),
        (
            &[&format!("{U1} data"), &format!("{U1} other")],
            2,
            ErrorKind::Duplicate,
        ),
        (
            &[&format!("{U1} data"), &format!("{U2} data")],
            2,
            ErrorKind::Duplicate,
        ),
    ];
    for (lines, line, kind) in cases {
        assert_eq!(
            parse(&text(lines)),
            Err(ParseError {
                line: *line,
                kind: *kind
            }),
            "{lines:?}"
        );
    }
    assert_eq!(
        parse(&[0xff, 0xfe]),
        Err(ParseError {
            line: 0,
            kind: ErrorKind::NotText
        })
    );
}

#[test]
fn uuids_round_trip() {
    let u = parse_uuid(U1).unwrap();
    assert_eq!(uuid_hex(&u), "3f2a9c1e000040008000000000000001");
    assert_eq!(parse_uuid(&uuid_hex(&u)), Some(u));
    assert_eq!(parse_uuid("3f2a9c1e-0000-4000-8000-00000000000"), None);
    assert_eq!(parse_uuid(""), None);
}

fn found(device: u16, uuid: &str, label: &str) -> Found {
    Found {
        device,
        uuid: parse_uuid(uuid).unwrap(),
        label: label.into(),
    }
}

#[test]
fn names_come_from_the_table_never_from_the_disk() {
    let table = parse(&text(&[&format!("{U1} data ro")])).unwrap();
    // the mapped volume gets its name; one that calls itself `data` but is not mapped gets none
    let plan = plan(&table, &[found(0, U2, "data"), found(1, U1, "whatever")]);
    assert_eq!(
        plan[0].1,
        Decision::Publish {
            by_uuid: uuid_hex(&parse_uuid(U2).unwrap()),
            name: None,
            readonly: false,
            sub: None
        }
    );
    assert_eq!(
        plan[1].1,
        Decision::Publish {
            by_uuid: uuid_hex(&parse_uuid(U1).unwrap()),
            name: Some("data".into()),
            readonly: true,
            sub: None
        }
    );
}

#[test]
fn a_cloned_disk_is_refused_and_the_first_keeps_its_names() {
    let table = parse(&text(&[&format!("{U1} data")])).unwrap();
    // attached in either order: whichever comes first wins, the other is refused
    let a = plan(&table, &[found(0, U1, "x"), found(1, U1, "x")]);
    assert!(matches!(a[0].1, Decision::Publish { name: Some(_), .. }));
    assert_eq!(a[1].1, Decision::RefuseDuplicate);
    let b = plan(&table, &[found(1, U1, "x"), found(0, U1, "x")]);
    assert_eq!(b[0].0.device, 1);
    assert!(matches!(b[0].1, Decision::Publish { name: Some(_), .. }));
    assert_eq!(b[1].1, Decision::RefuseDuplicate);
}

/// Whatever the bytes, the parser answers with a table or an error: never a panic, and a table it accepts is consistent.
#[test]
fn the_parser_is_total_over_random_and_mutated_text() {
    let mut x = 0x1234_5678_9abc_def1u64;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let base = text(&[
        &format!("{U1} data ro sub=a/b"),
        &format!("{U2} scratch"),
        "# c",
    ]);
    for _ in 0..20_000 {
        let mut t = base.clone();
        for _ in 0..(next() % 6) {
            let i = (next() % t.len() as u64) as usize;
            match next() % 3 {
                0 => t[i] = next() as u8,
                1 => t[i] = b" \t\n#-=/."[(next() % 7) as usize],
                _ => {
                    t.remove(i);
                }
            }
            if t.is_empty() {
                t = base.clone();
            }
        }
        if let Ok(table) = parse(&t) {
            for (i, e) in table.iter().enumerate() {
                assert!(valid_name(&e.name));
                assert!(table
                    .iter()
                    .skip(i + 1)
                    .all(|o| o.uuid != e.uuid && o.name != e.name));
            }
        }
    }
    for _ in 0..5_000 {
        let n = (next() % 80) as usize;
        let t: Vec<u8> = (0..n).map(|_| next() as u8).collect();
        let _ = parse(&t);
    }
}
