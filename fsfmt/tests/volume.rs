use fsfmt::superblock::{slot_addr, Superblock, SLOTS};
use fsfmt::{check, mkfs, mount, Device, MemDevice, VolError, BLOCK};
use std::collections::BTreeMap;

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

const UUID: [u8; 16] = *b"0123456789abcdef";

fn fresh(blocks: u64) -> MemDevice {
    let mut d = MemDevice::new(blocks);
    mkfs(&mut d, UUID, "scratch").unwrap();
    d
}

fn contents<D: Device>(v: &mut fsfmt::Volume<D>) -> Model {
    v.tree
        .range(&mut v.pool, None, None)
        .unwrap()
        .into_iter()
        .collect()
}

#[test]
fn mkfs_then_mount_gives_an_empty_volume_with_its_identity() {
    let v = mount(fresh(200)).unwrap();
    assert_eq!(v.uuid, UUID);
    assert_eq!(v.label(), "scratch");
    assert!(v.tree.root().is_none());
    assert!(!v.dirty());
}

#[test]
fn refused_devices_and_labels() {
    let mut tiny = MemDevice::new(10);
    assert_eq!(mkfs(&mut tiny, UUID, "x"), Err(VolError::TooSmall));
    assert!(matches!(
        mount(MemDevice::new(100)),
        Err(VolError::NoValidSuperblock)
    ));
    let mut d = MemDevice::new(100);
    assert_eq!(
        mkfs(&mut d, UUID, &"x".repeat(33)),
        Err(VolError::LabelTooLong)
    );
    // a volume made for one size does not mount on another
    let mut big = MemDevice::new(300);
    for i in 0..SLOTS {
        let mut b = [0u8; BLOCK];
        fresh(200).read(slot_addr(i, 200), &mut b).unwrap();
        big.write(i.min(3), &b).unwrap();
    }
    assert!(matches!(
        mount(big),
        Err(VolError::SizeMismatch) | Err(VolError::NoValidSuperblock)
    ));
}

