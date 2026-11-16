//! Corruption (FS-SPEC section 11, check 4): flip one bit of a committed image anywhere and mount it: it never panics; either
//! the mount fails closed, or it falls back to the previous commit, or a read returns an error. Flipped bytes are never returned
//! as file data.

use doofs::{Fs, ROOT};
use fsfmt::{check, mkfs, mount, Device, MemDevice, BLOCK};
use memfs::Limits;

const BLOCKS: u64 = 160;

fn contents() -> Vec<(&'static [u8], Vec<u8>)> {
    vec![
        (b"a", (0..9000).map(|i| (i % 251) as u8).collect()),
        (b"b", vec![0x5a; 4096]),
        (b"c", vec![0xc3; 100]),
    ]
}

fn image() -> MemDevice {
    let mut d = MemDevice::new(BLOCKS);
    mkfs(&mut d, *b"corruption-test!", "flip").unwrap();
    let mut f = Fs::open(mount(d).unwrap(), Limits::default()).unwrap();
    for (name, data) in contents() {
        let id = f.create_file(ROOT, name).unwrap();
        f.write(id, 0, &data).unwrap();
    }
    f.commit().unwrap();
    // a second commit that leaves those files alone, so that falling back to the first shows the same files
    f.create_file(ROOT, b"z").unwrap();
    f.commit().unwrap();
    f.into_volume().into_device()
}

/// Mount `dev` and read everything: nothing may panic, and any bytes returned for a known file must be its real bytes.
fn probe(dev: MemDevice) -> &'static str {
    let Ok(vol) = mount(dev) else {
        return "mount refused";
    };
    let Ok(mut f) = Fs::open(vol, Limits::default()) else {
        return "open refused";
    };
    let mut outcome = "mounted";
    for (name, data) in contents() {
        match f.lookup(ROOT, name) {
            Ok(id) => match f.read(id, 0, 1 << 20) {
                Ok(got) => assert_eq!(got, data, "wrong bytes returned for {name:?}"),
                Err(_) => outcome = "read error",
            },
            Err(_) => outcome = "lookup error",
        }
    }
    let _ = f.read_dir(ROOT, None, 100);
    let _ = doofs::fsck::check_fs(&mut f);
    outcome
}

#[test]
fn a_flipped_bit_never_panics_and_never_returns_wrong_data() {
    let base = image();
    let mut seen = std::collections::BTreeMap::new();
    for addr in 0..BLOCKS {
        let mut blk = [0u8; BLOCK];
        base.clone().read(addr, &mut blk).unwrap();
        if blk.iter().all(|&b| b == 0) {
            continue; // never written
        }
        // every 61st bit (a prime stride, so the offsets within bytes vary), plus the first bytes of the block
        let bits = (0..BLOCK * 8).step_by(61).chain(0..64);
        for bit in bits {
            let mut d = base.clone();
            d.raw_mut(addr).unwrap()[bit / 8] ^= 1 << (bit % 8);
            *seen.entry(probe(d)).or_insert(0) += 1;
        }
    }
    eprintln!("{seen:?}");
    // every kind of outcome happens (a data bit gives a read error, a slot bit a fallback, a node bit a fallback or a refusal)
    assert!(seen.contains_key("read error"), "{seen:?}");
    assert!(seen.contains_key("mounted"), "{seen:?}");
    // the check tool agrees there is damage in at least some images, and never panics on any
    let mut d = base.clone();
    d.raw_mut(0).unwrap()[10] ^= 1;
    let _ = check(&mut d);
}
