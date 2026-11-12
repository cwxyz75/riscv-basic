//! memfs: hand-derived scenarios for every error, and a random walk against an independent
//! reference model (a path-keyed map written separately from the implementation).

use memfs::{Error, Fs, Kind, Limits, NodeId, ROOT};
use std::collections::BTreeMap;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn small() -> Limits {
    Limits {
        max_nodes: 6,
        max_bytes: 100,
        max_file: 60,
    }
}

#[test]
fn names_are_validated() {
    let mut fs = Fs::new(Limits::default());
    for bad in [&b""[..], b".", b"..", b"a/b", b"a\0", &[b'x'; 256][..]] {
        assert_eq!(fs.create_file(ROOT, bad), Err(Error::BadName), "{bad:?}");
        assert_eq!(fs.mkdir(ROOT, bad), Err(Error::BadName));
        assert_eq!(fs.lookup(ROOT, bad), Err(Error::BadName));
        assert_eq!(fs.unlink(ROOT, bad), Err(Error::BadName));
    }
    assert!(fs.create_file(ROOT, &[b'x'; 255]).is_ok());
    assert!(fs.create_file(ROOT, "é-ünï.txt".as_bytes()).is_ok());
}

#[test]
fn files_directories_and_their_errors() {
    let mut fs = Fs::new(Limits::default());
    let d = fs.mkdir(ROOT, b"d").unwrap();
    let f = fs.create_file(d, b"f").unwrap();
    assert_eq!(fs.kind(d), Ok(Kind::Dir));
    assert_eq!(fs.kind(f), Ok(Kind::File));
    assert_eq!(fs.mkdir(ROOT, b"d"), Err(Error::Exists));
    assert_eq!(fs.create_file(ROOT, b"d"), Err(Error::Exists));
    assert_eq!(fs.lookup(d, b"f"), Ok(f));
    assert_eq!(fs.lookup(d, b"g"), Err(Error::NotFound));
    assert_eq!(fs.lookup(f, b"x"), Err(Error::NotDirectory));
    assert_eq!(fs.create_file(f, b"x"), Err(Error::NotDirectory));
    assert_eq!(fs.walk(ROOT, &[b"d", b"f"]), Ok(f));
    assert_eq!(fs.walk(ROOT, &[b"d", b"f", b"x"]), Err(Error::NotDirectory));
    assert_eq!(fs.walk(ROOT, &[]), Ok(ROOT));
    assert_eq!(fs.unlink(ROOT, b"d"), Err(Error::NotEmpty));
    assert_eq!(fs.read(d, 0, 1), Err(Error::IsDirectory));
    assert_eq!(fs.write(d, 0, b"x"), Err(Error::IsDirectory));
    assert_eq!(fs.set_size(d, 1), Err(Error::IsDirectory));
    fs.unlink(d, b"f").unwrap();
    fs.unlink(ROOT, b"d").unwrap();
    assert_eq!(fs.lookup(ROOT, b"d"), Err(Error::NotFound));
    assert_eq!(fs.unlink(ROOT, b"d"), Err(Error::NotFound));
    assert_eq!(fs.node_count(), 1, "everything unlinked is freed");
}

