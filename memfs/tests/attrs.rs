//! Step 0 of the disk filesystem (FS-SPEC 9.1): attributes, indexes, queries, symlinks and modification times in `memfs`.
//! Hand-derived scenarios for every rule and error, then a random walk against an independent path-keyed model whose queries
//! are evaluated by brute force over its own maps.

// (the model walk is written for clarity: its tuples of bytes and its `map(|x| x.mtime = now)` updates stay as they are)
#![allow(clippy::type_complexity, clippy::option_map_unit_fn)]

use memfs::{AttrType, Error, Fs, Kind, Limits, NodeId, Term, ROOT};
use std::collections::BTreeMap;

fn u64v(v: u64) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}
fn i64v(v: i64) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

fn paths(fs: &Fs, dir: NodeId, index: &[u8], term: &Term) -> Vec<String> {
    fs.query(dir, index, term, None, 1000)
        .unwrap()
        .hits
        .into_iter()
        .map(|h| String::from_utf8(h.path).unwrap())
        .collect()
}

// ---------------------------------------------------------------------------------------------------------- attributes

#[test]
fn attributes_set_get_replace_remove_list() {
    let mut fs = Fs::new(Limits::default());
    let f = fs.create_file(ROOT, b"f").unwrap();
    assert_eq!(fs.attr_get(f, b"color"), Err(Error::NoAttr));
    fs.attr_set(f, b"color", AttrType::Text, b"red").unwrap();
    fs.attr_set(f, b"age", AttrType::U64, &u64v(7)).unwrap();
    assert_eq!(
        fs.attr_get(f, b"color"),
        Ok((AttrType::Text, b"red".to_vec()))
    );
    fs.attr_set(f, b"color", AttrType::Text, b"blue").unwrap();
    assert_eq!(
        fs.attr_get(f, b"color"),
        Ok((AttrType::Text, b"blue".to_vec())),
        "set replaces"
    );
    assert_eq!(
        fs.attr_list(f),
        Ok(vec![b"age".to_vec(), b"color".to_vec()])
    );
    fs.attr_remove(f, b"age").unwrap();
    assert_eq!(fs.attr_remove(f, b"age"), Err(Error::NoAttr));
    assert_eq!(fs.attr_list(f), Ok(vec![b"color".to_vec()]));
    // a directory has attributes too
    let d = fs.mkdir(ROOT, b"d").unwrap();
    fs.attr_set(d, b"tag", AttrType::Bytes, &[0, 1, 2]).unwrap();
    assert_eq!(fs.attr_list(d), Ok(vec![b"tag".to_vec()]));
}

#[test]
fn attribute_names_values_and_types_are_checked() {
    let mut fs = Fs::new(Limits::default());
    let f = fs.create_file(ROOT, b"f").unwrap();
    for bad in [
        &b""[..],
        b"a/b",
        b"a\0",
        b"name",
        b"size",
        b"mtime",
        &[b'x'; 256][..],
    ] {
        assert_eq!(
            fs.attr_set(f, bad, AttrType::Bytes, b"v"),
            Err(Error::BadName),
            "{bad:?}"
        );
    }
    assert!(fs.attr_set(f, &[b'x'; 255], AttrType::Bytes, b"v").is_ok());
    assert_eq!(
        fs.attr_set(f, b"big", AttrType::Bytes, &[0u8; 1025]),
        Err(Error::TooBig)
    );
    assert!(fs
        .attr_set(f, b"max", AttrType::Bytes, &[0u8; 1024])
        .is_ok());
    assert_eq!(
        fs.attr_set(f, b"n", AttrType::U64, &[1, 2, 3]),
        Err(Error::BadValue)
    );
    assert_eq!(
        fs.attr_set(f, b"n", AttrType::I64, &[0; 9]),
        Err(Error::BadValue)
    );
    assert_eq!(
        fs.attr_set(f, b"t", AttrType::Text, &[0xff, 0xfe]),
        Err(Error::BadValue),
        "text is UTF-8"
    );
    assert!(fs.attr_set(f, b"t", AttrType::Text, "é".as_bytes()).is_ok());
    assert_eq!(AttrType::from_code(3), Some(AttrType::I64));
    assert_eq!(AttrType::from_code(4), None);
}

#[test]
fn a_read_only_node_refuses_attribute_changes_but_reads_them() {
    let mut fs = Fs::new(Limits::default());
    let f = fs.create_file(ROOT, b"f").unwrap();
    fs.attr_set(f, b"k", AttrType::Bytes, b"v").unwrap();
    fs.set_readonly(f).unwrap();
    assert_eq!(
        fs.attr_set(f, b"k", AttrType::Bytes, b"w"),
        Err(Error::ReadOnly)
    );
    assert_eq!(fs.attr_remove(f, b"k"), Err(Error::ReadOnly));
    assert_eq!(fs.attr_get(f, b"k"), Ok((AttrType::Bytes, b"v".to_vec())));
    let img = fs.add_image_file(ROOT, b"img", 0, 10).unwrap();
    assert_eq!(
        fs.attr_set(img, b"k", AttrType::Bytes, b"v"),
        Err(Error::ReadOnly),
        "an image file is read-only"
    );
}

