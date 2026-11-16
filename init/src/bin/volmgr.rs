//! `volmgr`: the volume manager (FS-SPEC section 6, steps 5d and 5e). It asks the device manager for every block device, probes
//! each for a doofs superblock (a hostile device costs eight block reads; anything else is logged and ignored), reads the mount
//! table (`etc/mounts` in the boot image: a pure mapping from uuid to local name), decides with `mounts::plan` what to publish
//! (a cloned disk is refused; names come from the table, never from the disk), starts one `fsd` per volume and publishes it in
//! the registry as `vol-uuid-<hex>` and, if the table maps the uuid, `vol-<name>`. A volume server that dies is restarted
//! (it remounts from the newest valid superblock), its names republished; clients see `UNAVAILABLE` meanwhile.
//!
//! (The registry's names are flat, so the spec's `/svc/vol/by-uuid/<hex>` and `/svc/vol/<name>` are `vol-uuid-<hex>` and
//! `vol-<name>` for now: TIDY-UP T118.)
//!
//! Handles: `ServiceDevmgr`, `ServiceRegistry` (a connection with PUBLISH), `BootImage` (for `bin/fsd` and `etc/mounts`).
#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use mounts::{Decision, Found};
use proto::bootimg;
use proto::devmgr;
use proto::startup::{Role, Startup};
use rt::abi::signals;
use rt::blk::BlkDevice;
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};
use rt::sys;

/// Most devices looked at (the device manager lists them in slot order).
const MAX_DEVICES: u16 = 16;
/// A volume server is restarted this many times, then left down.
const MAX_RESTARTS: u32 = 5;

struct Volume {
    device: u16,
    uuid_hex: String,
    name: Option<String>,
    readonly: bool,
    sub: Option<String>,
    proc: Option<u32>,
    restarts: u32,
}

/// One OpenBlock exchange: the status and the block channel.
fn open_block(dm: u32, txid: u32, index: u16) -> Option<(devmgr::Status, Option<u32>)> {
    let req = devmgr::encode_open_block_at(txid, index);
    if sys::channel_write(dm, req.as_ptr(), req.len(), core::ptr::null(), 0).status != 0 {
        return None;
    }
    if sys::object_wait(dm, signals::READABLE, sys::clock_get() + 5000).status != 0 {
        return None;
    }
    let mut buf = [0u8; 64];
    let mut hs = [0u32; 2];
    let r = sys::channel_read(dm, buf.as_mut_ptr(), buf.len(), hs.as_mut_ptr(), hs.len());
    if r.status != 0 {
        return None;
    }
    let (t, status) = devmgr::decode_reply(&buf[..r.a1])?;
    if t != txid {
        return None;
    }
    Some((status, (r.a2 == 1).then_some(hs[0])))
}

/// OpenBlock, retrying while the previous driver for the device winds down.
fn open_block_retry(dm: u32, txid: &mut u32, index: u16) -> Option<(devmgr::Status, Option<u32>)> {
    let until = sys::clock_get() + 5000;
    loop {
        *txid += 1;
        let (st, h) = open_block(dm, *txid, index)?;
        if st != devmgr::Status::Busy || sys::clock_get() >= until {
            return Some((st, h));
        }
        let _ = sys::object_wait(dm, 1 << 24, sys::clock_get() + 20);
    }
}

fn image_file(bi: u32, name: &[u8]) -> Option<Vec<u8>> {
    let total = sys::vmo_get_size(bi).a1;
    let prefix = rt::spawn::read_image(bi, 0, (total as u64).min(128 * 1024))?;
    let entries = bootimg::parse_prefix(&prefix, total).ok()?;
    let e = entries.iter().find(|e| e.name == name)?;
    rt::spawn::read_image(bi, u64::from(e.offset), u64::from(e.size))
}

/// Start the volume server for `v` on block channel `blk`, and publish its names.
fn start(v: &mut Volume, blk: u32, image: &[u8], reg: &mut rt::fs::Fs) -> bool {
    let mut handles = alloc::vec![blk];
    let mut roles = alloc::vec![(Role::BlockDevice, 0u32)];
    let mut names: Vec<String> = alloc::vec![format!("vol-uuid-{}", v.uuid_hex)];
    if let Some(n) = &v.name {
        names.push(format!("vol-{n}"));
    }
    let mut published: Vec<(String, u32)> = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let c = sys::channel_create();
        if c.status != 0 {
            return false;
        }
        let (mine, theirs) = (c.a1 as u32, c.a2 as u32);
        handles.push(mine);
        roles.push((
            if i == 0 {
                Role::ServiceListener
            } else {
                Role::ServiceListenerAlt
            },
            handles.len() as u32 - 1,
        ));
        published.push((name.clone(), theirs));
    }
    let mut args: Vec<Vec<u8>> = alloc::vec![b"fsd".to_vec()];
    if v.readonly {
        args.push(b"ro".to_vec());
    }
    if let Some(s) = &v.sub {
        args.push(format!("sub={s}").into_bytes());
    }
    let startup = Startup {
        args,
        roles,
        ..Startup::default()
    };
    let Ok(bytes) = startup.encode() else {
        return false;
    };
    let Ok(p) = rt::spawn::spawn(image, &bytes, &handles) else {
        return false;
    };
    v.proc = Some(p);
    // (after a restart the old listeners closed with the old server and the registry has marked the names stale, which the
    // caller waited for: a publish replaces a stale name; a publish that fails closes the listener, so the server is useless)
    for (name, theirs) in published {
        if reg.publish(name.as_bytes(), theirs).is_err() {
            sys::process_kill(p, -1);
            return false;
        }
    }
    true
}

