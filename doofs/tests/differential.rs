//! Differential test (FS-SPEC section 8, step 3): the same random operation sequence through `memfs` (the oracle) and `doofs`
//! (on an in-memory volume). Every result must match: the same error for a refused operation, the same bytes, listing, size,
//! kind and modification time for an accepted one, the same quota counters; and the volume must verify and survive a remount.

use doofs::{Fs, FsError};
use fsfmt::{check, mkfs, mount, verify_all, MemDevice};
use memfs::{AttrType, Error, Limits, Term};
use std::collections::BTreeMap;

thread_local! {
    static SEEN: std::cell::RefCell<BTreeMap<String, usize>> = const { std::cell::RefCell::new(BTreeMap::new()) };
}

fn seen(k: String) {
    SEEN.with(|s| *s.borrow_mut().entry(k).or_insert(0) += 1);
}

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

const LIMITS: Limits = Limits {
    max_nodes: 40,
    max_bytes: 64 * 1024,
    max_file: 24 * 1024,
    max_meta: 6 * 1024,
};
const BLOCKS: u64 = 3000;

struct Pair {
    m: memfs::Fs,
    d: Fs<MemDevice>,
    /// memfs id to doofs id, for every node ever made (a reused memfs number is overwritten).
    map: BTreeMap<u32, u64>,
    ids: Vec<u32>,
    held: Vec<u32>,
    now: u64,
}

fn same<T: PartialEq + std::fmt::Debug>(
    what: &str,
    m: Result<T, Error>,
    d: Result<T, FsError>,
) -> Option<T> {
    match (m, d) {
        (Ok(a), Ok(b)) => {
            assert_eq!(a, b, "{what}");
            seen("ok".into());
            Some(a)
        }
        (Err(a), Err(FsError::Fs(b))) => {
            assert_eq!(a, b, "{what}: error");
            seen(format!("{a:?}"));
            None
        }
        (a, b) => panic!("{what}: memfs {a:?} but doofs {b:?}"),
    }
}

impl Pair {
    fn new() -> Pair {
        let mut dev = MemDevice::new(BLOCKS);
        mkfs(&mut dev, *b"differential-uid", "diff").unwrap();
        let d = Fs::open(mount(dev).unwrap(), LIMITS).unwrap();
        let mut map = BTreeMap::new();
        map.insert(0, 0);
        let mut m = memfs::Fs::new(LIMITS);
        m.set_time(1000);
        let mut d = d;
        d.set_time(1000);
        Pair {
            m,
            d,
            map,
            ids: vec![0],
            held: Vec::new(),
            now: 1000,
        }
    }

    fn did(&self, m: u32) -> u64 {
        self.map.get(&m).copied().unwrap_or(9_999_999)
    }

    fn made(&mut self, m: Result<u32, Error>, d: Result<u64, FsError>, what: &str) {
        match (m, d) {
            (Ok(a), Ok(b)) => {
                self.map.insert(a, b);
                if !self.ids.contains(&a) {
                    self.ids.push(a);
                }
            }
            (Err(a), Err(FsError::Fs(b))) => {
                assert_eq!(a, b, "{what}: error");
                seen(format!("{a:?}"));
            }
            (a, b) => panic!("{what}: memfs {a:?} but doofs {b:?}"),
        }
    }

    fn pick(&self, r: &mut Rng) -> u32 {
        if r.below(30) == 0 {
            return 4_000_000;
        }
        self.ids[r.below(self.ids.len())]
    }

    fn compare_node(&mut self, id: u32) {
        let did = self.did(id);
        let k = same("kind", self.m.kind(id), self.d.kind(did));
        if k.is_some() {
            same("readonly", self.m.is_readonly(id), self.d.is_readonly(did));
            same("mtime", self.m.mtime(id), self.d.mtime(did));
            // the parent is compared through the map
            let (pm, pd) = (self.m.parent(id).unwrap(), self.d.parent(did).unwrap());
            assert_eq!(self.did(pm), pd, "parent of {id}");
            // size of a file or link; a directory's entry count
            same("size", self.m.size(id), self.d.size(did));
        }
    }