#[test]
fn reads_writes_holes_and_truncation() {
    let mut fs = Fs::new(Limits::default());
    let f = fs.create_file(ROOT, b"f").unwrap();
    assert_eq!(fs.read(f, 0, 10), Ok(b"".to_vec()));
    assert_eq!(fs.write(f, 0, b"hello"), Ok(5));
    assert_eq!(fs.read(f, 0, 100), Ok(b"hello".to_vec()));
    assert_eq!(fs.read(f, 1, 3), Ok(b"ell".to_vec()));
    assert_eq!(fs.read(f, 5, 3), Ok(b"".to_vec()), "at the end");
    assert_eq!(fs.read(f, 99, 3), Ok(b"".to_vec()), "past the end");
    assert_eq!(
        fs.read(f, u64::MAX, 3),
        Ok(b"".to_vec()),
        "an absurd offset"
    );
    // a write past the end leaves a zero-filled gap
    assert_eq!(fs.write(f, 8, b"X"), Ok(1));
    assert_eq!(fs.read(f, 0, 100), Ok(b"hello\0\0\0X".to_vec()));
    assert_eq!(fs.size(f), Ok(9));
    assert_eq!(fs.bytes_used(), 9);
    // overwrite in the middle does not change the size
    assert_eq!(fs.write(f, 1, b"EY"), Ok(2));
    assert_eq!(fs.read(f, 0, 3), Ok(b"hEY".to_vec()));
    assert_eq!(fs.size(f), Ok(9));
    assert_eq!(
        fs.write(f, 3, b""),
        Ok(0),
        "an empty write is fine anywhere"
    );
    assert_eq!(fs.write(f, u64::MAX, b"x"), Err(Error::BadRange));
    assert_eq!(fs.write(f, u64::MAX - 1, b"xyz"), Err(Error::BadRange));
    fs.set_size(f, 3).unwrap();
    assert_eq!(fs.read(f, 0, 10), Ok(b"hEY".to_vec()));
    assert_eq!(fs.bytes_used(), 3, "shrinking gives the bytes back");
    fs.set_size(f, 6).unwrap();
    assert_eq!(fs.read(f, 0, 10), Ok(b"hEY\0\0\0".to_vec()));
    assert_eq!(fs.bytes_used(), 6);
}

#[test]
fn quotas_fail_cleanly_and_change_nothing() {
    let mut fs = Fs::new(small());
    let f = fs.create_file(ROOT, b"f").unwrap();
    // one file at most 60 bytes
    assert_eq!(fs.write(f, 0, &[1u8; 61]), Err(Error::TooBig));
    assert_eq!(fs.size(f), Ok(0), "nothing was written");
    assert_eq!(fs.write(f, 0, &[1u8; 60]), Ok(60));
    assert_eq!(fs.write(f, 60, b"x"), Err(Error::TooBig));
    assert_eq!(fs.set_size(f, 61), Err(Error::TooBig));
    // total 100 bytes: a second file can have 40
    let g = fs.create_file(ROOT, b"g").unwrap();
    assert_eq!(fs.write(g, 0, &[2u8; 41]), Err(Error::Quota));
    assert_eq!(fs.size(g), Ok(0));
    assert_eq!(fs.set_size(g, 41), Err(Error::Quota), "holes count");
    assert_eq!(fs.write(g, 0, &[2u8; 40]), Ok(40));
    assert_eq!(fs.bytes_used(), 100);
    // freeing bytes makes room again
    fs.set_size(f, 10).unwrap();
    assert_eq!(fs.write(g, 40, &[3u8; 20]), Ok(20));
    // nodes: root + f + g = 3 live, limit 6
    fs.create_file(ROOT, b"h").unwrap();
    fs.create_file(ROOT, b"i").unwrap();
    fs.create_file(ROOT, b"j").unwrap();
    assert_eq!(fs.node_count(), 6);
    assert_eq!(fs.create_file(ROOT, b"k"), Err(Error::Quota));
    assert_eq!(fs.mkdir(ROOT, b"k"), Err(Error::Quota));
    assert_eq!(
        fs.lookup(ROOT, b"k"),
        Err(Error::NotFound),
        "no half-created entry"
    );
    fs.unlink(ROOT, b"j").unwrap();
    fs.create_file(ROOT, b"k").unwrap();
    // deleting a file returns its bytes
    let before = fs.bytes_used();
    fs.unlink(ROOT, b"g").unwrap();
    assert_eq!(fs.bytes_used(), before - 60);
}

