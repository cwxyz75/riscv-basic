use pathres::*;
use std::collections::BTreeMap;

fn c(p: &str) -> Comps {
    p.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.as_bytes().to_vec())
        .collect()
}

#[test]
fn dotdot_is_lexical_and_clamps_at_the_root() {
    assert_eq!(join_abs(&parse_abs(b"/..")), b"/");
    assert_eq!(join_abs(&parse_abs(b"/a/../../b")), b"/b");
    assert_eq!(join_abs(&parse_abs(b"/a/./b//c/")), b"/a/b/c");
    assert_eq!(join_abs(&absolute(&c("/home/sub"), b"../..")), b"/");
    assert_eq!(
        join_abs(&absolute(&c("/home/sub"), b"../../..")),
        b"/",
        "above the root stays the root"
    );
    assert_eq!(
        join_abs(&absolute(&c("/home/sub"), b"/etc")),
        b"/etc",
        "an absolute path ignores the cwd"
    );
    assert_eq!(join_rel(&c("/a/b")), b"a/b");
}

fn table() -> Table {
    Table {
        mounts: vec![(c("/"), 1), (c("/mnt/store"), 2), (c("/svc"), 3)],
        cwd: Some(c("/home")),
        cwd_handle: Some(7),
    }
}

#[test]
fn a_relative_path_below_the_cwd_starts_at_the_cwd_handle() {
    let t = table();
    let p = t.place(b"x/../y").unwrap();
    assert_eq!((p.handle, p.rel, p.base), (7, c("y"), c("/home")));
    let p = t.place(b"a/b").unwrap();
    assert_eq!((p.handle, p.rel), (7, c("a/b")));
    let p = t.place(b".").unwrap();
    assert_eq!((p.handle, p.rel), (7, vec![]));
}

#[test]
fn a_path_that_climbs_is_walked_again_from_the_table() {
    let t = table();
    let p = t.place(b"../etc/trusted").unwrap();
    assert_eq!((p.handle, p.rel, p.base), (1, c("etc/trusted"), vec![]));
    let p = t.place(b"../../..").unwrap();
    assert_eq!((p.handle, p.rel), (1, vec![]), "clamped at the root");
}

#[test]
fn the_longest_mount_wins_and_dotdot_crosses_a_mount_boundary() {
    let t = table();
    let p = t.place(b"/mnt/store/x/y").unwrap();
    assert_eq!((p.handle, p.rel, p.base), (2, c("x/y"), c("/mnt/store")));
    let p = t.place(b"/mnt/store/../hello").unwrap();
    assert_eq!(
        (p.handle, p.rel),
        (1, c("mnt/hello")),
        "lexically /mnt/hello, which is the root's"
    );
    let p = t.place(b"/mnt").unwrap();
    assert_eq!((p.handle, p.rel), (1, c("mnt")));
    let p = t.place(b"/mnt/store").unwrap();
    assert_eq!(
        (p.handle, p.rel),
        (2, vec![]),
        "the mount point itself is the mount's root"
    );
    let p = t.place(b"/mnt/storefront").unwrap();
    assert_eq!(
        p.handle, 1,
        "a prefix is whole components, not a string prefix"
    );
}

#[test]
fn a_confined_process_cannot_climb_out() {
    let t = Table {
        mounts: vec![],
        cwd: Some(c("/home")),
        cwd_handle: Some(7),
    };
    assert!(t.place(b"a/b").is_some());
    assert_eq!(t.place(b"../x"), None, "nothing covers the parent");
    assert_eq!(t.place(b"/etc"), None, "no root entry");
    // a cwd whose path is unknown cannot be climbed from either
    let t = Table {
        mounts: vec![(c("/"), 1)],
        cwd: None,
        cwd_handle: Some(7),
    };
    assert!(t.place(b"a").is_some());
    assert_eq!(t.place(b"../a"), None);
    assert!(t.place(b"/a").is_some());
}