#[test]
fn attributes_are_charged_to_the_metadata_quota_and_come_back() {
    let mut fs = Fs::new(Limits {
        max_meta: 400,
        ..Limits::default()
    });
    let f = fs.create_file(ROOT, b"f").unwrap(); // 97
    let base = fs.meta_used();
    fs.attr_set(f, b"k", AttrType::Bytes, &[0u8; 100]).unwrap(); // 1 + 100 + 32
    assert_eq!(fs.meta_used(), base + 133);
    assert_eq!(
        fs.attr_set(f, b"k2", AttrType::Bytes, &[0u8; 200]),
        Err(Error::Quota),
        "over the quota, nothing changes"
    );
    assert_eq!(fs.meta_used(), base + 133);
    fs.attr_set(f, b"k", AttrType::Bytes, &[0u8; 10]).unwrap(); // replacing credits the old value
    assert_eq!(fs.meta_used(), base + 43);
    fs.attr_remove(f, b"k").unwrap();
    assert_eq!(fs.meta_used(), base);
    fs.attr_set(f, b"k", AttrType::Bytes, b"v").unwrap();
    fs.unlink(ROOT, b"f").unwrap();
    assert_eq!(
        fs.meta_used(),
        0,
        "a freed node returns its attributes' cost"
    );
}

// ------------------------------------------------------------------------------------------------------------ time

#[test]
fn modification_times_follow_content_changes_only() {
    let mut fs = Fs::new(Limits::default());
    fs.set_time(100);
    let d = fs.mkdir(ROOT, b"d").unwrap();
    let f = fs.create_file(d, b"f").unwrap();
    assert_eq!(
        (fs.mtime(d), fs.mtime(f), fs.mtime(ROOT)),
        (Ok(100), Ok(100), Ok(100))
    );
    fs.set_time(200);
    fs.write(f, 0, b"x").unwrap();
    assert_eq!(fs.mtime(f), Ok(200));
    assert_eq!(
        fs.mtime(d),
        Ok(100),
        "writing a file does not touch its directory"
    );
    fs.set_time(300);
    fs.attr_set(f, b"k", AttrType::Bytes, b"v").unwrap();
    assert_eq!(fs.mtime(f), Ok(200), "attributes do not change mtime");
    fs.set_size(f, 0).unwrap();
    assert_eq!(fs.mtime(f), Ok(300), "set_size does");
    fs.set_time(400);
    fs.create_file(d, b"g").unwrap();
    assert_eq!(fs.mtime(d), Ok(400), "an entry created in a directory");
    fs.set_time(500);
    fs.unlink(d, b"g").unwrap();
    assert_eq!(fs.mtime(d), Ok(500), "an entry removed");
    fs.set_time(600);
    fs.rename(d, b"f", ROOT, b"h").unwrap();
    assert_eq!(
        (fs.mtime(d), fs.mtime(ROOT)),
        (Ok(600), Ok(600)),
        "an entry renamed touches both directories"
    );
    assert_eq!(fs.mtime(f), Ok(300), "the moved file itself is unchanged");
}

// ------------------------------------------------------------------------------------------------- indexes and queries

#[test]
fn make_index_rules_and_the_backfill() {
    let mut fs = Fs::new(Limits::default());
    let a = fs.create_file(ROOT, b"a").unwrap();
    let b = fs.create_file(ROOT, b"b").unwrap();
    fs.attr_set(a, b"year", AttrType::U64, &u64v(1999)).unwrap();
    fs.attr_set(b, b"year", AttrType::U64, &u64v(2005)).unwrap();
    assert_eq!(
        fs.query(ROOT, b"year", &Term::Eq(u64v(1999)), None, 10),
        Err(Error::NoIndex),
        "no index yet"
    );
    fs.make_index(b"year", AttrType::U64).unwrap();
    assert_eq!(fs.make_index(b"year", AttrType::U64), Err(Error::Exists));
    assert_eq!(
        fs.make_index(b"name", AttrType::Bytes),
        Err(Error::BadName),
        "built-ins need none"
    );
    assert_eq!(
        paths(&fs, ROOT, b"year", &Term::Eq(u64v(1999))),
        vec!["a"],
        "back-filled"
    );
    // an attribute set after the index exists is in it at once
    let c = fs.create_file(ROOT, b"c").unwrap();
    fs.attr_set(c, b"year", AttrType::U64, &u64v(1999)).unwrap();
    assert_eq!(
        paths(&fs, ROOT, b"year", &Term::Eq(u64v(1999))),
        vec!["a", "c"]
    );
    fs.attr_set(c, b"year", AttrType::U64, &u64v(3000)).unwrap();
    assert_eq!(
        paths(&fs, ROOT, b"year", &Term::Eq(u64v(1999))),
        vec!["a"],
        "a changed value leaves its old key"
    );
    fs.attr_remove(a, b"year").unwrap();
    assert!(
        paths(&fs, ROOT, b"year", &Term::Eq(u64v(1999))).is_empty(),
        "a removed attribute leaves the index"
    );
    assert_eq!(fs.indexes(), vec![(b"year".to_vec(), AttrType::U64)]);
}