#[test]
fn unlinked_nodes_live_while_open_and_names_can_be_reused() {
    let mut fs = Fs::new(Limits::default());
    let f = fs.create_file(ROOT, b"f").unwrap();
    fs.write(f, 0, b"data").unwrap();
    fs.retain(f).unwrap();
    fs.unlink(ROOT, b"f").unwrap();
    assert_eq!(fs.lookup(ROOT, b"f"), Err(Error::NotFound));
    assert_eq!(
        fs.read(f, 0, 10),
        Ok(b"data".to_vec()),
        "still readable through the connection"
    );
    assert_eq!(fs.bytes_used(), 4, "and still counted");
    // the name is free again, with a different node
    let f2 = fs.create_file(ROOT, b"f").unwrap();
    assert_ne!(f2, f);
    assert_eq!(fs.read(f2, 0, 10), Ok(b"".to_vec()));
    fs.release(f);
    assert_eq!(fs.bytes_used(), 0, "released: freed");
    assert_eq!(
        fs.read(f, 0, 1),
        Err(Error::NotFound),
        "a stale id names nothing"
    );
    // a freed slot is reused by the next node
    let f3 = fs.create_file(ROOT, b"g").unwrap();
    assert_eq!(f3, f);
    // a directory with an open connection can be unlinked when empty
    let d = fs.mkdir(ROOT, b"d").unwrap();
    fs.retain(d).unwrap();
    fs.unlink(ROOT, b"d").unwrap();
    assert_eq!(
        fs.create_file(d, b"x"),
        Err(Error::NotFound),
        "an orphaned directory takes nothing new"
    );
    fs.release(d);
    // the root is never freed
    fs.retain(ROOT).unwrap();
    fs.release(ROOT);
    assert!(fs.lookup(ROOT, b"g").is_ok());
}

#[test]
fn read_only_directories_files_and_images() {
    let mut fs = Fs::new(Limits::default());
    let bin = fs.mkdir(ROOT, b"bin").unwrap();
    let prog = fs.add_image_file(bin, b"hello", 4096, 300).unwrap();
    let tmp = fs.mkdir(ROOT, b"tmp").unwrap();
    let doc = fs.create_file(tmp, b"doc").unwrap();
    fs.write(doc, 0, b"x").unwrap();
    fs.set_readonly(bin).unwrap();
    fs.set_readonly(doc).unwrap();
    assert_eq!(fs.image_extent(prog), Ok((4096, 300)));
    assert_eq!(fs.kind(prog), Ok(Kind::Image));
    assert_eq!(fs.size(prog), Ok(300));
    assert_eq!(fs.is_readonly(prog), Ok(true));
    assert_eq!(fs.is_readonly(tmp), Ok(false));
    assert_eq!(fs.create_file(bin, b"new"), Err(Error::ReadOnly));
    assert_eq!(fs.mkdir(bin, b"new"), Err(Error::ReadOnly));
    assert_eq!(fs.unlink(bin, b"hello"), Err(Error::ReadOnly));
    assert_eq!(fs.write(prog, 0, b"x"), Err(Error::ReadOnly));
    assert_eq!(fs.set_size(prog, 0), Err(Error::ReadOnly));
    assert_eq!(
        fs.read(prog, 0, 1),
        Err(Error::ReadOnly),
        "image bytes live in the boot image, not here"
    );
    assert_eq!(fs.write(doc, 0, b"y"), Err(Error::ReadOnly));
    assert_eq!(fs.set_size(doc, 0), Err(Error::ReadOnly));
    assert_eq!(
        fs.read(doc, 0, 5),
        Ok(b"x".to_vec()),
        "a read-only file reads"
    );
    assert_eq!(
        fs.create_file(tmp, b"other"),
        Ok(fs.lookup(tmp, b"other").unwrap())
    );
    assert_eq!(
        fs.add_image_file(tmp, b"bad", u64::MAX, 2),
        Err(Error::BadRange)
    );
    assert_eq!(fs.image_extent(doc), Err(Error::NotFound));
    assert_eq!(fs.image_extent(bin), Err(Error::IsDirectory));
}