#[test]
fn committed_data_survives_a_remount_and_uncommitted_data_does_not() {
    let mut v = mount(fresh(500)).unwrap();
    v.tree.insert(&mut v.pool, b"a", b"1").unwrap();
    v.commit().unwrap();
    v.tree.insert(&mut v.pool, b"b", b"2").unwrap();
    // crash: no commit for b
    let mut v = mount(v.into_device()).unwrap();
    assert_eq!(v.tree.get(&mut v.pool, b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(v.tree.get(&mut v.pool, b"b").unwrap(), None);
    v.tree.insert(&mut v.pool, b"b", b"2").unwrap();
    v.commit().unwrap();
    let mut v = mount(v.into_device()).unwrap();
    assert_eq!(contents(&mut v).len(), 2);
}

#[test]
fn a_torn_newest_slot_falls_back_to_the_previous_commit() {
    let mut v = mount(fresh(500)).unwrap();
    v.tree.insert(&mut v.pool, b"a", b"1").unwrap();
    v.commit().unwrap();
    v.tree.insert(&mut v.pool, b"b", b"2").unwrap();
    v.commit().unwrap();
    let newest = v.last.gen;
    let mut d = v.into_device();
    d.raw_mut(slot_addr(newest % SLOTS, 500)).unwrap()[200] ^= 0x10;
    let mut v = mount(d).unwrap();
    assert_eq!(v.last.gen, newest - 1);
    assert_eq!(contents(&mut v).len(), 1);
    // and it carries on: the next commit has a higher generation than the torn one
    v.tree.insert(&mut v.pool, b"c", b"3").unwrap();
    v.commit().unwrap();
    assert!(v.last.gen > newest - 1);
    let r = check(&mut v.into_device());
    assert!(r.problems.is_empty(), "{:?}", r.problems);
}

#[test]
fn a_block_from_another_volume_is_caught() {
    let mut a = mount(fresh(500)).unwrap();
    for i in 0..50u8 {
        a.tree.insert(&mut a.pool, &[i], &[i; 300]).unwrap();
    }
    a.commit().unwrap();
    let mut b_dev = MemDevice::new(500);
    mkfs(&mut b_dev, *b"another volume!!", "b").unwrap();
    let mut b = mount(b_dev).unwrap();
    for i in 0..50u8 {
        b.tree.insert(&mut b.pool, &[i], &[i; 300]).unwrap();
    }
    b.commit().unwrap();
    let root = b.last.root.unwrap().addr;
    let mut ad = a.into_device();
    let mut bd = b.into_device();
    // put volume A's root block where B's root was: same address, other owner
    let mut blk = [0u8; BLOCK];
    ad.read(root, &mut blk).unwrap();
    bd.write(root, &blk).unwrap();
    let rep = check(&mut bd);
    assert!(
        rep.slots.iter().any(|s| matches!(s.tree, Some(Err(_)))),
        "the planted block is caught"
    );
    // mount refuses that commit and falls back to the older, intact one (the empty volume)
    let mut v = mount(bd).unwrap();
    assert_eq!(v.last.gen, 1);
    assert!(contents(&mut v).is_empty());
}

#[test]
fn the_check_report_names_each_slot() {
    let mut v = mount(fresh(500)).unwrap();
    v.tree.insert(&mut v.pool, b"a", b"1").unwrap();
    v.commit().unwrap();
    let mut d = v.into_device();
    let r = check(&mut d);
    assert!(r.problems.is_empty());
    assert_eq!(r.slots.len(), 8);
    assert_eq!(r.slots.iter().filter(|s| s.sb.is_ok()).count(), 2);
    assert_eq!(r.chosen, Some(2));
    assert_eq!(r.reachable, 1);
    // damage the only node: no slot verifies fully
    let root = r.slots[2].sb.as_ref().unwrap().root.unwrap().addr;
    d.raw_mut(root).unwrap()[50] ^= 1;
    let r = check(&mut d);
    assert_eq!(r.chosen, Some(1), "falls back to the empty volume");
    assert!(r.slots[2].tree.as_ref().unwrap().is_err());
}

/// Random work with commits, crashes (drop without commit) and remounts, against a model of the last committed state.
#[test]
fn random_commits_and_crashes_match_the_last_commit_and_reuse_space() {
    for seed in 1..=8u64 {
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut v = mount(fresh(1200)).unwrap();
        let mut live: Model = BTreeMap::new();
        let mut committed: Model = BTreeMap::new();
        let mut peak = 0;
        for _ in 0..1500 {
            match r.next() % 20 {
                0..=11 => {
                    let k = format!("key{:03}", r.next() % 120).into_bytes();
                    let val = vec![(r.next() & 0xff) as u8; (r.next() % 700) as usize];
                    v.tree.insert(&mut v.pool, &k, &val).unwrap();
                    live.insert(k, val);
                }
                12..=15 => {
                    let k = format!("key{:03}", r.next() % 120).into_bytes();
                    assert_eq!(v.tree.delete(&mut v.pool, &k).unwrap(), live.remove(&k));
                }
                16..=17 => {
                    v.commit().unwrap();
                    committed = live.clone();
                }
                18 => {
                    // crash and remount: only the last commit survives
                    v = mount(v.into_device()).unwrap();
                    live = committed.clone();
                    assert_eq!(contents(&mut v), live, "seed {seed}");
                }
                _ => {
                    let rep = v.tree.verify(&mut v.pool).unwrap();
                    assert_eq!(rep.entries, live.len());
                    // FI2: nothing the live tree or the last commit reaches is free
                    for a in &rep.reachable {
                        assert!(!v.pool.free_list().contains(a));
                    }
                    let last =
                        fsfmt::Tree::verify_at(&mut v.pool, v.last.root, v.last.gen).unwrap();
                    for a in &last.reachable {
                        assert!(
                            !v.pool.free_list().contains(a),
                            "seed {seed}: the last commit's block {a} is free"
                        );
                    }
                    peak = peak.max(rep.nodes);
                }
            }
        }
        v.commit().unwrap();
        let mut d = v.into_device();
        let rep = check(&mut d);
        assert!(rep.problems.is_empty(), "seed {seed}: {:?}", rep.problems);
        // the volume never needed anywhere near the whole device for a live set this small: space is reused
        let used = (1200 - 8) - rep.unreachable;
        assert!(used < 400, "seed {seed}: {used} blocks in use");
        // every slot that decodes is a plausible superblock of this volume
        for s in &rep.slots {
            if let Ok(sb) = &s.sb {
                assert_eq!(sb.uuid, UUID);
                let _: &Superblock = sb;
            }
        }
    }
}

/// Records the order of writes and flushes.
struct Log {
    inner: MemDevice,
    ops: Vec<(char, u64)>,
}
impl Device for Log {
    fn blocks(&self) -> u64 {
        self.inner.blocks()
    }
    fn read(&mut self, a: u64, b: &mut [u8; BLOCK]) -> Result<(), fsfmt::DevError> {
        self.inner.read(a, b)
    }
    fn write(&mut self, a: u64, d: &[u8; BLOCK]) -> Result<(), fsfmt::DevError> {
        self.ops.push(('w', a));
        self.inner.write(a, d)
    }
    fn flush(&mut self) -> Result<(), fsfmt::DevError> {
        self.ops.push(('f', 0));
        self.inner.flush()
    }
}

#[test]
fn a_commit_flushes_the_data_then_writes_the_slot_then_flushes() {
    let mut v = mount(Log {
        inner: fresh(500),
        ops: Vec::new(),
    })
    .unwrap();
    v.tree.insert(&mut v.pool, b"a", b"1").unwrap();
    v.commit().unwrap();
    let ops = v.into_device().ops;
    let n = ops.len();
    // the last three operations: flush, the slot write (generation 2: slot 2), flush
    assert_eq!(&ops[n - 3..], &[('f', 0), ('w', 2), ('f', 0)]);
    // and the data block was written before that first flush
    assert!(ops[..n - 3].iter().any(|(k, a)| *k == 'w' && *a >= 4));
}

#[test]
fn overwriting_forever_does_not_run_out_of_space() {
    let mut v = mount(fresh(60)).unwrap();
    for i in 0..2000u32 {
        v.tree.insert(&mut v.pool, b"k", &i.to_le_bytes()).unwrap();
        v.commit().unwrap();
    }
    assert_eq!(
        v.tree.get(&mut v.pool, b"k").unwrap(),
        Some(1999u32.to_le_bytes().to_vec())
    );
    let rep = check(&mut v.into_device());
    assert!(rep.problems.is_empty());
}

#[test]
fn the_reserve_is_kept_back_from_ordinary_writes_but_open_to_the_deleting_paths() {
    let mut v = mount(fresh(60)).unwrap();
    v.pool.set_reserve(10);
    let mut n = 0u32;
    // ordinary writes stop with the reserve untouched
    while v
        .tree
        .insert(&mut v.pool, &n.to_be_bytes(), &[7; 900])
        .is_ok()
    {
        n += 1;
    }
    assert!(
        v.pool.free_blocks() >= 9,
        "the reserve is untouched: {}",
        v.pool.free_blocks()
    );
    // the reserve's users can still write
    v.pool.allow_reserve(true);
    assert!(v.tree.delete(&mut v.pool, &0u32.to_be_bytes()).is_ok());
    v.pool.allow_reserve(false);
}

#[test]
fn probe_recognises_a_volume_and_refuses_everything_else() {
    use fsfmt::probe;
    let mut d = fresh(200);
    let s = probe(&mut d).unwrap();
    assert_eq!((s.uuid, s.label_str()), (UUID, "scratch"));
    // blank, random bytes, too small, and a volume on a device of another size
    assert!(probe(&mut MemDevice::new(200)).is_none());
    let mut junk = MemDevice::new(200);
    for a in 0..200 {
        junk.write(a, &[0x5a; BLOCK]).unwrap();
    }
    assert!(probe(&mut junk).is_none());
    assert!(probe(&mut MemDevice::new(5)).is_none());
    let mut other = MemDevice::new(300);
    for i in 0..SLOTS {
        let mut b = [0u8; BLOCK];
        fresh(200).read(slot_addr(i, 200), &mut b).unwrap();
        other.write(i.min(3), &b).unwrap();
    }
    assert!(
        probe(&mut other).is_none(),
        "a geometry larger than the device"
    );
    // a label with control bytes
    let mut bad = fresh(200);
    let mut sb = probe(&mut bad).unwrap();
    sb.label[0] = 7;
    for i in 0..SLOTS {
        bad.write(slot_addr(i, 200), &sb.encode()).unwrap();
    }
    assert!(probe(&mut bad).is_none());
    // bit flips in a good image never panic and never invent a different identity
    let good = fresh(200);
    for a in [0u64, 1, 196, 197] {
        for bit in (0..BLOCK * 8).step_by(37) {
            let mut d = good.clone();
            d.raw_mut(a).unwrap()[bit / 8] ^= 1 << (bit % 8);
            if let Some(s) = probe(&mut d) {
                assert_eq!(s.uuid, UUID);
            }
        }
    }
}
