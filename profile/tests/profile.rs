use profile::*;

const ALICE: &str = r#"{ "name": "alice", "uid": 1000, "gid": 100, "home": "homes/alice", "shell": "sh",
  "env": { "EDITOR": "kilo" },
  "programs": { "ls": { "root": true, "mounts": [ { "path": "/mnt/store", "volume": "store", "rights": "rw" } ] },
                "cat": { "mounts": [ { "volume": "store" } ] } },
  "quota": { "memory_mib": 32, "processes": 8 }, "autologin": true }"#;

#[test]
fn a_full_profile_parses() {
    let p = Profile::parse(ALICE).unwrap();
    assert_eq!(
        (
            p.name.as_str(),
            p.uid,
            p.gid,
            p.home.as_str(),
            p.shell.as_str()
        ),
        ("alice", 1000, 100, "homes/alice", "sh")
    );
    assert_eq!(p.env, [("EDITOR".to_string(), "kilo".to_string())]);
    assert_eq!(
        p.quota,
        Some(Quota {
            memory_mib: 32,
            processes: 8
        })
    );
    assert!(p.autologin);
    let ls = p.grant("ls").unwrap();
    assert!(ls.root);
    assert_eq!(
        ls.mounts,
        [Mount {
            path: "/mnt/store".into(),
            volume: "store".into(),
            rights: Rights::ReadWrite
        }]
    );
    // defaults: the path is /mnt/<volume>, rights are read-only, no root
    let cat = p.grant("cat").unwrap();
    assert!(!cat.root);
    assert_eq!(
        cat.mounts,
        [Mount {
            path: "/mnt/store".into(),
            volume: "store".into(),
            rights: Rights::Read
        }]
    );
    assert!(p.grant("echo").is_none());
}