    /// Walk both trees and compare everything reachable from the root.
    fn deep(&mut self) {
        let mut work = vec![0u32];
        while let Some(dir) = work.pop() {
            let dd = self.did(dir);
            let lm = self.m.read_dir(dir, None, 1000).unwrap();
            let ld = self.d.read_dir(dd, None, 1000).unwrap();
            assert_eq!(lm.len(), ld.len(), "entries of {dir}");
            for ((nm, km), (nd, kd)) in lm.iter().zip(ld.iter()) {
                assert_eq!(nm, nd);
                assert_eq!(km, kd, "kind of {nm:?}");
                let cm = self.m.lookup(dir, nm).unwrap();
                let cd = self.d.lookup(dd, nd).unwrap();
                assert_eq!(self.did(cm), cd, "{nm:?} names the same node");
                self.compare_node(cm);
                match km {
                    memfs::Kind::Dir => work.push(cm),
                    memfs::Kind::File => {
                        let a = self.m.read(cm, 0, usize::MAX >> 4).unwrap();
                        let b = self.d.read(cd, 0, usize::MAX >> 4).unwrap();
                        assert!(a == b, "contents of {nm:?}");
                    }
                    memfs::Kind::Symlink => {
                        assert_eq!(self.m.read_link(cm).unwrap(), self.d.read_link(cd).unwrap())
                    }
                    memfs::Kind::Image => unreachable!(),
                }
            }
        }
        // FI2: nothing the last commit or the live tree reaches is on the free list (a crash now must find the last commit whole)
        let vol = &mut self.d.vol;
        let mut free = vol.pool.free_list().to_vec();
        free.sort_unstable();
        let (live_root, live_gen, last) = (vol.tree.root(), vol.pool.open_gen, vol.last);
        for (root, gen) in [(live_root, live_gen), (last.root, last.gen)] {
            let v = verify_all(&mut vol.pool, root, gen).unwrap();
            assert!(
                v.reachable.iter().all(|a| free.binary_search(a).is_err()),
                "a reachable block is free"
            );
        }
        let problems = doofs::fsck::check_fs(&mut self.d);
        assert!(problems.is_empty(), "fsck: {problems:?}");
        let c = self.d.counters();
        assert_eq!(c.nodes as usize, self.m.node_count(), "node count");
        assert_eq!(c.bytes as usize, self.m.bytes_used(), "bytes");
        assert_eq!(c.meta as usize, self.m.meta_used(), "meta");
        for id in self.ids.clone() {
            self.compare_node(id);
            let did = self.did(id);
            let (m, d) = (self.m.attr_list(id), self.d.attr_list(did));
            if let Some(names) = same("attr list", m, d) {
                for n in names {
                    same(
                        "attr value",
                        self.m.attr_get(id, &n),
                        self.d.attr_get(did, &n),
                    );
                }
            }
        }
        assert_eq!(self.m.indexes(), self.d.indexes().unwrap(), "the indexes");
    }

