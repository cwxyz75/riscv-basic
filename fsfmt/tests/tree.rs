use fsfmt::crc::crc32c;
use fsfmt::device::MemDevice;
use fsfmt::pool::Pool;
use fsfmt::tree::{Snapshot, Tree};
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
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn key(r: &mut Rng, space: u64) -> Vec<u8> {
    let n = r.below(space);
    let mut k = format!("k{n:05}").into_bytes();
    if n.is_multiple_of(7) {
        k.extend(std::iter::repeat_n(b'x', 150 + (n % 100) as usize));
    }
    k
}

fn val(r: &mut Rng) -> Vec<u8> {
    let n = match r.below(4) {
        0 => 0,
        1 => 8,
        2 => 100,
        _ => 900,
    } as usize;
    (0..n).map(|_| r.next() as u8).collect()
}

#[test]
fn crc32c_check_vector() {
    assert_eq!(crc32c(b"123456789"), 0xE306_9283);
}

#[test]
fn random_walk_matches_a_btreemap_with_snapshots() {
    for seed in 1..=6u64 {
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut pool = Pool::new(MemDevice::new(20_000), 7, 1);
        let mut tree = Tree::new();
        let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut snaps: Vec<(Snapshot, Model)> = Vec::new();
        let space = [40, 400, 1500][(seed % 3) as usize];
        for step in 0..3000 {
            match r.below(10) {
                0..=5 => {
                    let (k, v) = (key(&mut r, space), val(&mut r));
                    assert_eq!(tree.insert(&mut pool, &k, &v).unwrap(), model.insert(k, v));
                }
                6..=8 => {
                    let k = key(&mut r, space);
                    assert_eq!(tree.delete(&mut pool, &k).unwrap(), model.remove(&k));
                }
                _ => {
                    let s = tree.commit(&mut pool);
                    snaps.push((s, model.clone()));
                    if snaps.len() > 3 {
                        let (old, _) = snaps.remove(r.below(snaps.len() as u64) as usize);
                        Tree::release(&mut pool, &old);
                        pool.reclaim();
                    }
                }
            }
            if step % 150 == 0 {
                let v = tree.verify(&mut pool).unwrap();
                assert_eq!(v.entries, model.len());
                let all = tree.range(&mut pool, None, None).unwrap();
                assert!(all.iter().map(|(k, v)| (k, v)).eq(model.iter()));
                // a random sub-range
                let (a, b) = (key(&mut r, space), key(&mut r, space));
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                let got = tree.range(&mut pool, Some(&lo), Some(&hi)).unwrap();
                let want: Vec<_> = model
                    .range(lo.clone()..hi.clone())
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                assert_eq!(got, want);
                // every retained snapshot still reads exactly what it had
                for (s, m) in &snaps {
                    let vs = Tree::verify_at(&mut pool, s.root, s.gen).unwrap();
                    assert_eq!(vs.entries, m.len());
                    // FI2: nothing a snapshot (or the live tree) reaches is on the free list
                    for a in &vs.reachable {
                        assert!(
                            !pool.free_list().contains(a),
                            "snapshot block {a} was freed"
                        );
                    }
                    let got = Tree::range_at(&mut pool, s.root, None, None).unwrap();
                    assert!(got.iter().map(|(k, v)| (k, v)).eq(m.iter()));
                }
                for a in &v.reachable {
                    assert!(!pool.free_list().contains(a), "live block {a} was freed");
                }
            }
        }
        for (k, v) in &model {
            assert_eq!(tree.get(&mut pool, k).unwrap().as_ref(), Some(v));
        }
        // drain: delete everything, release everything, and the tree is empty and the space comes back
        for k in model.keys().cloned().collect::<Vec<_>>() {
            tree.delete(&mut pool, &k).unwrap();
        }
        assert!(tree.root().is_none());
        for (s, _) in snaps.drain(..) {
            Tree::release(&mut pool, &s);
        }
        pool.reclaim();
        assert_eq!(pool.retired_blocks(), 0, "seed {seed}");
        // nothing leaked: every block ever handed out is back on the free list
        assert_eq!(
            pool.free_blocks() as u64,
            pool.high_water() - 1,
            "seed {seed}"
        );
    }
}

#[test]
fn bad_keys_and_values_are_refused() {
    let mut pool = Pool::new(MemDevice::new(100), 1, 1);
    let mut t = Tree::new();
    assert!(t.insert(&mut pool, b"", b"v").is_err());
    assert!(t.insert(&mut pool, &[1; fsfmt::MAX_KEY + 1], b"v").is_err());
    assert!(t
        .insert(&mut pool, b"k", &[0; fsfmt::MAX_VALUE + 1])
        .is_err());
    assert!(t
        .insert(&mut pool, &[1; fsfmt::MAX_KEY], &[0; fsfmt::MAX_VALUE])
        .is_ok());
}

#[test]
fn a_flipped_bit_is_caught_by_the_check_field() {
    let mut pool = Pool::new(MemDevice::new(1000), 1, 1);
    let mut t = Tree::new();
    for i in 0..200u32 {
        t.insert(&mut pool, &i.to_be_bytes(), &[7; 200]).unwrap();
    }
    let v = t.verify(&mut pool).unwrap();
    let victim = v.reachable[v.reachable.len() / 2];
    pool.dev.raw_mut(victim).unwrap()[100] ^= 1;
    assert!(t.verify(&mut pool).is_err());
}