#[test]
fn defaults_and_the_identity_the_libc_synthesizes() {
    let p = Profile::parse(r#"{ "name": "bob", "uid": 1001, "home": "homes/bob" }"#).unwrap();
    assert_eq!(
        (p.gid, p.shell.as_str(), p.autologin, p.quota.clone()),
        (1001, "sh", false, None)
    );
    assert_eq!(
        p.session_env(),
        [
            ("USER", "bob"),
            ("HOME", "/home"),
            ("UID", "1001"),
            ("GID", "1001")
        ]
        .map(|(a, b)| (a.to_string(), b.to_string()))
    );
    assert_eq!(p.passwd_line(), "bob:x:1001:1001:bob:/home:/bin/sh\n");
}

#[test]
fn bad_profiles_are_refused_with_a_reason() {
    let cases: &[(&str, &str)] = &[
        ("{", "not valid JSON"),
        ("[]", "not an object"),
        (r#"{"uid":1,"home":"h"}"#, "lacks `name`"),
        (r#"{"name":"a","home":"h"}"#, "lacks `uid`"),
        (r#"{"name":"a","uid":1}"#, "lacks `home`"),
        (r#"{"name":"Alice","uid":1,"home":"h"}"#, "`name`"),
        (r#"{"name":"a b","uid":1,"home":"h"}"#, "`name`"),
        (r#"{"name":"a","uid":-1,"home":"h"}"#, "`uid`"),
        (r#"{"name":"a","uid":1.5,"home":"h"}"#, "`uid`"),
        (r#"{"name":"a","uid":99999999999,"home":"h"}"#, "`uid`"),
        (r#"{"name":"a","uid":1,"home":"/abs"}"#, "`home`"),
        (r#"{"name":"a","uid":1,"home":"a/../b"}"#, "`home`"),
        (r#"{"name":"a","uid":1,"home":"a//b"}"#, "`home`"),
        (
            r#"{"name":"a","uid":1,"home":"h","shel":"sh"}"#,
            "unknown profile key `shel`",
        ),
        (
            r#"{"name":"a","uid":1,"home":"h","shell":"../sh"}"#,
            "`shell`",
        ),
        (
            r#"{"name":"a","uid":1,"home":"h","env":{"lower":"x"}}"#,
            "`env`",
        ),
        (
            r#"{"name":"a","uid":1,"home":"h","env":{"HOME":"x"}}"#,
            "`env`",
        ),
        (
            r#"{"name":"a","uid":1,"home":"h","programs":{"ls":{"mounts":[{"volume":"v","path":"/"}]}}}"#,
            "`path`",
        ),
        (
            r#"{"name":"a","uid":1,"home":"h","programs":{"ls":{"mounts":[{"volume":"v","path":"/tmp"}]}}}"#,
            "`path`",
        ),
        (
            r#"{"name":"a","uid":1,"home":"h","programs":{"ls":{"mounts":[{"volume":"v","path":"/proc/x"}]}}}"#,
            "`path`",
        ),
        (
            r#"{"name":"a","uid":1,"home":"h","programs":{"ls":{"mounts":[{"volume":"v","path":"rel"}]}}}"#,
            "`path`",
        ),
        (
            r#"{"name":"a","uid":1,"home":"h","programs":{"ls":{"mounts":[{"volume":"v","rights":"x"}]}}}"#,
            "`rights`",
        ),
        (
            r#"{"name":"a","uid":1,"home":"h","programs":{"ls":{"mounts":[{"path":"/m"}]}}}"#,
            "lacks `volume`",
        ),
        (
            r#"{"name":"a","uid":1,"home":"h","programs":{"ls":{"rooot":true}}}"#,
            "unknown profile key `rooot`",
        ),
        (
            r#"{"name":"a","uid":1,"home":"h","programs":{"ls":{"mounts":[{"volume":"v","extra":1}]}}}"#,
            "unknown profile key `extra`",
        ),
        (
            r#"{"name":"a","uid":1,"home":"h","quota":{"memory_mib":0}}"#,
            "`memory_mib`",
        ),
        (
            r#"{"name":"a","uid":1,"home":"h","quota":{"cpu":1}}"#,
            "unknown profile key `cpu`",
        ),
        (
            r#"{"name":"a","uid":1,"home":"h","autologin":"yes"}"#,
            "`autologin`",
        ),
        (
            r#"{"name":"a","uid":1,"home":"h","name":"b"}"#,
            "not valid JSON",
        ), // a duplicate key
    ];
    for (src, want) in cases {
        let e = Profile::parse(src).expect_err(src);
        assert!(e.to_string().contains(want), "{src}: {e}");
    }
    let big = format!(
        r#"{{"name":"a","uid":1,"home":"h","env":{{"X":"{}"}}}}"#,
        "x".repeat(MAX_PROFILE_BYTES)
    );
    assert_eq!(Profile::parse(&big), Err(ProfileError::TooLarge));
    // a half-given quota fills in the other limit with the default
    let q = Profile::parse(r#"{"name":"a","uid":1,"home":"h","quota":{"processes":4}}"#)
        .unwrap()
        .quota
        .unwrap();
    assert_eq!(
        (q.processes, q.memory_mib),
        (4, Quota::default().memory_mib)
    );
}

#[test]
fn exactly_one_autologin_and_distinct_homes() {
    let mk = |name: &str, home: &str, auto: bool| {
        Profile::parse(&format!(
            r#"{{"name":"{name}","uid":1,"home":"{home}","autologin":{auto}}}"#
        ))
        .unwrap()
    };
    let (a, b, c) = (
        mk("a", "homes/a", true),
        mk("b", "homes/b", false),
        mk("c", "homes/c", true),
    );
    assert_eq!(pick_autologin(std::slice::from_ref(&b)).unwrap(), None);
    assert_eq!(
        pick_autologin(&[a.clone(), b.clone()])
            .unwrap()
            .unwrap()
            .name,
        "a"
    );
    assert!(pick_autologin(&[a.clone(), c]).is_err());
    assert!(check_homes_distinct(&[a.clone(), b.clone()]).is_ok());
    assert!(check_homes_distinct(&[a.clone(), mk("d", "homes/a", false)]).is_err());
    assert!(
        check_homes_distinct(&[mk("e", "homes", false), a]).is_err(),
        "one home inside another"
    );
    assert!(check_homes_distinct(&[b]).is_ok());
}

#[test]
fn no_input_panics() {
    let mut seed = 99u64;
    let alphabet: Vec<char> = "{}[]\":,0123456789 truefalsnamidhoeqp/_-.\n"
        .chars()
        .collect();
    for _ in 0..20000 {
        let mut s = String::new();
        for _ in 0..40 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            s.push(alphabet[(seed % alphabet.len() as u64) as usize]);
        }
        let _ = Profile::parse(&s);
    }
}

#[test]
fn spawn_defaults_to_granted_and_can_be_turned_off() {
    let p = |extra: &str| {
        Profile::parse(&format!(
            r#"{{"name":"a","uid":1,"home":"homes/a"{extra}}}"#
        ))
    };
    assert!(
        p("").unwrap().spawn,
        "the grant is on unless the profile says otherwise"
    );
    assert!(p(r#","spawn":true"#).unwrap().spawn);
    assert!(!p(r#","spawn":false"#).unwrap().spawn);
    assert!(p(r#","spawn":"no""#).is_err(), "a string is not a boolean");
    assert!(p(r#","spawn":0"#).is_err(), "nor is a number");
}