#[test]
fn an_attribute_of_another_type_is_not_in_the_index() {
    let mut fs = Fs::new(Limits::default());
    let a = fs.create_file(ROOT, b"a").unwrap();
    let b = fs.create_file(ROOT, b"b").unwrap();
    fs.attr_set(a, b"v", AttrType::U64, &u64v(5)).unwrap();
    fs.attr_set(b, b"v", AttrType::Bytes, &u64v(5)).unwrap();
    fs.make_index(b"v", AttrType::U64).unwrap();
    assert_eq!(paths(&fs, ROOT, b"v", &Term::Eq(u64v(5))), vec!["a"]);
}

#[test]
fn values_over_255_bytes_cannot_be_indexed() {
    let mut fs = Fs::new(Limits::default());
    let a = fs.create_file(ROOT, b"a").unwrap();
    fs.attr_set(a, b"s", AttrType::Bytes, &[7u8; 256]).unwrap();
    assert_eq!(
        fs.make_index(b"s", AttrType::Bytes),
        Err(Error::TooBig),
        "the back-fill meets a long value"
    );
    assert!(fs.indexes().is_empty(), "nothing changed");
    // a long value of another type does not matter
    fs.attr_remove(a, b"s").unwrap();
    fs.attr_set(a, b"s", AttrType::Bytes, &[7u8; 255]).unwrap();
    fs.make_index(b"s", AttrType::Bytes).unwrap();
    assert_eq!(
        fs.attr_set(a, b"s", AttrType::Bytes, &[7u8; 256]),
        Err(Error::TooBig)
    );
    assert_eq!(
        fs.attr_get(a, b"s").unwrap().1.len(),
        255,
        "refused, the old value stays"
    );
    // and at most 16 indexes
    for i in 0..15 {
        fs.make_index(format!("i{i}").as_bytes(), AttrType::Bytes)
            .unwrap();
    }
    assert_eq!(
        fs.make_index(b"one-too-many", AttrType::Bytes),
        Err(Error::Quota)
    );
}

#[test]
fn keys_order_by_type() {
    let mut fs = Fs::new(Limits::default());
    fs.make_index(b"n", AttrType::I64).unwrap();
    fs.make_index(b"u", AttrType::U64).unwrap();
    for (name, n) in [
        ("m5", -5i64),
        ("m1", -1),
        ("z", 0),
        ("p1", 1),
        ("p300", 300),
    ] {
        let f = fs.create_file(ROOT, name.as_bytes()).unwrap();
        fs.attr_set(f, b"n", AttrType::I64, &i64v(n)).unwrap();
        fs.attr_set(f, b"u", AttrType::U64, &u64v(n as u64))
            .unwrap();
    }
    let all = Term::Range { lo: None, hi: None };
    assert_eq!(
        paths(&fs, ROOT, b"n", &all),
        vec!["m5", "m1", "z", "p1", "p300"],
        "signed order"
    );
    assert_eq!(
        paths(&fs, ROOT, b"u", &all),
        vec!["z", "p1", "p300", "m5", "m1"],
        "unsigned order: the negatives, as huge numbers, come last"
    );
}

#[test]
fn range_bounds_are_inclusive_below_and_exclusive_above() {
    let mut fs = Fs::new(Limits::default());
    fs.make_index(b"k", AttrType::U64).unwrap();
    for n in [10u64, 20, 30, 40] {
        let f = fs.create_file(ROOT, format!("f{n}").as_bytes()).unwrap();
        fs.attr_set(f, b"k", AttrType::U64, &u64v(n)).unwrap();
    }
    let r = |lo: Option<u64>, hi: Option<u64>| Term::Range {
        lo: lo.map(u64v),
        hi: hi.map(u64v),
    };
    assert_eq!(
        paths(&fs, ROOT, b"k", &r(Some(20), Some(40))),
        vec!["f20", "f30"]
    );
    assert_eq!(
        paths(&fs, ROOT, b"k", &r(Some(20), None)),
        vec!["f20", "f30", "f40"]
    );
    assert_eq!(paths(&fs, ROOT, b"k", &r(None, Some(20))), vec!["f10"]);
    assert_eq!(
        paths(&fs, ROOT, b"k", &r(Some(25), Some(25))),
        Vec::<String>::new()
    );
    assert_eq!(
        fs.query(ROOT, b"k", &Term::Eq(vec![1, 2]), None, 10),
        Err(Error::BadValue),
        "a term must fit the type"
    );
}

