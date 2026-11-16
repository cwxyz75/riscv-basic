//! A device that fails (review findings 1, 2, 3 and 7 of reviews/2026-11-16-doofs-slice.md): a read error at mount is not a
//! reason to fall back to an older commit, repeated failed commits do not wear through the older slots, and the tree survives
//! long keys and big values.

use fsfmt::superblock::{slot_addr, Superblock, SLOTS};
use fsfmt::{mkfs, mount, DevError, Device, MemDevice, Tree, VolError, BLOCK, MAX_KEY, MAX_VALUE};
use std::collections::BTreeMap;

struct Faulty {
    inner: MemDevice,
    /// Reads of this block fail.
    fail_read: Option<u64>,
    /// Writes to a superblock slot tear (half lands) and fail.
    tear_slots: bool,
}

impl Device for Faulty {
    fn blocks(&self) -> u64 {
        self.inner.blocks()
    }
    fn read(&mut self, a: u64, b: &mut [u8; BLOCK]) -> Result<(), DevError> {
        if self.fail_read == Some(a) {
            return Err(DevError::Io);
        }
        self.inner.read(a, b)
    }
    fn write(&mut self, a: u64, d: &[u8; BLOCK]) -> Result<(), DevError> {
        let n = self.inner.blocks();
        if self.tear_slots && (0..SLOTS).any(|i| slot_addr(i, n) == a) {
            let mut old = [0u8; BLOCK];
            self.inner.read(a, &mut old).unwrap();
            old[..BLOCK / 2].copy_from_slice(&d[..BLOCK / 2]);
            self.inner.write(a, &old).unwrap();
            return Err(DevError::Io);
        }
        self.inner.write(a, d)
    }
    fn flush(&mut self) -> Result<(), DevError> {
        self.inner.flush()
    }
}

fn volume() -> fsfmt::Volume<Faulty> {
    let mut d = MemDevice::new(400);
    mkfs(&mut d, *b"faulty-volume-id", "f").unwrap();
    mount(Faulty {
        inner: d,
        fail_read: None,
        tear_slots: false,
    })
    .unwrap()
}

#[test]
fn a_read_error_at_mount_is_an_error_not_a_rollback() {
    let mut v = volume();
    v.tree.insert(&mut v.pool, b"a", b"1").unwrap();
    v.commit().unwrap();
    v.tree.insert(&mut v.pool, b"b", b"2").unwrap();
    v.commit().unwrap();
    let root = v.last.root.unwrap().addr;
    let mut d = v.into_device();
    d.fail_read = Some(root);
    assert!(
        matches!(mount(d), Err(VolError::Dev(DevError::Io))),
        "the newest commit is not silently dropped"
    );
}

#[test]
fn failed_commits_do_not_wear_through_the_older_slots() {
    let mut v = volume();
    v.tree.insert(&mut v.pool, b"keep", b"me").unwrap();
    v.commit().unwrap();
    v.pool.dev.tear_slots = true;
    for i in 0..20u8 {
        v.tree.insert(&mut v.pool, &[b'x', i], b"y").unwrap();
        assert!(v.commit().is_err());
    }
    v.pool.dev.tear_slots = false;
    // the machine dies here: the volume mounts at the last good commit
    let mut m = mount(v.into_device()).unwrap();
    assert_eq!(
        m.tree.get(&mut m.pool, b"keep").unwrap(),
        Some(b"me".to_vec())
    );
}

#[test]
fn a_hostile_superblock_generation_is_refused() {
    let s = Superblock {
        uuid: [1; 16],
        label: [0; 32],
        generation: u64::MAX,
        blocks: 100,
        root: None,
    };
    assert!(Superblock::decode(&s.encode()).is_err());
}

/// Long keys and big values, with deletes in runs, against a model: nothing panics and every tree verifies.
#[test]
fn long_keys_and_big_values_survive_splits_and_merges() {
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for _round in 0..4 {
        let mut d = MemDevice::new(6000);
        mkfs(&mut d, [7; 16], "l").unwrap();
        let mut v = mount(d).unwrap();
        let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let key = |n: u64| {
            let mut k = vec![0u8; 100 + (n % 7) as usize * 70];
            k[..8].copy_from_slice(&n.to_be_bytes());
            if n.is_multiple_of(3) {
                k.truncate(MAX_KEY);
                k.resize(MAX_KEY, 0);
            }
            k
        };
        for step in 0..2500u64 {
            let n = next() % 400;
            if next() % 3 != 0 {
                let val = vec![(next() & 0xff) as u8; (next() % (MAX_VALUE as u64 + 1)) as usize];
                v.tree.insert(&mut v.pool, &key(n), &val).unwrap();
                model.insert(key(n), val);
            } else {
                // a run of deletes
                for m in n..(n + 1 + next() % 12) {
                    assert_eq!(
                        v.tree.delete(&mut v.pool, &key(m)).unwrap(),
                        model.remove(&key(m))
                    );
                }
            }
            if step % 250 == 0 {
                assert_eq!(v.tree.verify(&mut v.pool).unwrap().entries, model.len());
                v.commit().unwrap();
            }
        }
        let all = v.tree.range(&mut v.pool, None, None).unwrap();
        assert!(all.iter().map(|(k, x)| (k, x)).eq(model.iter()));
        let _ = Tree::new();
    }
}
