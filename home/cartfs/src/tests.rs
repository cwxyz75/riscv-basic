//! Expected bytes and sector numbers below are worked out by hand from the layout
//! in the crate docs, not read back from the code.
use super::*;
use alloc::string::ToString;

fn fresh() -> Disk<MemDisk> {
    Disk::format(MemDisk::new(4096)).unwrap()
}

fn sector(d: &Disk<MemDisk>, n: usize) -> &[u8] {
    &d.dev.bytes[n * SECTOR..(n + 1) * SECTOR]
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

#[test]
fn format_layout() {
    let d = fresh();
    let h = sector(&d, 0);
    assert_eq!(&h[..8], b"RVDISK01");
    assert_eq!(&h[8..12], &[0x00, 0x10, 0, 0]); // 4096 = 0x1000
    assert_eq!(&h[12..16], &[64, 0, 0, 0]);
    // sectors 0..=5 are in use: bits 0-5 of byte 0
    let m = sector(&d, 1);
    assert_eq!(m[0], 0b0011_1111);
    assert!(m[1..].iter().all(|&b| b == 0));
    // directory is empty
    assert!(sector(&d, 2).iter().all(|&b| b == 0));
}

#[test]
fn small_disk_marks_missing_sectors_used() {
    // 16 sectors: sectors 16.. do not exist, so they are marked used
    let d = Disk::format(MemDisk::new(16)).unwrap();
    let m = sector(&d, 1);
    assert_eq!(m[0], 0b0011_1111);
    assert_eq!(m[1], 0); // sectors 8-15 free
    assert_eq!(m[2], 0xFF); // sectors 16-23 beyond the end
    let mut d = d;
    assert_eq!(d.free_sectors().unwrap(), 10);
}

#[test]
fn save_one_file_layout() {
    let mut d = fresh();
    let data = pattern(1000); // 510 + 490 bytes: two sectors, 6 and 7
    d.save("hello", Kind::Basic, &data).unwrap();
    let e = &sector(&d, 2)[..32];
    assert_eq!(&e[..5], b"HELLO");
    assert!(e[5..16].iter().all(|&b| b == 0));
    assert_eq!(e[16], 1); // Basic
    assert_eq!(&e[18..20], &[6, 0]);
    assert_eq!(&e[20..24], &[0xE8, 0x03, 0, 0]); // 1000
    let s6 = sector(&d, 6);
    assert_eq!(&s6[..2], &[7, 0]);
    assert_eq!(&s6[2..], &data[..510]);
    let s7 = sector(&d, 7);
    assert_eq!(&s7[..2], &[0, 0]);
    assert_eq!(&s7[2..492], &data[510..]);
    assert!(s7[492..].iter().all(|&b| b == 0));
    assert_eq!(sector(&d, 1)[0], 0xFF); // sectors 0-7 used
    assert_eq!(d.load("HELLO", Kind::Basic).unwrap(), data);
}

#[test]
fn empty_file_takes_one_sector() {
    let mut d = fresh();
    d.save("e", Kind::Logo, b"").unwrap();
    assert_eq!(sector(&d, 2)[16], 2);
    assert_eq!(sector(&d, 1)[0], 0b0111_1111);
    assert_eq!(d.load("e", Kind::Logo).unwrap(), b"");
    assert_eq!(d.free_sectors().unwrap(), 4090 - 1);
}

#[test]
fn exact_sector_boundaries() {
    let mut d = fresh();
    for n in [509, 510, 511, 1020, 1021] {
        let data = pattern(n);
        d.save("b", Kind::Basic, &data).unwrap();
        assert_eq!(d.load("b", Kind::Basic).unwrap(), data, "{n}");
    }
    // 510 bytes is exactly one sector, 511 needs two
    d.save("b", Kind::Basic, &pattern(510)).unwrap();
    assert_eq!(d.free_sectors().unwrap(), 4090 - 1);
    d.save("b", Kind::Basic, &pattern(511)).unwrap();
    assert_eq!(d.free_sectors().unwrap(), 4090 - 2);
}

#[test]
fn second_file_and_directory_order() {
    let mut d = fresh();
    d.save("zeta", Kind::Basic, &pattern(100)).unwrap(); // sector 6
    d.save("alpha", Kind::Logo, &pattern(100)).unwrap(); // sector 7
    assert_eq!(&sector(&d, 2)[32 + 18..32 + 20], &[7, 0]); // entry 1
    let dir = d.dir().unwrap();
    // DIR lists sorted by name
    assert_eq!(
        dir,
        [
            DirEntry {
                name: "ALPHA".to_string(),
                kind: Kind::Logo,
                len: 100
            },
            DirEntry {
                name: "ZETA".to_string(),
                kind: Kind::Basic,
                len: 100
            },
        ]
    );
}

#[test]
fn freed_sectors_are_reused_and_chains_fragment() {
    let mut d = fresh();
    d.save("a", Kind::Basic, &pattern(1000)).unwrap(); // 6,7
    d.save("b", Kind::Basic, &pattern(100)).unwrap(); // 8
    d.delete("a", Kind::Basic).unwrap();
    assert_eq!(sector(&d, 1)[0], 0b0011_1111); // 6,7 freed: bits 6,7 clear
    assert_eq!(sector(&d, 1)[1], 0b0000_0001); // sector 8 still used
    assert_eq!(sector(&d, 2)[16], 0); // entry freed
                                      // three sectors: takes 6, 7, then 9 (8 is b's)
    let big = pattern(1500);
    d.save("c", Kind::Logo, &big).unwrap();
    assert_eq!(&sector(&d, 6)[..2], &[7, 0]);
    assert_eq!(&sector(&d, 7)[..2], &[9, 0]);
    assert_eq!(&sector(&d, 9)[..2], &[0, 0]);
    assert_eq!(d.load("c", Kind::Logo).unwrap(), big);
    assert_eq!(d.load("b", Kind::Basic).unwrap(), pattern(100));
}

#[test]
fn replace_reuses_own_sectors_and_keeps_entry_slot() {
    let mut d = fresh();
    d.save("a", Kind::Basic, &pattern(1000)).unwrap(); // 6,7
    d.save("b", Kind::Basic, &pattern(10)).unwrap(); // 8
    d.save("a", Kind::Basic, &pattern(600)).unwrap(); // replaces: 6,7 again
    assert_eq!(&sector(&d, 2)[..1], b"A"); // same directory slot
    assert_eq!(&sector(&d, 2)[18..20], &[6, 0]);
    assert_eq!(d.dir().unwrap().len(), 2);
    assert_eq!(d.load("a", Kind::Basic).unwrap(), pattern(600));
    // replace with a smaller file frees the tail
    d.save("a", Kind::Basic, &pattern(10)).unwrap();
    assert_eq!(sector(&d, 1)[0], 0b0111_1111); // 0-6 used, 7 freed
    assert_eq!(sector(&d, 1)[1], 0b0000_0001); // 8 is b's
}

#[test]
fn names_are_case_insensitive_and_validated() {
    let mut d = fresh();
    d.save("Mixed.Case-1_x", Kind::Basic, b"x").unwrap();
    assert_eq!(d.load("MIXED.CASE-1_X", Kind::Basic).unwrap(), b"x");
    assert_eq!(d.dir().unwrap()[0].name, "MIXED.CASE-1_X");
    assert_eq!(d.save("", Kind::Basic, b""), Err(Error::BadName));
    assert_eq!(d.save("has space", Kind::Basic, b""), Err(Error::BadName));
    assert_eq!(d.save("q\"uote", Kind::Basic, b""), Err(Error::BadName));
    assert_eq!(d.save("Ünï", Kind::Basic, b""), Err(Error::BadName));
    d.save("EXACTLY16CHARS_AB", Kind::Basic, b"").unwrap_err(); // 17 chars
    d.save("EXACTLY16CHARS_A", Kind::Basic, b"").unwrap(); // 16 chars
    assert_eq!(d.load("nothere", Kind::Basic), Err(Error::NotFound));
}

#[test]
fn kind_mismatch() {
    let mut d = fresh();
    d.save("p", Kind::Basic, b"10 END").unwrap();
    assert_eq!(d.load("p", Kind::Logo), Err(Error::TypeMismatch));
    assert_eq!(d.save("p", Kind::Logo, b"x"), Err(Error::TypeMismatch));
    assert_eq!(d.delete("p", Kind::Logo), Err(Error::TypeMismatch));
    assert_eq!(d.load("p", Kind::Basic).unwrap(), b"10 END");
}

#[test]
fn directory_full_at_64_files() {
    let mut d = fresh();
    for i in 0..64 {
        d.save(&alloc::format!("F{i}"), Kind::Basic, b"x").unwrap();
    }
    assert_eq!(d.save("ONEMORE", Kind::Basic, b"x"), Err(Error::Full));
    // replacing an existing one still works
    d.save("F5", Kind::Basic, b"yy").unwrap();
    // deleting makes room
    d.delete("F0", Kind::Basic).unwrap();
    d.save("ONEMORE", Kind::Basic, b"x").unwrap();
    // the freed slot (entry 0) was reused
    assert_eq!(&sector(&d, 2)[..7], b"ONEMORE");
}

#[test]
fn disk_full_leaves_disk_unchanged() {
    // 16 sectors: 10 data sectors = 5100 bytes at most
    let mut d = Disk::format(MemDisk::new(16)).unwrap();
    d.save("a", Kind::Basic, &pattern(4000)).unwrap(); // 8 sectors, 2 left
    let before = d.dev.bytes.clone();
    assert_eq!(d.save("b", Kind::Basic, &pattern(1100)), Err(Error::Full)); // needs 3
    assert_eq!(d.dev.bytes, before);
    // replacing "a" by something that fits only because a's sectors are freed
    d.save("a", Kind::Basic, &pattern(5000)).unwrap(); // 10 sectors
    assert_eq!(d.free_sectors().unwrap(), 0);
    assert_eq!(d.load("a", Kind::Basic).unwrap(), pattern(5000));
    // too big even for an empty disk
    assert_eq!(d.save("a", Kind::Basic, &pattern(5200)), Err(Error::Full));
    assert_eq!(d.load("a", Kind::Basic).unwrap(), pattern(5000)); // old copy intact
}

#[test]
fn too_big() {
    let mut d = fresh();
    d.save("max", Kind::Basic, &pattern(MAX_FILE)).unwrap();
    assert_eq!(d.load("max", Kind::Basic).unwrap().len(), MAX_FILE);
    assert_eq!(
        d.save("max2", Kind::Basic, &pattern(MAX_FILE + 1)),
        Err(Error::TooBig)
    );
}

#[test]
fn mount_remembers_everything() {
    let mut d = fresh();
    d.save("keep", Kind::Logo, &pattern(777)).unwrap();
    let mut d2 = Disk::mount(d.into_device()).unwrap();
    assert_eq!(d2.load("keep", Kind::Logo).unwrap(), pattern(777));
}

#[test]
fn mount_rejects_unformatted_and_corrupt() {
    assert_eq!(
        Disk::mount(MemDisk::new(4096)).err(),
        Some(Error::NotFormatted)
    );
    assert_eq!(
        Disk::mount(MemDisk::new(2)).err(),
        Some(Error::NotFormatted)
    );
    let mut d = fresh();
    d.dev.bytes[8..12].copy_from_slice(&5000u32.to_le_bytes()); // more than the device has
    assert_eq!(Disk::mount(d.into_device()).err(), Some(Error::Corrupt));
}

#[test]
fn corrupt_chains_are_errors_not_hangs() {
    let mut d = fresh();
    d.save("loop", Kind::Basic, &pattern(1000)).unwrap(); // 6,7
                                                          // make sector 7 point back at 6
    d.dev.bytes[7 * SECTOR] = 6;
    assert_eq!(d.load("loop", Kind::Basic), Err(Error::Corrupt));
    // a chain that ends early
    let mut d = fresh();
    d.save("short", Kind::Basic, &pattern(1000)).unwrap();
    d.dev.bytes[6 * SECTOR] = 0;
    assert_eq!(d.load("short", Kind::Basic), Err(Error::Corrupt));
    // first sector inside the reserved area
    let mut d = fresh();
    d.save("bad", Kind::Basic, b"x").unwrap();
    d.dev.bytes[2 * SECTOR + 18] = 3;
    assert_eq!(d.load("bad", Kind::Basic), Err(Error::Corrupt));
}