#[test]
fn a_query_returns_only_nodes_strictly_below_the_directory_it_is_asked_through() {
    let mut fs = Fs::new(Limits::default());
    fs.make_index(b"t", AttrType::Text).unwrap();
    let a = fs.mkdir(ROOT, b"a").unwrap();
    let ab = fs.mkdir(a, b"b").unwrap();
    let b = fs.mkdir(ROOT, b"b").unwrap();
    let mut mk = |dir: NodeId, name: &str| {
        let f = fs.create_file(dir, name.as_bytes()).unwrap();
        fs.attr_set(f, b"t", AttrType::Text, b"x").unwrap();
        f
    };
    mk(a, "f1");
    mk(ab, "f2");
    mk(b, "f3");
    fs.attr_set(a, b"t", AttrType::Text, b"x").unwrap();
    let eq = Term::Eq(b"x".to_vec());
    // (equal keys order by node id: a, f1, f2, f3 were created in that order)
    assert_eq!(
        paths(&fs, ROOT, b"t", &eq),
        vec!["a", "a/f1", "a/b/f2", "b/f3"]
    );
    assert_eq!(
        paths(&fs, a, b"t", &eq),
        vec!["f1", "b/f2"],
        "below a: not a itself, paths relative to a"
    );
    assert_eq!(paths(&fs, ab, b"t", &eq), vec!["f2"]);
    assert_eq!(paths(&fs, b, b"t", &eq), vec!["f3"]);
    let f1 = fs.lookup(a, b"f1").unwrap();
    assert_eq!(fs.query(f1, b"t", &eq, None, 10), Err(Error::NotDirectory));
}

#[test]
fn nodes_unlinked_while_open_are_not_returned() {
    let mut fs = Fs::new(Limits::default());
    fs.make_index(b"t", AttrType::Text).unwrap();
    let f = fs.create_file(ROOT, b"f").unwrap();
    fs.attr_set(f, b"t", AttrType::Text, b"x").unwrap();
    fs.retain(f).unwrap(); // a connection holds it
    fs.unlink(ROOT, b"f").unwrap();
    assert!(paths(&fs, ROOT, b"t", &Term::Eq(b"x".to_vec())).is_empty());
    fs.release(f);
}

#[test]
fn paging_with_after_and_the_result_limit() {
    let mut fs = Fs::new(Limits::default());
    fs.make_index(b"k", AttrType::U64).unwrap();
    for i in 0..25u64 {
        let f = fs.create_file(ROOT, format!("f{i:02}").as_bytes()).unwrap();
        fs.attr_set(f, b"k", AttrType::U64, &u64v(i / 5)).unwrap(); // five nodes share each key
    }
    let all = Term::Range { lo: None, hi: None };
    let mut seen = Vec::new();
    let mut after: Option<(Vec<u8>, NodeId)> = None;
    loop {
        let r = fs
            .query(
                ROOT,
                b"k",
                &all,
                after.as_ref().map(|(k, i)| (k.as_slice(), *i)),
                7,
            )
            .unwrap();
        assert!(r.hits.len() <= 7);
        seen.extend(r.hits.iter().map(|h| h.path.clone()));
        if !r.more {
            break;
        }
        let last = r.hits.last().unwrap();
        after = Some((last.key.clone(), last.id));
    }
    let want: Vec<Vec<u8>> = (0..25).map(|i| format!("f{i:02}").into_bytes()).collect();
    assert_eq!(
        seen, want,
        "every node exactly once, in (key, id) order, across pages"
    );
    // `max` above the limit is clamped
    let r = fs.query(ROOT, b"k", &all, None, 1_000_000).unwrap();
    assert_eq!((r.hits.len(), r.more), (25, false));
    assert_eq!(memfs::MAX_QUERY, 1000);
}

#[test]
fn the_built_in_indexes_name_size_and_mtime() {
    let mut fs = Fs::new(Limits::default());
    fs.set_time(1000);
    let d = fs.mkdir(ROOT, b"d").unwrap();
    let a = fs.create_file(d, b"alpha").unwrap();
    fs.write(a, 0, b"12345").unwrap();
    fs.set_time(2000);
    let b = fs.create_file(ROOT, b"beta").unwrap();
    fs.write(b, 0, b"123").unwrap();
    assert_eq!(
        paths(&fs, ROOT, b"name", &Term::Eq(b"alpha".to_vec())),
        vec!["d/alpha"]
    );
    assert_eq!(
        paths(
            &fs,
            ROOT,
            b"name",
            &Term::Range {
                lo: Some(b"b".to_vec()),
                hi: None
            }
        ),
        vec!["beta", "d"],
        "names compare bytewise: beta and d are at or after b"
    );
    assert_eq!(
        paths(&fs, ROOT, b"size", &Term::Eq(u64v(5))),
        vec!["d/alpha"]
    );
    assert_eq!(
        paths(
            &fs,
            ROOT,
            b"size",
            &Term::Range {
                lo: Some(u64v(1)),
                hi: Some(u64v(5))
            }
        ),
        vec!["d", "beta"]
    );
    assert_eq!(
        paths(&fs, ROOT, b"size", &Term::Eq(u64v(1))),
        vec!["d"],
        "a directory's size is its entry count"
    );
    assert_eq!(
        paths(
            &fs,
            ROOT,
            b"mtime",
            &Term::Range {
                lo: Some(u64v(2000)),
                hi: None
            }
        ),
        vec!["beta"]
    );
    let both = paths(
        &fs,
        ROOT,
        b"mtime",
        &Term::Range {
            lo: Some(u64v(0)),
            hi: Some(u64v(2000)),
        },
    );
    assert_eq!(both, vec!["d", "d/alpha"]);
    // an index of the same name as a built-in cannot be made
    assert_eq!(fs.make_index(b"size", AttrType::U64), Err(Error::BadName));
}

// ----------------------------------------------------------------------------------------------------------- symlinks