#[test]
fn directory_listing_pages_by_name() {
    let mut fs = Fs::new(Limits::default());
    for n in ["b", "a", "d", "c", "e"] {
        fs.create_file(ROOT, n.as_bytes()).unwrap();
    }
    fs.mkdir(ROOT, b"sub").unwrap();
    let all = fs.read_dir(ROOT, None, 100).unwrap();
    let names: Vec<&[u8]> = all.iter().map(|(n, _)| n.as_slice()).collect();
    assert_eq!(
        names,
        [&b"a"[..], b"b", b"c", b"d", b"e", b"sub"],
        "bytewise order"
    );
    assert_eq!(all[5].1, Kind::Dir);
    assert_eq!(all[0].1, Kind::File);
    // pagination by the last name seen survives changes between pages
    let page1 = fs.read_dir(ROOT, None, 2).unwrap();
    assert_eq!(page1.len(), 2);
    fs.unlink(ROOT, b"a").unwrap(); // removed before the next page
    fs.create_file(ROOT, b"bb").unwrap(); // added in the middle
    let page2 = fs.read_dir(ROOT, Some(&page1[1].0), 100).unwrap();
    let names: Vec<&[u8]> = page2.iter().map(|(n, _)| n.as_slice()).collect();
    assert_eq!(names, [&b"bb"[..], b"c", b"d", b"e", b"sub"]);
    assert_eq!(fs.read_dir(ROOT, Some(b"zzz"), 10), Ok(vec![]));
    assert_eq!(fs.read_dir(ROOT, None, 0), Ok(vec![]));
    let f = fs.lookup(ROOT, b"c").unwrap();
    assert_eq!(fs.read_dir(f, None, 1), Err(Error::NotDirectory));
}

// --- the random walk against an independent model ---

/// The reference: a path-keyed map. `None` is a directory, `Some(bytes)` a file.
#[derive(Default)]
struct Model {
    entries: BTreeMap<Vec<u8>, Option<Vec<u8>>>, // path joined with '/'
}

fn join(path: &[&[u8]]) -> Vec<u8> {
    path.join(&b'/')
}

impl Model {
    fn parent_ok(&self, path: &[&[u8]]) -> Result<(), Error> {
        // every proper prefix must exist and be a directory
        for i in 1..path.len() {
            match self.entries.get(&join(&path[..i])) {
                None => return Err(Error::NotFound),
                Some(Some(_)) => return Err(Error::NotDirectory),
                Some(None) => {}
            }
        }
        Ok(())
    }
    fn create(&mut self, path: &[&[u8]], dir: bool) -> Result<(), Error> {
        self.parent_ok(path)?;
        let key = join(path);
        if self.entries.contains_key(&key) {
            return Err(Error::Exists);
        }
        self.entries
            .insert(key, if dir { None } else { Some(vec![]) });
        Ok(())
    }
    fn unlink(&mut self, path: &[&[u8]]) -> Result<(), Error> {
        self.parent_ok(path)?;
        let key = join(path);
        match self.entries.get(&key) {
            None => return Err(Error::NotFound),
            Some(None) => {
                let prefix = [key.clone(), b"/".to_vec()].concat();
                if self.entries.keys().any(|k| k.starts_with(&prefix)) {
                    return Err(Error::NotEmpty);
                }
            }
            Some(Some(_)) => {}
        }
        self.entries.remove(&key);
        Ok(())
    }
    fn write(&mut self, path: &[&[u8]], off: usize, data: &[u8]) -> Result<(), Error> {
        self.parent_ok(path)?;
        match self.entries.get_mut(&join(path)) {
            None => Err(Error::NotFound),
            Some(None) => Err(Error::IsDirectory),
            Some(Some(v)) => {
                if !data.is_empty() {
                    if v.len() < off + data.len() {
                        v.resize(off + data.len(), 0);
                    }
                    v[off..off + data.len()].copy_from_slice(data);
                }
                Ok(())
            }
        }
    }
    fn read(&self, path: &[&[u8]], off: usize, len: usize) -> Result<Vec<u8>, Error> {
        self.parent_ok(path)?;
        match self.entries.get(&join(path)) {
            None => Err(Error::NotFound),
            Some(None) => Err(Error::IsDirectory),
            Some(Some(v)) => {
                let s = off.min(v.len());
                Ok(v[s..(s + len).min(v.len())].to_vec())
            }
        }
    }
    fn set_size(&mut self, path: &[&[u8]], n: usize) -> Result<(), Error> {
        self.parent_ok(path)?;
        match self.entries.get_mut(&join(path)) {
            None => Err(Error::NotFound),
            Some(None) => Err(Error::IsDirectory),
            Some(Some(v)) => {
                v.resize(n, 0);
                Ok(())
            }
        }
    }
    fn list(&self, path: &[&[u8]]) -> Result<Vec<Vec<u8>>, Error> {
        if !path.is_empty() {
            self.parent_ok(path)?;
            match self.entries.get(&join(path)) {
                None => return Err(Error::NotFound),
                Some(Some(_)) => return Err(Error::NotDirectory),
                Some(None) => {}
            }
        }
        let prefix = if path.is_empty() {
            vec![]
        } else {
            [join(path), b"/".to_vec()].concat()
        };
        Ok(self
            .entries
            .keys()
            .filter(|k| {
                k.starts_with(&prefix)
                    && !k[prefix.len()..].contains(&b'/')
                    && k.len() > prefix.len()
            })
            .map(|k| k[prefix.len()..].to_vec())
            .collect())
    }
}