#[derive(Clone)]
enum Node {
    Dir,
    File,
    Link(&'static str),
}

/// A model server: one tree of absolute model paths; a handle is the model path of the directory it is rooted at.
struct Model {
    nodes: BTreeMap<String, Node>,
    roots: BTreeMap<u32, &'static str>,
    follow_last: bool,
}

impl Model {
    fn open(&self, handle: u32, rel: &[u8]) -> Result<String, Stop> {
        let root = self.roots[&handle];
        let comps: Vec<&str> = std::str::from_utf8(rel)
            .unwrap()
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        if comps.contains(&"..") {
            return Err(Stop::Status(-108)); // the server refuses a raw `..`
        }
        let mut at = root.trim_end_matches('/').to_string();
        for (i, comp) in comps.iter().enumerate() {
            at = format!("{at}/{comp}");
            match self.nodes.get(&at) {
                None => return Err(Stop::Status(-100)),
                Some(Node::Link(t)) if i + 1 < comps.len() || self.follow_last => {
                    return Err(Stop::Symlink {
                        consumed: i + 1,
                        target: t.as_bytes().to_vec(),
                    });
                }
                Some(Node::File) if i + 1 < comps.len() => return Err(Stop::Status(-104)),
                _ => {}
            }
        }
        Ok(at)
    }
}

fn model() -> Model {
    let mut nodes = BTreeMap::new();
    for (p, n) in [
        ("/etc", Node::Dir),
        ("/etc/hosts", Node::File),
        ("/a", Node::Dir),
        ("/a/b", Node::Dir),
        ("/a/b/link", Node::Link("../c")),
        ("/a/c", Node::File),
        ("/a/up", Node::Link("../../../etc")),
        ("/abs", Node::Link("/mnt/store/f")),
        ("/d", Node::Link("a/b")),
        ("/x", Node::Link("y")),
        ("/y", Node::Link("x")),
        ("/self", Node::Link("self")),
        ("/vol2", Node::Dir),
        ("/vol2/f", Node::File),
        ("/vol2/inner", Node::Link("../hosts")),
    ] {
        nodes.insert(p.to_string(), n);
    }
    // the second volume's own tree is rooted at /vol2
    Model {
        nodes,
        roots: BTreeMap::from([(1, "/"), (2, "/vol2")]),
        follow_last: true,
    }
}

fn run(m: &Model, t: &Table, path: &str) -> Result<(String, String), Fail> {
    let m2 = m;
    open_following(t, path.as_bytes(), |h, rel| m2.open(h, rel))
        .map(|(s, abs)| (s, String::from_utf8(join_abs(&abs)).unwrap()))
}

fn root_table() -> Table {
    Table {
        mounts: vec![(c("/"), 1), (c("/mnt/store"), 2)],
        cwd: Some(c("/a")),
        cwd_handle: None,
    }
}

#[test]
fn a_relative_link_with_dotdot_resolves_from_the_links_own_directory() {
    let (m, t) = (model(), root_table());
    assert_eq!(run(&m, &t, "/a/b/link").unwrap().0, "/a/c");
    assert_eq!(run(&m, &t, "b/link").unwrap().0, "/a/c", "from the cwd");
}

#[test]
fn a_link_that_climbs_above_the_root_is_clamped() {
    let (m, t) = (model(), root_table());
    assert_eq!(run(&m, &t, "/a/up/hosts").unwrap().0, "/etc/hosts");
}

#[test]
fn an_absolute_target_is_walked_against_the_table_across_a_mount() {
    let (m, t) = (model(), root_table());
    assert_eq!(
        run(&m, &t, "/abs").unwrap(),
        ("/vol2/f".to_string(), "/mnt/store/f".to_string())
    );
}

#[test]
fn a_link_in_the_middle_of_a_path_is_followed_and_the_rest_kept() {
    let (m, t) = (model(), root_table());
    assert_eq!(
        run(&m, &t, "/d/link").unwrap().0,
        "/a/c",
        "/d -> a/b, then link -> ../c"
    );
    // `..` is lexical: /d/../etc is /etc, whatever /d points at
    assert_eq!(run(&m, &t, "/d/../etc/hosts").unwrap().0, "/etc/hosts");
}

#[test]
fn a_link_inside_a_mount_resolves_against_the_mount_prefix() {
    let (m, t) = (model(), root_table());
    // /mnt/store/inner -> ../hosts means /mnt/hosts, which the root volume does not have
    assert_eq!(
        run(&m, &t, "/mnt/store/inner").unwrap_err(),
        Fail::Status(-100)
    );
}

#[test]
fn loops_end_with_eloop_and_the_limit_is_exact() {
    let (m, t) = (model(), root_table());
    assert_eq!(run(&m, &t, "/x").unwrap_err(), Fail::Loop);
    assert_eq!(run(&m, &t, "/self").unwrap_err(), Fail::Loop);
    // a chain of exactly MAX_LINKS links works, one more does not
    for (n, ok) in [(MAX_LINKS, true), (MAX_LINKS + 1, false)] {
        let mut m = Model {
            nodes: BTreeMap::new(),
            roots: BTreeMap::from([(1, "/")]),
            follow_last: true,
        };
        m.nodes.insert("/end".into(), Node::File);
        let mut prev = "end".to_string();
        for i in 0..n {
            let name: &'static str = Box::leak(format!("l{i}").into_boxed_str());
            m.nodes.insert(
                format!("/{name}"),
                Node::Link(Box::leak(prev.clone().into_boxed_str())),
            );
            prev = name.to_string();
        }
        let t = Table {
            mounts: vec![(c("/"), 1)],
            cwd: None,
            cwd_handle: None,
        };
        let r = run(&m, &t, &format!("/{prev}"));
        assert_eq!(r.is_ok(), ok, "{n} links");
    }
}

#[test]
fn the_server_still_refuses_a_raw_dotdot_so_the_libc_must_never_send_one() {
    let (m, t) = (model(), root_table());
    // every path the table produces is free of `..`
    for p in ["a/../a/c", "/a/b/../b/link", "../a/c", "/mnt/store/../a/c"] {
        let place = t.place(p.as_bytes()).unwrap();
        assert!(place.rel.iter().all(|x| x != b".."), "{p}");
    }
    assert_eq!(m.open(1, b"a/../etc").unwrap_err(), Stop::Status(-108));
    assert_eq!(run(&m, &t, "/a/../etc/hosts").unwrap().0, "/etc/hosts");
}

#[test]
fn the_empty_path_names_nothing() {
    let (m, t) = (model(), root_table());
    assert_eq!(run(&m, &t, "").unwrap_err(), Fail::NoMount);
}
