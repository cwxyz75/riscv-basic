use doofs::{fsck::check_fs, Fs};
use fsfmt::{mkfs, mount, MemDevice};
use memfs::Limits;

fn fresh() -> Fs<MemDevice> {
    let mut d = MemDevice::new(300);
    mkfs(&mut d, [5; 16], "fsck").unwrap();
    let mut f = Fs::open(mount(d).unwrap(), Limits::default()).unwrap();
    let dir = f.mkdir(0, b"d").unwrap();
    let a = f.create_file(dir, b"a").unwrap();
    f.write(a, 0, &[1u8; 5000]).unwrap();
    f.symlink(dir, b"l", b"a", 0).unwrap();
    f
}

fn key(tag: u8, id: u64, rest: &[u8]) -> Vec<u8> {
    let mut k = vec![tag];
    k.extend_from_slice(&id.to_be_bytes());
    k.extend_from_slice(rest);
    k
}

#[test]
fn a_consistent_filesystem_has_no_problems() {
    let mut f = fresh();
    assert_eq!(check_fs(&mut f), Vec::<String>::new());
}

#[test]
fn each_kind_of_damage_is_reported() {
    // an entry that names a node that does not exist
    let mut f = fresh();
    f.vol
        .tree
        .insert(&mut f.vol.pool, &key(2, 0, b"ghost"), &999u64.to_be_bytes())
        .unwrap();
    assert!(check_fs(&mut f).iter().any(|p| p.contains("missing node")));
    // an orphan record for a linked node
    let mut f = fresh();
    f.vol
        .tree
        .insert(&mut f.vol.pool, &key(5, 1, b""), b"")
        .unwrap();
    assert!(check_fs(&mut f).iter().any(|p| p.contains("orphan record")));
    // an extent past the end of its file
    let mut f = fresh();
    f.vol
        .tree
        .insert(
            &mut f.vol.pool,
            &key(3, 2, &77u64.to_be_bytes()),
            &[0u8; 32],
        )
        .unwrap();
    assert!(check_fs(&mut f).iter().any(|p| p.contains("past its size")));
    // a directory that lost an entry without its size changing
    let mut f = fresh();
    f.vol
        .tree
        .delete(&mut f.vol.pool, &key(2, 1, b"l"))
        .unwrap();
    let p = check_fs(&mut f);
    assert!(p.iter().any(|p| p.contains("entries")), "{p:?}");
    assert!(
        p.iter().any(|p| p.contains("linked but named 0 times")),
        "{p:?}"
    );
    // counters that do not add up
    let mut f = fresh();
    f.vol
        .tree
        .insert(&mut f.vol.pool, &[6], &[0u8; 24])
        .unwrap();
    let p = check_fs(&mut f);
    assert!(p.iter().any(|p| p.contains("counters")), "{p:?}");
}