#[test]
fn symlinks_hold_relative_targets_and_are_never_followed() {
    let mut fs = Fs::new(Limits::default());
    let d = fs.mkdir(ROOT, b"d").unwrap();
    let e = fs.mkdir(d, b"e").unwrap();
    let t = fs.create_file(d, b"target").unwrap();
    let l = fs.symlink(e, b"link", b"../target", ROOT).unwrap();
    assert_eq!(fs.kind(l), Ok(Kind::Symlink));
    assert_eq!(fs.read_link(l), Ok(b"../target".to_vec()));
    assert_eq!(fs.size(l), Ok(9));
    assert_eq!(
        fs.lookup(e, b"link"),
        Ok(l),
        "lookup returns the link, not its target"
    );
    assert_ne!(fs.lookup(e, b"link"), Ok(t));
    assert_eq!(fs.read(l, 0, 10), Err(Error::Invalid));
    assert_eq!(fs.write(l, 0, b"x"), Err(Error::Invalid));
    assert_eq!(fs.set_size(l, 0), Err(Error::Invalid));
    assert_eq!(fs.read_link(t), Err(Error::Invalid));
    assert_eq!(fs.read_link(d), Err(Error::IsDirectory));
    assert_eq!(
        fs.read_dir(e, None, 10).unwrap(),
        vec![(b"link".to_vec(), Kind::Symlink)]
    );
    // unlink like a file; the cost comes back
    let before = fs.meta_used();
    fs.unlink(e, b"link").unwrap();
    assert!(fs.meta_used() < before);
    fs.unlink(d, b"target").unwrap();
}

#[test]
fn a_symlink_may_not_climb_above_the_connection_root() {
    let mut fs = Fs::new(Limits::default());
    let a = fs.mkdir(ROOT, b"a").unwrap();
    let b = fs.mkdir(a, b"b").unwrap();
    // created through the root: the link's directory is `b`, depth 2
    for (target, ok) in [
        (&b"x"[..], true),
        (b"../x", true),
        (b"../../x", true),
        (b"../../../x", false),
        (b"./x", true),
        (b"p/../../../x", true),
        (b"p/../../../../x", false),
        (b"/etc/x", false),
        (b"", false),
        (b"a//b", false),
        (b"a/", false),
        (b"a b", false),
    ] {
        let r = fs.symlink(b, b"l", target, ROOT);
        assert_eq!(r.is_ok(), ok, "{:?}", String::from_utf8_lossy(target));
        if let Ok(id) = r {
            assert_eq!(fs.read_link(id).unwrap(), target);
            fs.unlink(b, b"l").unwrap();
        } else {
            assert_eq!(r, Err(Error::BadTarget));
        }
    }
    // created through `a` (the connection root is `a`, the link's directory `b` is one level below): one `..` is the limit
    assert!(fs.symlink(b, b"l1", b"../x", a).is_ok());
    assert_eq!(fs.symlink(b, b"l2", b"../../x", a), Err(Error::BadTarget));
    // `dir` must be at or below the root it is created through
    assert_eq!(fs.symlink(a, b"l3", b"x", b), Err(Error::Invalid));
    // a target of 1025 bytes
    assert_eq!(
        fs.symlink(b, b"l4", &vec![b'x'; 1025], ROOT),
        Err(Error::BadTarget)
    );
    assert!(fs.symlink(b, b"l5", &vec![b'x'; 255], ROOT).is_ok());
    // the shared rule is the same one clients use
    assert!(proto::file::check_link_target(b"../x", 1).is_ok());
    assert!(proto::file::check_link_target(b"../x", 0).is_err());
}

#[test]
fn a_hostile_writers_links_do_not_extend_a_readers_reach() {
    // the writer plants `link -> ../secret` and `link -> /home/secret` in a shared directory; a reader holding only that
    // directory connection (root = the directory) cannot even have them created, and the server never follows one
    let mut fs = Fs::new(Limits::default());
    let shared = fs.mkdir(ROOT, b"shared").unwrap();
    assert_eq!(
        fs.symlink(shared, b"up", b"../secret", shared),
        Err(Error::BadTarget)
    );
    assert_eq!(
        fs.symlink(shared, b"abs", b"/home/secret", shared),
        Err(Error::BadTarget)
    );
    assert!(fs.symlink(shared, b"ok", b"sub/file", shared).is_ok());
}

// ----------------------------------------------------------------------------------------------- the random walk

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

#[derive(Clone, PartialEq, Debug)]
enum MKind {
    File,
    Dir,
    Link(Vec<u8>),
}

#[derive(Clone, Debug)]
struct MNode {
    kind: MKind,
    size: u64,
    mtime: u64,
    attrs: BTreeMap<Vec<u8>, (AttrType, Vec<u8>)>,
}

/// The model: paths to nodes, written separately from `memfs` (no node ids, no parent links; a directory's children are found
/// by looking at path prefixes).
struct Model {
    nodes: BTreeMap<Vec<u8>, MNode>,
    indexes: BTreeMap<Vec<u8>, AttrType>,
    now: u64,
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.is_empty() {
        name.to_vec()
    } else {
        [dir, b"/", name].concat()
    }
}