    /// A random query through both, compared as sets ordered by (key, path): the tie-break by node id differs between the
    /// two (memfs reuses ids), so what is compared in order is the key, and the paths of equal keys as sets.
    fn query(&mut self, r: &mut Rng, what: &str) {
        let dir = if r.below(3) == 0 { self.pick(r) } else { 0 };
        let index: &[u8] = [
            &b"name"[..],
            b"size",
            b"mtime",
            b"color",
            b"rank",
            b"tag",
            b"missing",
        ][r.below(7)];
        let enc = |r: &mut Rng, ty_hint: usize| -> Vec<u8> {
            match (index, ty_hint) {
                (b"name", _) => NAMES[r.below(NAMES.len())].to_vec(),
                (b"size" | b"mtime", _) => (r.below(3000) as u64).to_le_bytes().to_vec(),
                (_, 0) => (r.below(5) as i64 - 2).to_le_bytes().to_vec(),
                (_, 1) => vec![b'a' + r.below(3) as u8],
                _ => vec![1, 2],
            }
        };
        let hint = r.below(3);
        let term = match r.below(3) {
            0 => Term::Eq(enc(r, hint)),
            1 => Term::Range {
                lo: Some(enc(r, hint)),
                hi: None,
            },
            _ => Term::Range {
                lo: if r.below(2) == 0 {
                    None
                } else {
                    Some(enc(r, hint))
                },
                hi: Some(enc(r, hint)),
            },
        };
        let m = self.m.query(dir, index, &term, None, 1000);
        let d = self.d.query(self.did(dir), index, &term, None, 1000);
        match (m, d) {
            (Ok(a), Ok(b)) => {
                let key = |h: &(Vec<u8>, Vec<u8>)| h.clone();
                let mut x: Vec<_> = a
                    .hits
                    .iter()
                    .map(|h| (h.key.clone(), h.path.clone()))
                    .collect();
                let mut y: Vec<_> = b
                    .hits
                    .iter()
                    .map(|h| (h.key.clone(), h.path.clone()))
                    .collect();
                // doofs's own order is (key, id): check it, then compare as sets
                assert!(
                    b.hits
                        .windows(2)
                        .all(|w| (&w[0].key, w[0].id) < (&w[1].key, w[1].id)),
                    "{what}: order"
                );
                x.sort_by_key(key);
                y.sort_by_key(key);
                assert_eq!(x, y, "{what}: query hits");
                assert_eq!(a.more, b.more, "{what}: more");
                seen("query ok".into());
                if !b.hits.is_empty() {
                    seen("query hit".into());
                }
                // paging in doofs: pages of 2 by (key, id) rebuild the full list, and `more` is right
                let mut got = Vec::new();
                let mut after: Option<(Vec<u8>, u64)> = None;
                loop {
                    let page = self
                        .d
                        .query(
                            self.did(dir),
                            index,
                            &term,
                            after.as_ref().map(|(k, i)| (k.as_slice(), *i)),
                            2,
                        )
                        .unwrap();
                    got.extend(page.hits.iter().cloned());
                    if !page.more {
                        break;
                    }
                    let last = page.hits.last().unwrap();
                    after = Some((last.key.clone(), last.id));
                }
                assert_eq!(got, b.hits, "{what}: pages rebuild the list");
                let capped = self.d.query(self.did(dir), index, &term, None, 2).unwrap();
                assert_eq!(capped.more, b.hits.len() > 2, "{what}: more at a cap of 2");
                assert_eq!(capped.hits.len(), b.hits.len().min(2));
            }
            (Err(a), Err(FsError::Fs(b))) => {
                assert_eq!(a, b, "{what}: query error");
                seen(format!("query {a:?}"));
            }
            (a, b) => panic!("{what}: memfs {a:?} but doofs {b:?}"),
        }
    }

    fn remount(&mut self) {
        for h in std::mem::take(&mut self.held) {
            self.m.release(h);
        }
        seen("remount".into());
        self.d.commit().unwrap();
        let placeholder = Fs::open(mount(fresh_dev()).unwrap(), LIMITS).unwrap();
        let old = std::mem::replace(&mut self.d, placeholder);
        let dev = old.into_volume().into_device();
        self.d = Fs::open(mount(dev).unwrap(), LIMITS).unwrap();
        self.d.set_time(self.now);
    }
}

fn fresh_dev() -> MemDevice {
    let mut d = MemDevice::new(100);
    mkfs(&mut d, [1; 16], "x").unwrap();
    d
}

const NAMES: &[&[u8]] = &[
    b"a",
    b"b",
    b"c",
    b"d",
    b"dir",
    b"e",
    b"",
    b".",
    b"..",
    b"x/y",
    b"long-name-0123456789012345678901234567890123456789",
    // names are UTF-8 without control characters (FS-SPEC F6, step 0b): good ones that look alike but differ, and refused ones
    b"caf\xc3\xa9",
    b"cafe\xcc\x81",
    b"\xff\xfe",
    b"tab\there",
    b"nel\xc2\x85",
    b"\xed\xa0\x80",
];
const ATTR_NAMES: &[&[u8]] = &[
    b"color",
    b"rank",
    b"tag",
    b"x",
    b"y",
    b"name",
    b"size",
    b"mtime",
    b"",
    b"a/b",
    b"caf\xc3\xa9",
    b"\xffattr",
    b"ctl\x01",
];