#[no_mangle]
pub fn app_main() -> i32 {
    let env = match startup::read() {
        Ok(e) => e,
        Err(startup::EnvError::NoMessage) => return EXIT_NO_STARTUP,
        Err(_) => return EXIT_BAD_STARTUP,
    };
    let (Some(dm), Some(registry), Some(bi)) = (
        env.handle(Role::ServiceDevmgr),
        env.handle(Role::ServiceRegistry),
        env.handle(Role::BootImage),
    ) else {
        return 3;
    };
    let Some(fsd_image) = image_file(bi, b"bin/fsd") else {
        return 4;
    };
    let table = match image_file(bi, b"etc/mounts") {
        None => Vec::new(),
        Some(t) => match mounts::parse(&t) {
            Ok(t) => t,
            Err(e) => {
                rt::println!(
                    "volmgr: etc/mounts is not usable ({:?} at line {}): no names",
                    e.kind,
                    e.line
                );
                Vec::new()
            }
        },
    };
    let mut reg = rt::fs::Fs::new(registry);
    // ---- discovery
    let mut txid = 0u32;
    let mut found: Vec<Found> = Vec::new();
    let mut chans: Vec<u32> = Vec::new();
    for index in 0..MAX_DEVICES {
        let Some((st, h)) = open_block_retry(dm, &mut txid, index) else {
            break;
        };
        if st == devmgr::Status::NoDevice {
            break;
        }
        let (devmgr::Status::Ok, Some(ch)) = (st, h) else {
            rt::println!("volmgr: device {index}: {st:?}");
            continue;
        };
        let probed = BlkDevice::open(ch).and_then(|mut d| fsfmt::probe(&mut d));
        match probed {
            Some(sb) => {
                rt::println!(
                    "volmgr: device {index}: doofs volume {} {:?}",
                    mounts::uuid_hex(&sb.uuid),
                    sb.label_str()
                );
                found.push(Found {
                    device: index,
                    uuid: sb.uuid,
                    label: String::from(sb.label_str()),
                });
                chans.push(ch);
            }
            None => {
                rt::println!("volmgr: device {index}: not a doofs volume, ignored");
                sys::handle_close(ch);
            }
        }
    }
    // ---- naming and serving
    let mut volumes: Vec<Volume> = Vec::new();
    for ((f, d), ch) in mounts::plan(&table, &found).into_iter().zip(chans) {
        match d {
            Decision::RefuseDuplicate => {
                rt::println!(
                    "volmgr: device {}: a second volume with uuid {}: refused",
                    f.device,
                    mounts::uuid_hex(&f.uuid)
                );
                sys::handle_close(ch);
            }
            Decision::Publish {
                by_uuid,
                name,
                readonly,
                sub,
            } => {
                let mut v = Volume {
                    device: f.device,
                    uuid_hex: by_uuid,
                    name,
                    readonly,
                    sub,
                    proc: None,
                    restarts: 0,
                };
                if start(&mut v, ch, &fsd_image, &mut reg) {
                    rt::println!("volmgr: serving {} as {:?}", v.uuid_hex, v.name);
                    volumes.push(v);
                }
            }
        }
    }
    rt::println!("volmgr: up, {} volume(s)", volumes.len());
    // ---- supervision
    let p = sys::port_create();
    if p.status != 0 {
        return 5;
    }
    let port = p.a1 as u32;
    for (i, v) in volumes.iter().enumerate() {
        if let Some(pr) = v.proc {
            sys::port_bind(port, pr, signals::TERMINATED, i as u64);
        }
    }
    loop {
        let r = sys::port_wait(port, u64::MAX);
        if r.status != 0 {
            return 6;
        }
        let i = r.a1;
        let Some(v) = volumes.get_mut(i) else {
            continue;
        };
        if let Some(old) = v.proc.take() {
            sys::handle_close(old);
        }
        if v.restarts >= MAX_RESTARTS {
            rt::println!("volmgr: volume {} keeps failing: left down", v.uuid_hex);
            continue;
        }
        v.restarts += 1;
        // let the registry notice the old listeners closed, so that the names are stale and can be published again
        let _ = sys::object_wait(dm, 1 << 24, sys::clock_get() + 200);
        rt::println!(
            "volmgr: volume {} ended: restart {}",
            v.uuid_hex,
            v.restarts
        );
        let Some((devmgr::Status::Ok, Some(ch))) = open_block_retry(dm, &mut txid, v.device) else {
            continue;
        };
        if start(v, ch, &fsd_image, &mut reg) {
            if let Some(pr) = v.proc {
                sys::port_bind(port, pr, signals::TERMINATED, i as u64);
            }
        }
    }
}