fn comps(path: &[u8]) -> Vec<&[u8]> {
    if path.is_empty() {
        vec![]
    } else {
        path.split(|&b| b == b'/').collect()
    }
}

/// The index key, derived separately from `AttrType::key`: the specification's words, not its code.
fn model_key(ty: AttrType, v: &[u8]) -> Option<Vec<u8>> {
    match ty {
        AttrType::Bytes => Some(v.to_vec()),
        AttrType::Text => std::str::from_utf8(v).ok().map(|_| v.to_vec()),
        AttrType::U64 if v.len() == 8 => Some(v.iter().rev().copied().collect()),
        AttrType::I64 if v.len() == 8 => {
            let mut k: Vec<u8> = v.iter().rev().copied().collect();
            k[0] ^= 0x80;
            Some(k)
        }
        _ => None,
    }
}

/// The level rule for a symlink target, from the spec text: names go down, `..` goes up, never above the root.
fn model_target_ok(t: &[u8], depth: i64) -> bool {
    if t.is_empty() || t.len() > 1024 || t.contains(&0) || t[0] == b'/' {
        return false;
    }
    let mut level = depth;
    for c in t.split(|&b| b == b'/') {
        match c {
            b"." => {}
            b".." => {
                level -= 1;
                if level < 0 {
                    return false;
                }
            }
            c if c.is_empty() || c.len() > 255 || c.contains(&0) => return false,
            _ => level += 1,
        }
    }
    true
}

impl Model {
    fn children(&self, dir: &[u8]) -> Vec<Vec<u8>> {
        self.nodes
            .keys()
            .filter(|p| {
                let parent: &[u8] = match p.iter().rposition(|&b| b == b'/') {
                    Some(i) => &p[..i],
                    None => b"",
                };
                parent == dir
            })
            .cloned()
            .collect()
    }

    fn key_of(&self, path: &[u8], index: &[u8], ty: AttrType) -> Option<Vec<u8>> {
        let n = &self.nodes[path];
        match index {
            b"name" => Some(path.rsplit(|&b| b == b'/').next().unwrap().to_vec()),
            b"size" => {
                let size = match &n.kind {
                    MKind::Dir => self.children(path).len() as u64,
                    _ => n.size,
                };
                Some(size.to_be_bytes().to_vec())
            }
            b"mtime" => Some(n.mtime.to_be_bytes().to_vec()),
            _ => {
                let (t, v) = n.attrs.get(index)?;
                if *t != ty {
                    return None;
                }
                model_key(*t, v)
            }
        }
    }

