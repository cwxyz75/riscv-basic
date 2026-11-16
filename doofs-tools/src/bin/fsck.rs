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
    std::process::exit(if r.chosen.is_some() && r.problems.is_empty() {
        0
    } else {
        1
    });
}
