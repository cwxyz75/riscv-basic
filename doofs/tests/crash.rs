//! The crash harness (FS-SPEC section 11, check 2; invariants FI3, FI4). A workload of one to three operations (create, write,
//! truncate, rename, rename-over, unlink, mkdir, fsync) runs on `doofs` over a recording device, in lockstep with `memfs` as the
//! model. The device log is then replayed to every crash point, with the writes that were not yet flushed dropped in every
//! pattern we sample (none, all, each alone dropped, random subsets) and some of them torn (half a block). Each image must
//! mount, pass `fsck`, and hold exactly the model state of the last durable commit or of a commit that was in flight; any
//! state after a returned `fsync` must still be there (FI4).

use doofs::{fsck::check_fs, Fs, ROOT};
use fsfmt::{check, mkfs, mount, DevError, Device, MemDevice, BLOCK};
use memfs::{AttrType, Kind, Limits, Term};
use std::collections::BTreeMap;

const BLOCKS: u64 = 120;

#[derive(Clone)]
enum Op {
    W(u64, Box<[u8; BLOCK]>),
    F,
}

struct Rec {
    dev: MemDevice,
    log: Vec<Op>,
}

impl Device for Rec {
    fn blocks(&self) -> u64 {
        self.dev.blocks()
    }
    fn read(&mut self, a: u64, b: &mut [u8; BLOCK]) -> Result<(), DevError> {
        self.dev.read(a, b)
    }
    fn write(&mut self, a: u64, d: &[u8; BLOCK]) -> Result<(), DevError> {
        self.log.push(Op::W(a, Box::new(*d)));
        self.dev.write(a, d)
    }
    fn flush(&mut self) -> Result<(), DevError> {
        self.log.push(Op::F);
        self.dev.flush()
    }
}