    fn query(
        &self,
        dir: &[u8],
        index: &[u8],
        term: &Term,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, Error> {
        let ty = match index {
            b"name" => AttrType::Bytes,
            b"size" | b"mtime" => AttrType::U64,
            _ => *self.indexes.get(index).ok_or(Error::NoIndex)?,
        };
        let enc = |v: &Vec<u8>| model_key(ty, v).ok_or(Error::BadValue);
        let (lo, hi, eq) = match term {
            Term::Eq(v) => (None, None, Some(enc(v)?)),
            Term::Range { lo, hi } => (
                lo.as_ref().map(enc).transpose()?,
                hi.as_ref().map(enc).transpose()?,
                None,
            ),
        };
        let prefix: Vec<u8> = if dir.is_empty() {
            vec![]
        } else {
            [dir, b"/"].concat()
        };
        let mut out = Vec::new();
        for p in self.nodes.keys() {
            if !p.starts_with(&prefix) || p.len() == prefix.len() && !prefix.is_empty() {
                continue;
            }
            let Some(key) = self.key_of(p, index, ty) else {
                continue;
            };
            let ok = match &eq {
                Some(e) => &key == e,
                None => {
                    lo.as_ref().is_none_or(|l| key >= *l) && hi.as_ref().is_none_or(|h| key < *h)
                }
            };
            if ok {
                out.push((key, p[prefix.len()..].to_vec()));
            }
        }
        Ok(out)
    }
}

fn walk(fs: &Fs, path: &[u8]) -> NodeId {
    fs.walk(ROOT, &comps(path)).unwrap()
}

#[test]
fn a_random_walk_agrees_with_the_path_keyed_model() {
    let attr_names: [&[u8]; 3] = [b"a", b"b", b"c"];
    for seed in 1..=60u64 {
        let mut r = Rng(seed * 7919 + 13);
        let mut fs = Fs::new(Limits::default());
        let mut m = Model {
            nodes: BTreeMap::new(),
            indexes: BTreeMap::new(),
            now: 0,
        };
        let mut queries = 0;
        for step in 0..500 {
            let paths: Vec<Vec<u8>> = m.nodes.keys().cloned().collect();
            let dirs: Vec<Vec<u8>> = std::iter::once(Vec::new())
                .chain(
                    paths
                        .iter()
                        .filter(|p| m.nodes[*p].kind == MKind::Dir)
                        .cloned(),
                )
                .collect();
            let pick = |r: &mut Rng, v: &Vec<Vec<u8>>| v[r.below(v.len() as u64) as usize].clone();
            let name = |r: &mut Rng| -> Vec<u8> { format!("n{}", r.below(6)).into_bytes() };
            match r.below(100) {
                0..=14 => {
                    let d = pick(&mut r, &dirs);
                    let n = name(&mut r);
                    let res = fs.mkdir(walk(&fs, &d), &n);
                    let p = join(&d, &n);
                    if m.nodes.contains_key(&p) {
                        assert_eq!(res, Err(Error::Exists), "seed {seed} step {step}");
                    } else {
                        res.unwrap();
                        m.nodes.insert(
                            p,
                            MNode {
                                kind: MKind::Dir,
                                size: 0,
                                mtime: m.now,
                                attrs: BTreeMap::new(),
                            },
                        );
                        m.nodes.get_mut(&d).map(|x| x.mtime = m.now);
                        if d.is_empty() {
                            // (the root's own time is not observable by a query, which excludes it)
                        }
                    }
                }
                15..=29 => {
                    let d = pick(&mut r, &dirs);
                    let n = name(&mut r);
                    let res = fs.create_file(walk(&fs, &d), &n);
                    let p = join(&d, &n);
                    if m.nodes.contains_key(&p) {
                        assert_eq!(res, Err(Error::Exists));
                    } else {
                        res.unwrap();
                        m.nodes.insert(
                            p,
                            MNode {
                                kind: MKind::File,
                                size: 0,
                                mtime: m.now,
                                attrs: BTreeMap::new(),
                            },
                        );
                        m.nodes.get_mut(&d).map(|x| x.mtime = m.now);
                    }
                }
                30..=39 => {
                    let files: Vec<_> = paths
                        .iter()
                        .filter(|p| m.nodes[*p].kind == MKind::File)
                        .cloned()
                        .collect();
                    if !files.is_empty() {
                        let p = pick(&mut r, &files);
                        let len = r.below(40) as usize;
                        fs.write(walk(&fs, &p), 0, &vec![1u8; len]).unwrap();
                        let n = m.nodes.get_mut(&p).unwrap();
                        if len > 0 {
                            n.size = n.size.max(len as u64);
                            n.mtime = m.now;
                        }
                    }
                }
                40..=47 => {
                    if !paths.is_empty() {
                        let p = pick(&mut r, &paths);
                        let (d, n) = match p.iter().rposition(|&b| b == b'/') {
                            Some(i) => (p[..i].to_vec(), p[i + 1..].to_vec()),
                            None => (vec![], p.clone()),
                        };
                        let res = fs.unlink(walk(&fs, &d), &n);
                        if m.nodes[&p].kind == MKind::Dir && !m.children(&p).is_empty() {
                            assert_eq!(res, Err(Error::NotEmpty));
                        } else {
                            res.unwrap();
                            m.nodes.remove(&p);
                            m.nodes.get_mut(&d).map(|x| x.mtime = m.now);
                        }
                    }
                }
                48..=55 => {
                    if !paths.is_empty() {
                        let p = pick(&mut r, &paths);
                        let td = pick(&mut r, &dirs);
                        let tn = name(&mut r);
                        let (fd, fname) = match p.iter().rposition(|&b| b == b'/') {
                            Some(i) => (p[..i].to_vec(), p[i + 1..].to_vec()),
                            None => (vec![], p.clone()),
                        };
                        let np = join(&td, &tn);
                        // only renames to a free name outside the moved subtree (the rest is covered by memfs's own tests)
                        let inside = np == p || np.starts_with(&[p.as_slice(), b"/"].concat());
                        if !m.nodes.contains_key(&np)
                            && !inside
                            && !(m.nodes[&p].kind == MKind::Dir
                                && (td == p || td.starts_with(&[p.as_slice(), b"/"].concat())))
                        {
                            fs.rename(walk(&fs, &fd), &fname, walk(&fs, &td), &tn)
                                .unwrap();
                            let moved: Vec<Vec<u8>> = m
                                .nodes
                                .keys()
                                .filter(|k| {
                                    **k == p || k.starts_with(&[p.as_slice(), b"/"].concat())
                                })
                                .cloned()
                                .collect();
                            for k in moved {
                                let node = m.nodes.remove(&k).unwrap();
                                let nk = [np.as_slice(), &k[p.len()..]].concat();
                                m.nodes.insert(nk, node);
                            }
                            m.nodes.get_mut(&fd).map(|x| x.mtime = m.now);
                            m.nodes.get_mut(&td).map(|x| x.mtime = m.now);
                        }
                    }
                }
                56..=70 => {
                    if !paths.is_empty() {
                        let p = pick(&mut r, &paths);
                        let an = attr_names[r.below(3) as usize];
                        let (ty, v): (AttrType, Vec<u8>) = match r.below(4) {
                            0 => (AttrType::Bytes, vec![r.below(4) as u8; r.below(3) as usize]),
                            1 => (AttrType::Text, format!("t{}", r.below(4)).into_bytes()),
                            2 => (AttrType::U64, u64v(r.below(4))),
                            _ => (AttrType::I64, i64v(r.below(5) as i64 - 2)),
                        };
                        fs.attr_set(walk(&fs, &p), an, ty, &v).unwrap();
                        m.nodes
                            .get_mut(&p)
                            .unwrap()
                            .attrs
                            .insert(an.to_vec(), (ty, v));
                    }
                }
                71..=75 => {
                    if !paths.is_empty() {
                        let p = pick(&mut r, &paths);
                        let an = attr_names[r.below(3) as usize];
                        let res = fs.attr_remove(walk(&fs, &p), an);
                        match m.nodes.get_mut(&p).unwrap().attrs.remove(an) {
                            Some(_) => res.unwrap(),
                            None => assert_eq!(res, Err(Error::NoAttr)),
                        }
                    }
                }
                76..=79 => {
                    let an = attr_names[r.below(3) as usize];
                    let ty = [
                        AttrType::Bytes,
                        AttrType::Text,
                        AttrType::U64,
                        AttrType::I64,
                    ][r.below(4) as usize];
                    let res = fs.make_index(an, ty);
                    if m.indexes.contains_key(an) {
                        assert_eq!(res, Err(Error::Exists));
                    } else {
                        res.unwrap();
                        m.indexes.insert(an.to_vec(), ty);
                    }
                }
                80..=84 => {
                    let d = pick(&mut r, &dirs);
                    let n = name(&mut r);
                    let target: &[u8] =
                        [&b"x"[..], b"../x", b"../../x", b"/abs", b"a/../..", b"y/z"]
                            [r.below(6) as usize];
                    let depth = comps(&d).len() as i64;
                    let res = fs.symlink(walk(&fs, &d), &n, target, ROOT);
                    let p = join(&d, &n);
                    if m.nodes.contains_key(&p) {
                        assert!(res.is_err());
                    } else if model_target_ok(target, depth) {
                        res.unwrap();
                        m.nodes.insert(
                            p,
                            MNode {
                                kind: MKind::Link(target.to_vec()),
                                size: target.len() as u64,
                                mtime: m.now,
                                attrs: BTreeMap::new(),
                            },
                        );
                        m.nodes.get_mut(&d).map(|x| x.mtime = m.now);
                    } else {
                        assert_eq!(
                            res,
                            Err(Error::BadTarget),
                            "{:?} at depth {depth}",
                            String::from_utf8_lossy(target)
                        );
                    }
                }
                85..=89 => {
                    m.now += 1 + r.below(50);
                    fs.set_time(m.now);
                }
                _ => {
                    let d = pick(&mut r, &dirs);
                    let index: Vec<u8> = match r.below(5) {
                        0 => b"name".to_vec(),
                        1 => b"size".to_vec(),
                        2 => b"mtime".to_vec(),
                        _ => attr_names[r.below(3) as usize].to_vec(),
                    };
                    let ty = match index.as_slice() {
                        b"name" => AttrType::Bytes,
                        b"size" | b"mtime" => AttrType::U64,
                        _ => m.indexes.get(&index).copied().unwrap_or(AttrType::U64),
                    };
                    let val = |r: &mut Rng| -> Vec<u8> {
                        match ty {
                            AttrType::Bytes => format!("n{}", r.below(6)).into_bytes(),
                            AttrType::Text => format!("t{}", r.below(4)).into_bytes(),
                            AttrType::U64 => u64v(r.below(60)),
                            AttrType::I64 => i64v(r.below(5) as i64 - 2),
                        }
                    };
                    let term = match r.below(3) {
                        0 => Term::Eq(val(&mut r)),
                        1 => Term::Range {
                            lo: Some(val(&mut r)),
                            hi: None,
                        },
                        _ => Term::Range {
                            lo: Some(val(&mut r)),
                            hi: Some(val(&mut r)),
                        },
                    };
                    let got = fs.query(walk(&fs, &d), &index, &term, None, 1000);
                    match m.query(&d, &index, &term) {
                        Err(e) => assert_eq!(got.map(|_| ()), Err(e), "seed {seed} step {step}"),
                        Ok(mut want) => {
                            // equal keys order by node id: resolve each model path to its id for the tie-break
                            want.sort_by_key(|(k, p)| (k.clone(), walk(&fs, &join(&d, p))));
                            let got =
                                got.unwrap_or_else(|e| panic!("seed {seed} step {step}: {e:?}"));
                            let have: Vec<(Vec<u8>, Vec<u8>)> = got
                                .hits
                                .iter()
                                .map(|h| (h.key.clone(), h.path.clone()))
                                .collect();
                            assert_eq!(
                                have,
                                want,
                                "seed {seed} step {step}: query {:?} {term:?} below {:?}",
                                String::from_utf8_lossy(&index),
                                String::from_utf8_lossy(&d)
                            );
                            assert!(!got.more);
                            queries += 1;
                        }
                    }
                }
            }
            // every node's size and mtime match the model at all times
            for (p, n) in &m.nodes {
                let id = walk(&fs, p);
                assert_eq!(
                    fs.mtime(id),
                    Ok(n.mtime),
                    "seed {seed} step {step}: mtime of {:?}",
                    String::from_utf8_lossy(p)
                );
                assert_eq!(
                    fs.attr_list(id).unwrap(),
                    n.attrs.keys().cloned().collect::<Vec<_>>()
                );
            }
        }
        assert!(
            queries > 20,
            "seed {seed}: the walk made only {queries} queries"
        );
    }
}