/// Walk a path in the implementation, mapping errors the way the model's `parent_ok` does.
fn parent(fs: &Fs, path: &[&[u8]]) -> Result<NodeId, Error> {
    let mut at = ROOT;
    for c in &path[..path.len() - 1] {
        at = fs.lookup(at, c)?;
        if fs.kind(at)? != Kind::Dir {
            return Err(Error::NotDirectory);
        }
    }
    Ok(at)
}

#[test]
fn random_operations_agree_with_the_model() {
    let names: [&[u8]; 3] = [b"a", b"b", b"c"];
    for seed in 1..300u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut fs = Fs::new(Limits::default());
        let mut model = Model::default();
        for step in 0..300 {
            let depth = 1 + rng.below(3) as usize;
            let path: Vec<&[u8]> = (0..depth).map(|_| names[rng.below(3) as usize]).collect();
            let ctx = format!(
                "seed {seed} step {step} path {:?}",
                path.iter()
                    .map(|p| String::from_utf8_lossy(p))
                    .collect::<Vec<_>>()
            );
            match rng.below(7) {
                0 | 1 => {
                    let dir = rng.below(2) == 0;
                    let want = model.create(&path, dir);
                    let got = parent(&fs, &path).and_then(|p| {
                        let n = path[path.len() - 1];
                        if dir {
                            fs.mkdir(p, n)
                        } else {
                            fs.create_file(p, n)
                        }
                        .map(|_| ())
                    });
                    assert_eq!(got, want, "create {ctx}");
                }
                2 => {
                    let want = model.unlink(&path);
                    let got = parent(&fs, &path).and_then(|p| fs.unlink(p, path[path.len() - 1]));
                    assert_eq!(got, want, "unlink {ctx}");
                }
                3 => {
                    let off = rng.below(20) as usize;
                    let data: Vec<u8> = (0..rng.below(12)).map(|_| rng.next() as u8).collect();
                    let want = model.write(&path, off, &data);
                    let got = parent(&fs, &path)
                        .and_then(|p| fs.lookup(p, path[path.len() - 1]))
                        .and_then(|id| fs.write(id, off as u64, &data).map(|_| ()));
                    assert_eq!(got, want, "write {ctx}");
                }
                4 => {
                    let (off, len) = (rng.below(30) as usize, rng.below(30) as usize);
                    let want = model.read(&path, off, len);
                    let got = parent(&fs, &path)
                        .and_then(|p| fs.lookup(p, path[path.len() - 1]))
                        .and_then(|id| fs.read(id, off as u64, len).map(|s| s.to_vec()));
                    assert_eq!(got, want, "read {ctx}");
                }
                5 => {
                    let n = rng.below(40) as usize;
                    let want = model.set_size(&path, n);
                    let got = parent(&fs, &path)
                        .and_then(|p| fs.lookup(p, path[path.len() - 1]))
                        .and_then(|id| fs.set_size(id, n as u64));
                    assert_eq!(got, want, "set_size {ctx}");
                }
                _ => {
                    let want = model.list(&path);
                    let got = if path.is_empty() {
                        Ok(ROOT)
                    } else {
                        parent(&fs, &path).and_then(|p| fs.lookup(p, path[path.len() - 1]))
                    }
                    .and_then(|id| fs.read_dir(id, None, 1000))
                    .map(|v| v.into_iter().map(|(n, _)| n).collect::<Vec<_>>());
                    assert_eq!(got, want, "list {ctx}");
                }
            }
            // the byte count equals the model's total file bytes
            let total: usize = model
                .entries
                .values()
                .map(|v| v.as_ref().map_or(0, |b| b.len()))
                .sum();
            assert_eq!(fs.bytes_used(), total, "bytes {ctx}");
            assert_eq!(fs.node_count(), model.entries.len() + 1, "nodes {ctx}");
        }
    }
}

