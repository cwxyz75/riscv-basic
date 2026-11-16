//! `mkfs-doofs [--uuid HEX] [--label NAME] [--size-mib N] IMAGE`: write an empty volume. With `--size-mib` the image is
//! created (or truncated) at that size; otherwise the file or device is used as it is.

use doofs_tools::{hex, parse_uuid, random_uuid, FileDevice};
use fsfmt::mkfs;

fn main() {
    let (mut uuid, mut label, mut size, mut path) = (None, String::new(), None::<u64>, None);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--uuid" => uuid = args.next().and_then(|s| parse_uuid(&s)).or_else(usage),
            "--label" => label = args.next().unwrap_or_else(usage),
            "--size-mib" => size = args.next().and_then(|s| s.parse().ok()).or_else(usage),
            _ if path.is_none() => path = Some(a),
            _ => usage(),
        }
    }
    let path = path.unwrap_or_else(usage);
    let uuid = uuid.unwrap_or_else(random_uuid);
    let mut dev = match size {
        Some(m) => FileDevice::create(&path, m * 256),
        None => FileDevice::open(&path, true),
    }
    .unwrap_or_else(|e| fail(&format!("{path}: {e}")));
    if let Err(e) = mkfs(&mut dev, uuid, &label) {
        fail(&format!("mkfs: {e:?}"));
    }
    println!("{path}: doofs volume {} label {label:?}", hex(&uuid));
}

fn usage<T>() -> T {
    eprintln!("usage: mkfs-doofs [--uuid HEX] [--label NAME] [--size-mib N] IMAGE");
    std::process::exit(2)
}

fn fail(m: &str) -> ! {
    eprintln!("{m}");
    std::process::exit(1)
}