/// A random attribute value, mostly well formed, sometimes not (the wrong length, bad UTF-8, too long to store or to index).
fn attr_value(r: &mut Rng) -> (AttrType, Vec<u8>) {
    let ty = [
        AttrType::Bytes,
        AttrType::Text,
        AttrType::U64,
        AttrType::I64,
    ][r.below(4)];
    let v = match (ty, r.below(12)) {
        (AttrType::Bytes | AttrType::Text, 0) => vec![b'z'; 300],
        (AttrType::Bytes | AttrType::Text, 1) => vec![b'q'; 1100],
        (AttrType::Text, 2) => vec![0xff, 0xfe],
        (AttrType::Bytes, 3) => vec![0, 1, 0, 255],
        (AttrType::Bytes | AttrType::Text, _) => {
            let n = r.below(6);
            (0..n).map(|_| b"abc"[r.below(3)]).collect()
        }
        (_, 0) => vec![1, 2, 3],
        (_, _) => (r.below(5) as i64 - 2).to_le_bytes().to_vec(),
    };
    (ty, v)
}

const TARGETS: &[&[u8]] = &[
    b"a",
    b"../a",
    b"x/y",
    b"..",
    b"/abs",
    b"a/../..",
    b"",
    b"dir/a/../b",
    b"./a",
];

fn data(r: &mut Rng) -> Vec<u8> {
    let n = match r.below(4) {
        0 => r.below(20),
        1 => r.below(700),
        2 => r.below(5000),
        _ => r.below(9000),
    };
    let b = r.next() as u8;
    (0..n).map(|i| b.wrapping_add((i * 7) as u8)).collect()
}

