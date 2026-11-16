//! A full volume: a write that cannot be done leaves nothing half done, deleting still works (the reserve), and the space comes
//! back after a commit.

use doofs::{Fs, FsError, RESERVE};
use fsfmt::{check, mkfs, mount, verify_all, MemDevice};
use memfs::{Error, Limits};

fn limits() -> Limits {
    Limits {
        max_nodes: 10_000,
        max_bytes: 1 << 30,
        max_file: 1 << 30,
        max_meta: 1 << 30,
    }
}

fn fs(blocks: u64) -> Fs<MemDevice> {
    let mut d = MemDevice::new(blocks);
    mkfs(&mut d, [9; 16], "space").unwrap();
    Fs::open(mount(d).unwrap(), limits()).unwrap()
}

#[test]
fn a_full_volume_refuses_cleanly_keeps_its_data_and_frees_on_delete() {
    let mut f = fs(300);
    let a = f.create_file(0, b"a").unwrap();
    let b = f.create_file(0, b"b").unwrap();
    let mut off = 0u64;
    let chunk = vec![0xA5u8; 4096];
    let err = loop {
        match f.write(a, off, &chunk) {
            Ok(_) => off += 4096,
            Err(e) => break e,
        }
    };
    assert_eq!(err, FsError::NoSpace);
    assert!(
        off > 100 * 4096,
        "it filled a good part of the volume first: {off}"
    );
    // nothing half done: the size, the counters and the data are those of the last good write
    assert_eq!(f.size(a).unwrap(), off);
    assert_eq!(f.counters().bytes, off);
    assert_eq!(f.read(a, off - 4096, 4096).unwrap(), chunk);
    // another file cannot grow either, and creating one is refused too (ordinary writes do not touch the reserve)
    assert_eq!(f.write(b, 0, &chunk), Err(FsError::NoSpace));
    assert_eq!(f.size(b).unwrap(), 0);
    // use up every block the ordinary paths may still take, then make everything durable: nothing is left to reclaim
    for i in 0..5000u32 {
        if f.create_file(0, format!("pad{i}").as_bytes()).is_err() {
            break;
        }
    }
    f.commit().unwrap();
    // deleting works on the full volume (it runs on the reserve), and then the space is there again
    f.unlink(0, b"a").unwrap();
    assert!(f.write(b, 0, &chunk).is_ok(), "space came back");
    f.commit().unwrap();
    let mut vol = f.into_volume();
    let v = verify_all(&mut vol.pool, vol.last.root, vol.last.gen).unwrap();
    assert!(v.entries > 0);
    let rep = check(&mut vol.into_device());
    assert!(rep.problems.is_empty(), "{:?}", rep.problems);
}

#[test]
fn a_failed_operation_leaves_the_tree_as_it_was() {
    let mut f = fs(90);
    let a = f.create_file(0, b"a").unwrap();
    let big = vec![1u8; 4096 * 5];
    let mut n = 0;
    while f.write(a, n, &big).is_ok() {
        n += big.len() as u64;
    }
    // the volume is full of data; creating files now fails part way (inode, entry, counters are several writes)
    let mut refused = 0;
    for i in 0..3000u32 {
        if refused >= 3 {
            break;
        }
        let name = format!("f{i}");
        let (counters, root, free) = (f.counters(), f.vol.tree.root(), f.vol.pool.free_blocks());
        match f.create_file(0, name.as_bytes()) {
            Ok(_) => {}
            Err(FsError::NoSpace) => {
                refused += 1;
                assert_eq!(f.counters(), counters, "counters back");
                assert_eq!(f.vol.tree.root(), root, "root back");
                // (the retry commit may have reclaimed blocks: more free is fine, fewer is a leak)
                assert!(f.vol.pool.free_blocks() >= free, "a block was lost");
                assert_eq!(
                    f.lookup(0, name.as_bytes()),
                    Err(FsError::Fs(Error::NotFound))
                );
            }
            Err(e) => panic!("{e:?}"),
        }
    }
    assert!(refused > 0, "the volume did fill up");
    f.vol.tree.verify(&mut f.vol.pool).unwrap();
    // no block is free twice, and none that is free is reachable
    let mut free = f.vol.pool.free_list().to_vec();
    free.sort_unstable();
    assert!(
        free.windows(2).all(|w| w[0] != w[1]),
        "a block is on the free list twice"
    );
    let gen = f.vol.pool.open_gen;
    let v = verify_all(&mut f.vol.pool, f.vol.tree.root(), gen).unwrap();
    assert!(
        v.reachable.iter().all(|a| free.binary_search(a).is_err()),
        "a reachable block is free"
    );
    f.unlink(0, b"a").unwrap();
    f.commit().unwrap();
    assert_eq!(f.counters().bytes, 0);
    assert_eq!(f.lookup(0, b"a"), Err(FsError::Fs(Error::NotFound)));
    assert!(f.create_file(0, b"again").is_ok());
    assert_eq!(RESERVE, 64);
}

#[test]
fn an_open_unlinked_file_is_reaped_at_the_next_mount() {
    let mut f = fs(200);
    let a = f.create_file(0, b"a").unwrap();
    f.write(a, 0, &vec![7u8; 20_000]).unwrap();
    f.retain(a).unwrap();
    f.unlink(0, b"a").unwrap();
    // still readable through the open handle
    assert_eq!(f.read(a, 19_999, 10).unwrap(), vec![7]);
    assert_eq!(f.counters().nodes, 2);
    f.commit().unwrap();
    // "crash" with the file open: the next mount reaps it and frees its blocks
    let dev = f.into_volume().into_device();
    let mut f = Fs::open(mount(dev).unwrap(), limits()).unwrap();
    assert_eq!(f.counters().nodes, 1);
    assert_eq!(f.counters().bytes, 0);
    assert_eq!(f.kind(a), Err(FsError::Fs(Error::NotFound)));
    f.commit().unwrap();
    let mut vol = f.into_volume();
    let v = verify_all(&mut vol.pool, vol.last.root, vol.last.gen).unwrap();
    assert_eq!(
        v.entries, 3,
        "the root inode, the counters and the next-id record: {v:?}"
    );
}

#[test]
fn the_byte_and_file_limits_are_exact() {
    let l = Limits {
        max_nodes: 100,
        max_bytes: 10_000,
        max_file: 8_000,
        max_meta: 1 << 20,
    };
    let mut d = MemDevice::new(400);
    mkfs(&mut d, [3; 16], "q").unwrap();
    let mut f = Fs::open(mount(d).unwrap(), l).unwrap();
    let (a, b) = (
        f.create_file(0, b"a").unwrap(),
        f.create_file(0, b"b").unwrap(),
    );
    assert_eq!(f.write(a, 0, &vec![1; 8_000]), Ok(8_000));
    assert_eq!(
        f.write(a, 8_000, &[1]),
        Err(FsError::Fs(Error::TooBig)),
        "one past max_file"
    );
    assert_eq!(f.set_size(a, 8_001), Err(FsError::Fs(Error::TooBig)));
    assert_eq!(
        f.write(b, 0, &vec![2; 2_000]),
        Ok(2_000),
        "exactly max_bytes in all"
    );
    assert_eq!(
        f.write(b, 2_000, &[2]),
        Err(FsError::Fs(Error::Quota)),
        "one past max_bytes"
    );
    assert_eq!(f.set_size(b, 2_001), Err(FsError::Fs(Error::Quota)));
    f.set_size(a, 7_999).unwrap();
    assert_eq!(f.write(b, 2_000, &[2]), Ok(1), "a freed byte is usable");
}
