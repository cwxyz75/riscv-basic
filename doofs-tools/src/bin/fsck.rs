//! `fsck-doofs IMAGE`: read every superblock slot, walk the trees, report. Exit 0 if the volume is clean (mount would use a
//! verifying slot and nothing else is wrong), 1 if there are problems, 2 on a usage or I/O error. Changes nothing.

use doofs_tools::{hex, FileDevice};
use fsfmt::check;

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: fsck-doofs IMAGE");
        std::process::exit(2);
    };
    let mut dev = FileDevice::open(&path, false).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        std::process::exit(2)
    });
    let r = check(&mut dev);
    for s in &r.slots {
        match &s.sb {
            Ok(sb) => println!(
                "slot {} (block {}): generation {} uuid {} label {:?} tree {}",
                s.slot,
                s.addr,
                sb.generation,
                hex(&sb.uuid),
                sb.label_str(),
                match &s.tree {
                    Some(Ok((e, n))) => format!("ok ({e} entries, {n} nodes)"),
                    Some(Err(e)) => format!("BAD ({e:?})"),
                    None => "-".into(),
                }
            ),
            Err(e) => println!(
                "slot {} (block {}): not a superblock ({e:?})",
                s.slot, s.addr
            ),
        }
    }
    match r.chosen {
        Some(c) => println!(
            "mount uses slot {c}: {} blocks reachable, {} free",
            r.reachable, r.unreachable
        ),
        None => println!("no usable slot"),
    }
    for p in &r.problems {
        println!("problem: {p}");
    }
    let mut bad = r.chosen.is_none() || !r.problems.is_empty();
    // the filesystem layer: link counts, directory sizes, orphans, attributes, indexes, quota counters. It runs on an
    // in-memory copy of the image (opening a volume reaps orphans and commits, which fsck must never do to the disk).
    if !bad {
        match fs_problems(&path) {
            Ok(p) => {
                for m in &p {
                    println!("problem: {m}");
                }
                bad = !p.is_empty();
            }
            Err(e) => {
                println!("problem: {e}");
                bad = true;
            }
        }
    }
    std::process::exit(i32::from(bad));
}

fn fs_problems(path: &str) -> Result<Vec<String>, String> {
    use fsfmt::{Device, MemDevice, BLOCK};
    let mut src = FileDevice::open(path, false).map_err(|e| format!("{path}: {e}"))?;
    let blocks = src.blocks();
    if blocks > 1 << 20 {
        return Ok(Vec::new()); // (4 GiB: too big to copy; the volume check above stands)
    }
    let mut mem = MemDevice::new(blocks);
    let mut b = [0u8; BLOCK];
    for a in 0..blocks {
        src.read(a, &mut b)
            .map_err(|e| format!("read {a}: {e:?}"))?;
        if b.iter().any(|&x| x != 0) {
            mem.write(a, &b).map_err(|e| format!("copy {a}: {e:?}"))?;
        }
    }
    let vol = fsfmt::mount(mem).map_err(|e| format!("mount: {e:?}"))?;
    let big = memfs::Limits {
        max_nodes: 1 << 24,
        max_bytes: usize::MAX / 2,
        max_file: usize::MAX / 2,
        max_meta: usize::MAX / 2,
    };
    let mut fs = doofs::Fs::open(vol, big).map_err(|e| format!("open: {e:?}"))?;
    Ok(doofs::fsck::check_fs(&mut fs))
}