fn run(seed: u64, steps: usize) {
    let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut p = Pair::new();
    for step in 0..steps {
        let what = format!("seed {seed} step {step}");
        let name = NAMES[r.below(NAMES.len())];
        match r.below(34) {
            0..=2 => {
                let dir = p.pick(&mut r);
                let (m, d) = (
                    p.m.create_file(dir, name),
                    p.d.create_file(p.did(dir), name),
                );
                p.made(m, d, &what);
            }
            3..=4 => {
                let dir = p.pick(&mut r);
                let (m, d) = (p.m.mkdir(dir, name), p.d.mkdir(p.did(dir), name));
                p.made(m, d, &what);
            }
            5 => {
                let (dir, root) = (p.pick(&mut r), p.pick(&mut r));
                let t = TARGETS[r.below(TARGETS.len())];
                let m = p.m.symlink(dir, name, t, root);
                let d = p.d.symlink(p.did(dir), name, t, p.did(root));
                p.made(m, d, &what);
            }
            6..=7 => {
                let dir = p.pick(&mut r);
                if p.m.lookup(dir, name).is_ok_and(|c| p.held.contains(&c)) {
                    seen("unlink-while-open".into());
                }
                same(&what, p.m.unlink(dir, name), p.d.unlink(p.did(dir), name));
            }
            8..=10 => {
                let (fd, td) = (p.pick(&mut r), p.pick(&mut r));
                let to = NAMES[r.below(NAMES.len())];
                same(
                    &what,
                    p.m.rename(fd, name, td, to),
                    p.d.rename(p.did(fd), name, p.did(td), to),
                );
            }
            11..=14 => {
                let id = p.pick(&mut r);
                let off = [0, 0, 1, 100, 4095, 4096, 4097, 9000, 20000, 24000][r.below(10)];
                let bytes = data(&mut r);
                same(
                    &what,
                    p.m.write(id, off, &bytes),
                    p.d.write(p.did(id), off, &bytes),
                );
            }
            15..=16 => {
                let id = p.pick(&mut r);
                let size = [0, 1, 100, 4096, 4097, 8192, 10000, 24576, 24577, 30000][r.below(10)];
                same(&what, p.m.set_size(id, size), p.d.set_size(p.did(id), size));
            }
            17 => {
                let id = p.pick(&mut r);
                let (off, len) = (
                    [0, 3, 4090, 4096, 9000][r.below(5)],
                    [1, 10, 5000, 30000][r.below(4)],
                );
                same(&what, p.m.read(id, off, len), p.d.read(p.did(id), off, len));
            }
            18 => {
                let id = p.pick(&mut r);
                let m = p.m.retain(id);
                let d = p.d.retain(p.did(id));
                if same(&what, m, d).is_some() {
                    p.held.push(id);
                }
            }
            19 => {
                if !p.held.is_empty() {
                    let id = p.held.swap_remove(r.below(p.held.len()));
                    p.m.release(id);
                    p.d.release(p.did(id)).unwrap();
                }
            }
            20 => {
                let dir = p.pick(&mut r);
                let after = if r.below(2) == 0 {
                    None
                } else {
                    Some(NAMES[r.below(NAMES.len())])
                };
                let max = [1, 3, 1000][r.below(3)];
                same(
                    &what,
                    p.m.read_dir(dir, after, max),
                    p.d.read_dir(p.did(dir), after, max),
                );
            }
            21 => {
                let dir = p.pick(&mut r);
                let (m, d) = (p.m.lookup(dir, name), p.d.lookup(p.did(dir), name));
                match (m, d) {
                    (Ok(a), Ok(b)) => assert_eq!(p.did(a), b, "{what}: lookup"),
                    (Err(a), Err(FsError::Fs(b))) => assert_eq!(a, b, "{what}"),
                    (a, b) => panic!("{what}: memfs {a:?} but doofs {b:?}"),
                }
            }
            22 => {
                if r.below(6) == 0 {
                    let id = p.pick(&mut r);
                    same(&what, p.m.set_readonly(id), p.d.set_readonly(p.did(id)));
                }
                p.now += 1 + r.next() % 50;
                p.m.set_time(p.now);
                p.d.set_time(p.now);
            }
            24..=26 => {
                let id = p.pick(&mut r);
                let name = ATTR_NAMES[r.below(ATTR_NAMES.len())];
                let (ty, value) = attr_value(&mut r);
                same(
                    &what,
                    p.m.attr_set(id, name, ty, &value),
                    p.d.attr_set(p.did(id), name, ty, &value),
                );
            }
            27 => {
                let id = p.pick(&mut r);
                let name = ATTR_NAMES[r.below(ATTR_NAMES.len())];
                same(&what, p.m.attr_get(id, name), p.d.attr_get(p.did(id), name));
            }
            28 => {
                let id = p.pick(&mut r);
                let name = ATTR_NAMES[r.below(ATTR_NAMES.len())];
                same(
                    &what,
                    p.m.attr_remove(id, name),
                    p.d.attr_remove(p.did(id), name),
                );
            }
            29 => {
                let id = p.pick(&mut r);
                same(&what, p.m.attr_list(id), p.d.attr_list(p.did(id)));
            }
            30 => {
                let name = ATTR_NAMES[r.below(ATTR_NAMES.len())];
                let ty = [
                    AttrType::Bytes,
                    AttrType::Text,
                    AttrType::U64,
                    AttrType::I64,
                ][r.below(4)];
                same(&what, p.m.make_index(name, ty), p.d.make_index(name, ty));
            }
            31..=32 => p.query(&mut r, &what),
            _ => {
                if r.below(8) == 0 {
                    p.remount();
                    p.deep();
                }
            }
        }
        if step % 40 == 0 {
            p.deep();
        }
    }
    p.deep();
    // release what is held, drain, and check the disk
    for h in std::mem::take(&mut p.held) {
        p.m.release(h);
        p.d.release(p.did(h)).unwrap();
    }
    p.deep();
    p.d.commit().unwrap();
    let mut vol = p.d.into_volume();
    let v = verify_all(&mut vol.pool, vol.last.root, vol.last.gen).unwrap();
    for a in &v.reachable {
        assert!(
            !vol.pool.free_list().contains(a),
            "seed {seed}: block {a} reachable and free"
        );
    }
    let rep = check(&mut vol.into_device());
    assert!(rep.problems.is_empty(), "seed {seed}: {:?}", rep.problems);
}

#[test]
fn doofs_matches_memfs_over_random_operations() {
    let seeds: u64 = std::env::var("DOOFS_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    for seed in 1..=seeds {
        run(seed, 1200);
    }
    // the walk must actually reach the interesting cases, or it proves little
    SEEN.with(|s| {
        let s = s.borrow();
        for k in [
            "ok",
            "Quota",
            "TooBig",
            "Exists",
            "NotEmpty",
            "NotFound",
            "BadName",
            "BadEncoding",
            "BadTarget",
            "ReadOnly",
            "Invalid",
            "IsDirectory",
            "NotDirectory",
            "remount",
            "unlink-while-open",
        ] {
            assert!(s.get(k).copied().unwrap_or(0) > 0, "never saw {k}: {s:?}");
        }
        eprintln!("{s:?}");
    });
}
