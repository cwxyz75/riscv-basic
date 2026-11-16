//! Regression tests for reviews/2026-11-16-doofs-slice.md: a symlink is made whole or not at all at every fill level, a
//! damaged volume with a parent cycle neither hangs a walk nor passes fsck, and values read from disk never overflow.

use doofs::{fsck::check_fs, Fs, FsError};
use fsfmt::{mkfs, mount, MemDevice};
use memfs::{Error, Limits};

fn limits() -> Limits {
    Limits {
        max_nodes: 100_000,
        max_bytes: 1 << 30,
        max_file: 1 << 30,
        max_meta: 1 << 30,
    }
}

fn fs(blocks: u64) -> Fs<MemDevice> {
    let mut d = MemDevice::new(blocks);
    mkfs(&mut d, [4; 16], "review").unwrap();
    Fs::open(mount(d).unwrap(), limits()).unwrap()
}

#[test]
fn a_symlink_is_whole_or_absent_at_every_fill_level() {
    for fill in 0..70u64 {
        let mut f = fs(200);
        let a = f.create_file(0, b"a").unwrap();
        // fill most of the volume, leaving a varying amount
        let mut off = 0;
        while f.vol.pool.free_blocks() as u64 > 64 + fill && f.write(a, off, &[1u8; 4096]).is_ok() {
            off += 4096;
        }
        match f.symlink(0, b"l", b"a", 0) {
            Ok(id) => {
                assert_eq!(f.read_link(id).unwrap(), b"a", "fill {fill}");
            }
            Err(_) => {
                assert_eq!(
                    f.lookup(0, b"l"),
                    Err(FsError::Fs(Error::NotFound)),
                    "fill {fill}: no half-made link"
                );
            }
        }
        let p = check_fs(&mut f);
        assert!(p.is_empty(), "fill {fill}: {p:?}");
    }
}

fn inode_key(id: u64) -> Vec<u8> {
    let mut k = vec![1u8];
    k.extend_from_slice(&id.to_be_bytes());
    k
}

/// Two directories that name each other as parent: walks end, fsck reports it.
#[test]
fn a_parent_cycle_ends_every_walk_and_fails_fsck() {
    let mut f = fs(200);
    let a = f.mkdir(0, b"a").unwrap();
    let b = f.mkdir(a, b"b").unwrap();
    f.create_file(b, b"leaf").unwrap();
    // damage: a's parent becomes b (b's already is a)
    let mut v = f
        .vol
        .tree
        .get(&mut f.vol.pool, &inode_key(a))
        .unwrap()
        .unwrap();
    v[8..16].copy_from_slice(&b.to_be_bytes());
    f.vol
        .tree
        .insert(&mut f.vol.pool, &inode_key(a), &v)
        .unwrap();
    let p = check_fs(&mut f);
    assert!(p.iter().any(|m| m.contains("parent chain")), "{p:?}");
    // none of these loops forever (each returns an error or a result)
    let _ = f.depth_below(b, 0);
    let _ = f.rename(0, b"a", b, b"moved");
    let _ = f.query(
        0,
        b"name",
        &doofs::Term::Range { lo: None, hi: None },
        None,
        10,
    );
}

/// Hostile numbers in records: operations answer with errors, never an arithmetic panic.
#[test]
fn hostile_records_do_not_overflow() {
    let mut f = fs(200);
    let x = f.create_file(0, b"x").unwrap();
    // an inode whose size and attribute cost are absurd, and a node id at the top of the range
    let mut v = f
        .vol
        .tree
        .get(&mut f.vol.pool, &inode_key(x))
        .unwrap()
        .unwrap();
    v[24..32].copy_from_slice(&u64::MAX.to_be_bytes());
    v[36..40].copy_from_slice(&u32::MAX.to_be_bytes());
    f.vol
        .tree
        .insert(&mut f.vol.pool, &inode_key(x), &v)
        .unwrap();
    let _ = check_fs(&mut f);
    let _ = f.read(x, 0, 10);
    let _ = f.set_size(x, 0);
    let _ = f.unlink(0, b"x");
    let _ = f.attr_list(u64::MAX);
    let _ = f.read_dir(u64::MAX, None, 10);
    let _ = f.attr_get(u64::MAX, b"k");
}
