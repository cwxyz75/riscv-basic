//! `dump-doofs IMAGE [--tree]`: print the newest verifying superblock, and with `--tree` every key and value length (keys as
//! printable bytes with `\xNN` escapes).

use doofs_tools::{hex, FileDevice};
use fsfmt::{mount, Tree};

fn esc(b: &[u8]) -> String {
    b.iter()
        .map(|&c| {
            if (0x20..0x7f).contains(&c) && c != b'\\' {
                (c as char).to_string()
            } else {
                format!("\\x{c:02x}")
            }
        })
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.iter().find(|a| !a.starts_with("--")) else {
        eprintln!("usage: dump-doofs IMAGE [--tree]");
        std::process::exit(2);
    };
    let dev = FileDevice::open(path, false).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        std::process::exit(2)
    });
    let mut v = mount(dev).unwrap_or_else(|e| {
        eprintln!("mount: {e:?}");
        std::process::exit(1)
    });
    println!(
        "uuid {} label {:?} generation {}",
        hex(&v.uuid),
        v.label(),
        v.last.gen
    );
    if args.iter().any(|a| a == "--tree") {
        for (k, val) in Tree::range_at(&mut v.pool, v.last.root, None, None).expect("tree") {
            println!("{} => {} bytes", esc(&k), val.len());
        }
    }
}