#[derive(Clone, Debug)]
enum W {
    Create(&'static str),
    Mkdir(&'static str),
    Write(&'static str, u64, usize, u8),
    Truncate(&'static str, u64),
    Rename(&'static str, &'static str),
    Unlink(&'static str),
    SetAttr(&'static str, &'static [u8]),
    RemoveAttr(&'static str),
    MakeIndex,
    Fsync,
}

fn all_ops() -> Vec<W> {
    use W::*;
    vec![
        Create("n"),
        Create("d/n"),
        Mkdir("m"),
        Write("g", 0, 5000, 0x11),
        Write("d/f", 4000, 300, 0x22),
        Truncate("d/f", 100),
        Truncate("g", 9000),
        Rename("g", "h"),
        Rename("g", "d/f"),
        Rename("d", "e"),
        Unlink("g"),
        Unlink("d/f"),
        SetAttr("g", b"red"),
        SetAttr("d/f", b"blue"),
        RemoveAttr("g"),
        MakeIndex,
        Fsync,
    ]
}

/// The state as (path, kind, content) triples: what a client could see.
type Dump = BTreeMap<String, (Kind, Vec<u8>)>;

trait Tree {
    fn dirs(&mut self, dir: u64) -> Vec<(Vec<u8>, Kind, u64)>;
    fn bytes(&mut self, id: u64) -> Vec<u8>;
    /// The attributes of a node as (name, value), and the entries of the `tag` index as (key, path).
    fn attrs(&mut self, id: u64) -> Vec<(Vec<u8>, Vec<u8>)>;
    fn tagged(&mut self) -> Vec<(Vec<u8>, Vec<u8>)>;
}

struct M<'a>(&'a mut memfs::Fs);
impl Tree for M<'_> {
    fn dirs(&mut self, dir: u64) -> Vec<(Vec<u8>, Kind, u64)> {
        let l = self.0.read_dir(dir as u32, None, 10_000).unwrap();
        l.into_iter()
            .map(|(n, k)| {
                let id = self.0.lookup(dir as u32, &n).unwrap() as u64;
                (n, k, id)
            })
            .collect()
    }
    fn bytes(&mut self, id: u64) -> Vec<u8> {
        self.0.read(id as u32, 0, 1 << 20).unwrap()
    }
    fn attrs(&mut self, id: u64) -> Vec<(Vec<u8>, Vec<u8>)> {
        let id = id as u32;
        let names = self.0.attr_list(id).unwrap();
        names
            .into_iter()
            .map(|n| {
                let v = self.0.attr_get(id, &n).unwrap().1;
                (n, v)
            })
            .collect()
    }
    fn tagged(&mut self) -> Vec<(Vec<u8>, Vec<u8>)> {
        match self
            .0
            .query(0, b"tag", &Term::Range { lo: None, hi: None }, None, 1000)
        {
            Ok(r) => {
                let mut v: Vec<_> = r.hits.into_iter().map(|h| (h.key, h.path)).collect();
                v.sort();
                v
            }
            Err(_) => Vec::new(),
        }
    }
}

struct D<'a, X: Device>(&'a mut Fs<X>);
impl<X: Device> Tree for D<'_, X> {
    fn dirs(&mut self, dir: u64) -> Vec<(Vec<u8>, Kind, u64)> {
        let l = self.0.read_dir(dir, None, 10_000).unwrap();
        l.into_iter()
            .map(|(n, k)| {
                let id = self.0.lookup(dir, &n).unwrap();
                (n, k, id)
            })
            .collect()
    }
    fn bytes(&mut self, id: u64) -> Vec<u8> {
        self.0
            .read(id, 0, 1 << 20)
            .expect("a file reads without error")
    }
    fn attrs(&mut self, id: u64) -> Vec<(Vec<u8>, Vec<u8>)> {
        let names = self.0.attr_list(id).expect("attributes list");
        names
            .into_iter()
            .map(|n| {
                let v = self.0.attr_get(id, &n).unwrap().1;
                (n, v)
            })
            .collect()
    }
    fn tagged(&mut self) -> Vec<(Vec<u8>, Vec<u8>)> {
        match self.0.query(
            ROOT,
            b"tag",
            &Term::Range { lo: None, hi: None },
            None,
            1000,
        ) {
            Ok(r) => {
                let mut v: Vec<_> = r.hits.into_iter().map(|h| (h.key, h.path)).collect();
                v.sort();
                v
            }
            Err(_) => Vec::new(),
        }
    }
}

fn dump(t: &mut dyn Tree) -> Dump {
    let mut out = Dump::new();
    let mut work = vec![(ROOT, String::new())];
    while let Some((dir, path)) = work.pop() {
        for (n, k, id) in t.dirs(dir) {
            let p = format!("{path}/{}", String::from_utf8_lossy(&n));
            for (an, av) in t.attrs(id) {
                out.insert(
                    format!("{p}@{}", String::from_utf8_lossy(&an)),
                    (Kind::File, av),
                );
            }
            match k {
                Kind::Dir => {
                    out.insert(p.clone(), (k, vec![]));
                    work.push((id, p));
                }
                Kind::File => {
                    let b = t.bytes(id);
                    out.insert(p, (k, b));
                }
                _ => {
                    out.insert(p, (k, vec![]));
                }
            }
        }
    }
    for (key, path) in t.tagged() {
        out.insert(
            format!(
                "tag-index:{}:{}",
                String::from_utf8_lossy(&key),
                String::from_utf8_lossy(&path)
            ),
            (Kind::File, vec![]),
        );
    }
    out
}

/// A short form of a dump for a failure message: path, kind, length and a checksum of the content.
fn brief(d: &Dump) -> String {
    d.iter()
        .map(|(p, (k, b))| format!("{p}:{k:?}:{}:{:08x}", b.len(), fsfmt::crc::crc32c(b)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn split(p: &str) -> (Vec<&[u8]>, &[u8]) {
    let mut v: Vec<&[u8]> = p.split('/').map(|s| s.as_bytes()).collect();
    let name = v.pop().unwrap();
    (v, name)
}

fn payload(len: usize, b: u8) -> Vec<u8> {
    (0..len).map(|i| b.wrapping_add((i % 251) as u8)).collect()
}

fn apply_m(m: &mut memfs::Fs, w: &W) {
    let walk = |m: &memfs::Fs, p: &str| -> Option<u32> {
        let (dir, name) = split(p);
        let d = m.walk(0, &dir).ok()?;
        m.lookup(d, name).ok()
    };
    match w {
        W::Create(p) => {
            let (d, n) = split(p);
            if let Ok(d) = m.walk(0, &d) {
                let _ = m.create_file(d, n);
            }
        }
        W::Mkdir(p) => {
            let (d, n) = split(p);
            if let Ok(d) = m.walk(0, &d) {
                let _ = m.mkdir(d, n);
            }
        }
        W::Write(p, off, len, b) => {
            if let Some(id) = walk(m, p) {
                let _ = m.write(id, *off, &payload(*len, *b));
            }
        }
        W::Truncate(p, s) => {
            if let Some(id) = walk(m, p) {
                let _ = m.set_size(id, *s);
            }
        }
        W::Rename(a, b) => {
            let ((da, na), (db, nb)) = (split(a), split(b));
            if let (Ok(x), Ok(y)) = (m.walk(0, &da), m.walk(0, &db)) {
                let _ = m.rename(x, na, y, nb);
            }
        }
        W::Unlink(p) => {
            let (d, n) = split(p);
            if let Ok(d) = m.walk(0, &d) {
                let _ = m.unlink(d, n);
            }
        }
        W::SetAttr(p, v) => {
            if let Some(id) = walk(m, p) {
                let _ = m.attr_set(id, b"tag", AttrType::Text, v);
            }
        }
        W::RemoveAttr(p) => {
            if let Some(id) = walk(m, p) {
                let _ = m.attr_remove(id, b"tag");
            }
        }
        W::MakeIndex => {
            let _ = m.make_index(b"tag", AttrType::Text);
        }
        W::Fsync => {}
    }
}

fn apply_d<X: Device>(f: &mut Fs<X>, w: &W) {
    fn walk<X: Device>(f: &mut Fs<X>, p: &str) -> Option<u64> {
        let (dir, name) = split(p);
        let d = f.walk(ROOT, &dir).ok()?;
        f.lookup(d, name).ok()
    }
    match w {
        W::Create(p) => {
            let (d, n) = split(p);
            if let Ok(d) = f.walk(ROOT, &d) {
                let _ = f.create_file(d, n);
            }
        }
        W::Mkdir(p) => {
            let (d, n) = split(p);
            if let Ok(d) = f.walk(ROOT, &d) {
                let _ = f.mkdir(d, n);
            }
        }
        W::Write(p, off, len, b) => {
            if let Some(id) = walk(f, p) {
                let _ = f.write(id, *off, &payload(*len, *b));
            }
        }
        W::Truncate(p, s) => {
            if let Some(id) = walk(f, p) {
                let _ = f.set_size(id, *s);
            }
        }
        W::Rename(a, b) => {
            let ((da, na), (db, nb)) = (split(a), split(b));
            if let (Ok(x), Ok(y)) = (f.walk(ROOT, &da), f.walk(ROOT, &db)) {
                let _ = f.rename(x, na, y, nb);
            }
        }
        W::Unlink(p) => {
            let (d, n) = split(p);
            if let Ok(d) = f.walk(ROOT, &d) {
                let _ = f.unlink(d, n);
            }
        }
        W::SetAttr(p, v) => {
            if let Some(id) = walk(f, p) {
                let _ = f.attr_set(id, b"tag", AttrType::Text, v);
            }
        }
        W::RemoveAttr(p) => {
            if let Some(id) = walk(f, p) {
                let _ = f.attr_remove(id, b"tag");
            }
        }
        W::MakeIndex => {
            let _ = f.make_index(b"tag", AttrType::Text);
        }
        W::Fsync => f.commit().expect("commit"),
    }
}

/// The initial, committed state: a directory with a file, and a small file.
fn initial() -> (MemDevice, memfs::Fs) {
    let limits = Limits::default();
    let mut dev = MemDevice::new(BLOCKS);
    mkfs(&mut dev, *b"crash-harness-id", "crash").unwrap();
    let mut f = Fs::open(mount(dev).unwrap(), limits).unwrap();
    let mut m = memfs::Fs::new(limits);
    for w in [
        W::Mkdir("d"),
        W::Create("d/f"),
        W::Write("d/f", 0, 6000, 0x33),
        W::Create("g"),
        W::Write("g", 0, 100, 0x44),
    ] {
        apply_d(&mut f, &w);
        apply_m(&mut m, &w);
    }
    f.commit().unwrap();
    (f.into_volume().into_device(), m)
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

struct Run {
    base: MemDevice,
    log: Vec<Op>,
    /// Per commit that wrote anything: (index of its slot write, index just after its last flush).
    commits: Vec<(usize, usize)>,
    /// models[0] is the initial state; models[j] the state at the j-th commit.
    models: Vec<Dump>,
}

fn run_workload(ws: &[W]) -> Run {
    let (base, mut model) = initial();
    let rec = Rec {
        dev: base.clone(),
        log: Vec::new(),
    };
    let mut f = Fs::open(mount(rec).unwrap(), Limits::default()).unwrap();
    let opened = f.vol.pool.dev.log.len();
    assert_eq!(opened, 0, "opening a clean volume writes nothing");
    let mut models = vec![dump(&mut M(&mut model))];
    let mut commits = Vec::new();
    for w in ws {
        let before = f.vol.pool.dev.log.len();
        apply_d(&mut f, w);
        apply_m(&mut model, w);
        let after = f.vol.pool.dev.log.len();
        if matches!(w, W::Fsync) && after > before {
            commits.push((after - 2, after));
            models.push(dump(&mut M(&mut model)));
        }
    }
    let log = f.into_volume().into_device().log;
    Run {
        base,
        log,
        commits,
        models,
    }
}

/// The image after a crash at `k` (the first `k` device operations happened): durable writes in full, the pending ones as chosen.
fn image(run: &Run, k: usize, pick: impl Fn(usize) -> Pick) -> MemDevice {
    let mut dev = run.base.clone();
    let last_flush = run.log[..k].iter().rposition(|o| matches!(o, Op::F));
    let durable_to = last_flush.map_or(0, |i| i + 1);
    for op in &run.log[..durable_to] {
        if let Op::W(a, d) = op {
            dev.write(*a, d).unwrap();
        }
    }
    for (n, op) in run.log[durable_to..k].iter().enumerate() {
        if let Op::W(a, d) = op {
            match pick(n) {
                Pick::Drop => {}
                Pick::Keep => dev.write(*a, d).unwrap(),
                Pick::Tear => {
                    let mut old = [0u8; BLOCK];
                    dev.read(*a, &mut old).unwrap();
                    old[..BLOCK / 2].copy_from_slice(&d[..BLOCK / 2]);
                    dev.write(*a, &old).unwrap();
                }
            }
        }
    }
    dev
}

#[derive(Clone, Copy, PartialEq)]
enum Pick {
    Drop,
    Keep,
    Tear,
}

fn verify(run: &Run, k: usize, dev: MemDevice, what: &str) {
    // FI3 and FI4: the state is that of a commit no older than the last one durable at `k`, and no newer than one in flight
    let durable = run.commits.iter().filter(|(_, end)| *end <= k).count();
    let started = run.commits.iter().filter(|(slot, _)| *slot < k).count();
    let rep = {
        let mut d = dev.clone();
        check(&mut d)
    };
    assert!(
        rep.problems.is_empty(),
        "{what}: volume check: {:?}",
        rep.problems
    );
    let vol = mount(dev).unwrap_or_else(|e| panic!("{what}: mount failed: {e:?}"));
    let mut f =
        Fs::open(vol, Limits::default()).unwrap_or_else(|e| panic!("{what}: open failed: {e:?}"));
    let problems = check_fs(&mut f);
    assert!(problems.is_empty(), "{what}: fsck: {problems:?}");
    let got = dump(&mut D(&mut f));
    assert!(
        run.models[durable..=started].contains(&got),
        "{what}: the state is none of commits {durable}..={started} (durable {durable}, started {started})\ngot {}",
        brief(&got)
    );
    // the remounted volume keeps working: another commit, another mount, the same state
    f.commit().unwrap();
    let vol = mount(f.into_volume().into_device()).unwrap();
    let mut f = Fs::open(vol, Limits::default()).unwrap();
    assert_eq!(
        dump(&mut D(&mut f)),
        got,
        "{what}: unstable across a remount"
    );
}

fn crash_test(ws: &[W], rng: &mut Rng, variants: usize) -> usize {
    let run = run_workload(ws);
    let n = run.log.len();
    let mut scenarios = 0;
    for k in 0..=n {
        let name = format!("{ws:?} crash after {k}/{n}");
        let pending = {
            let lf = run.log[..k]
                .iter()
                .rposition(|o| matches!(o, Op::F))
                .map_or(0, |i| i + 1);
            run.log[lf..k]
                .iter()
                .filter(|o| matches!(o, Op::W(..)))
                .count()
        };
        let mut picks: Vec<Vec<Pick>> = vec![vec![Pick::Drop; pending], vec![Pick::Keep; pending]];
        for i in 0..pending.min(4) {
            let mut v = vec![Pick::Keep; pending];
            v[i * pending / pending.clamp(1, 4)] = Pick::Drop;
            picks.push(v);
        }
        if pending > 0 {
            let mut v = vec![Pick::Keep; pending];
            v[pending - 1] = Pick::Tear;
            picks.push(v);
        }
        for _ in 0..variants {
            picks.push(
                (0..pending)
                    .map(|_| [Pick::Drop, Pick::Keep, Pick::Tear][(rng.next() % 3) as usize])
                    .collect(),
            );
        }
        for p in picks {
            let img = image(&run, k, |i| p.get(i).copied().unwrap_or(Pick::Drop));
            verify(&run, k, img, &name);
            scenarios += 1;
        }
    }
    scenarios
}

#[test]
fn every_one_and_two_operation_workload_survives_a_crash_at_every_point() {
    let ops = all_ops();
    let mut rng = Rng(0x5EED_1234_ABCD_9876);
    let mut scenarios = 0;
    for a in &ops {
        scenarios += crash_test(&[a.clone(), W::Fsync], &mut rng, 2);
        for b in &ops {
            scenarios += crash_test(&[a.clone(), b.clone()], &mut rng, 1);
        }
    }
    eprintln!("{scenarios} crash scenarios");
    assert!(scenarios > 10_000);
}

#[test]
fn sampled_three_operation_workloads_survive_too() {
    let ops = all_ops();
    let mut rng = Rng(0xFEED_5EED_0123_4567);
    let n: usize = std::env::var("DOOFS_CRASH3")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let mut scenarios = 0;
    for _ in 0..n {
        let ws: Vec<W> = (0..3)
            .map(|_| ops[(rng.next() % ops.len() as u64) as usize].clone())
            .collect();
        scenarios += crash_test(&ws, &mut rng, 2);
    }
    eprintln!("{scenarios} crash scenarios");
}

/// Attribute and index workloads that end in a commit (the exhaustive two-operation set has no room for three).
#[test]
fn attribute_and_index_workloads_survive_a_crash_at_every_point() {
    use W::*;
    let mut rng = Rng(0xA77_1B07_E5);
    let sets: Vec<Vec<W>> = vec![
        vec![SetAttr("g", b"red"), MakeIndex, Fsync],
        vec![MakeIndex, SetAttr("g", b"red"), Fsync],
        vec![
            SetAttr("g", b"red"),
            SetAttr("d/f", b"blue"),
            MakeIndex,
            Fsync,
        ],
        vec![MakeIndex, SetAttr("g", b"red"), RemoveAttr("g"), Fsync],
        vec![MakeIndex, SetAttr("g", b"red"), Rename("g", "h"), Fsync],
        vec![MakeIndex, SetAttr("g", b"red"), Unlink("g"), Fsync],
        vec![SetAttr("g", b"red"), Fsync, MakeIndex, Fsync],
        vec![MakeIndex, Fsync, SetAttr("g", b"red"), Fsync],
        vec![
            MakeIndex,
            Write("g", 0, 5000, 7),
            Fsync,
            Truncate("g", 10),
            Fsync,
        ],
    ];
    for ws in sets {
        crash_test(&ws, &mut rng, 3);
    }
}