#[test]
fn an_orphaned_directory_takes_nothing_new_and_leaks_nothing() {
    // Found by the S4b/S4c review: a directory unlinked while a connection holds it accepted new
    // children, which were never freed (the node and byte quotas leaked for good).
    let mut fs = Fs::new(Limits::default());
    let d = fs.mkdir(ROOT, b"d").unwrap();
    fs.retain(d).unwrap();
    fs.unlink(ROOT, b"d").unwrap();
    assert_eq!(
        fs.create_file(d, b"f"),
        Err(Error::NotFound),
        "no creating in an orphan"
    );
    assert_eq!(fs.mkdir(d, b"g"), Err(Error::NotFound));
    fs.release(d);
    assert_eq!(fs.node_count(), 1);
    assert_eq!(fs.bytes_used(), 0);
    // a child that is still held keeps its parent's contents alive no longer than its refs
    let d = fs.mkdir(ROOT, b"d").unwrap();
    let f = fs.create_file(d, b"f").unwrap();
    fs.write(f, 0, &[1u8; 100]).unwrap();
    fs.retain(f).unwrap();
    fs.unlink(d, b"f").unwrap();
    fs.unlink(ROOT, b"d").unwrap();
    assert_eq!(
        fs.read(f, 0, 3),
        Ok([1u8, 1, 1].to_vec()),
        "the held file survives its parent"
    );
    fs.release(f);
    assert_eq!((fs.node_count(), fs.bytes_used()), (1, 0));
}

#[test]
fn file_growth_does_not_hold_more_memory_than_it_counts() {
    // Amortised Vec growth reached twice the counted bytes after sequential appends.
    let mut fs = Fs::new(Limits::default());
    let f = fs.create_file(ROOT, b"f").unwrap();
    let chunk = [7u8; 32 * 1024];
    for i in 0..70u64 {
        fs.write(f, i * 32 * 1024, &chunk).unwrap();
        assert!(
            fs.data_capacity() <= fs.bytes_used() + 32 * 1024,
            "after chunk {i}: {} bytes held for {} counted",
            fs.data_capacity(),
            fs.bytes_used()
        );
    }
}

#[test]
fn chunked_storage_is_exact_across_chunk_boundaries() {
    let mut fs = Fs::new(Limits::default());
    let f = fs.create_file(ROOT, b"f").unwrap();
    let pattern: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
    assert_eq!(fs.write(f, 0, &pattern), Ok(70_000));
    assert_eq!(fs.read(f, 0, 100_000), Ok(pattern.clone()));
    assert_eq!(
        fs.read(f, 16_380, 10),
        Ok(pattern[16_380..16_390].to_vec()),
        "across a boundary"
    );
    assert_eq!(
        fs.read(f, 32_768, 1),
        Ok(pattern[32_768..32_769].to_vec()),
        "at a boundary"
    );
    // shrink into the middle of a chunk, grow again: the new bytes are zeros, not the old data
    fs.set_size(f, 20_000).unwrap();
    fs.set_size(f, 40_000).unwrap();
    let back = fs.read(f, 0, 40_000).unwrap();
    assert_eq!(&back[..20_000], &pattern[..20_000]);
    assert!(
        back[20_000..].iter().all(|&b| b == 0),
        "stale bytes came back after truncate+grow"
    );
    // a write that leaves a hole across several chunks
    let g = fs.create_file(ROOT, b"g").unwrap();
    fs.write(g, 50_000, b"tail").unwrap();
    let all = fs.read(g, 0, 60_000).unwrap();
    assert_eq!(all.len(), 50_004);
    assert!(all[..50_000].iter().all(|&b| b == 0));
    assert_eq!(&all[50_000..], b"tail");
    // memory held is the counted bytes rounded up to chunks, per file
    assert!(fs.data_capacity() <= fs.bytes_used() + 2 * 16 * 1024);
}
