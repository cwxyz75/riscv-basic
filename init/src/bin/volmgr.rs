//! `volmgr`: the volume manager (FS-SPEC section 6, steps 5d and 5e). It asks the device manager for every block device, probes
//! each for a doofs superblock (a hostile device costs eight block reads; anything else is logged and ignored), reads the mount
//! table (`etc/mounts` in the boot image: a pure mapping from uuid to local name), decides with `mounts::plan` what to publish
//! (a cloned disk is refused; names come from the table, never from the disk), starts one `fsd` per volume and publishes it in
//! the registry as `vol-uuid-<hex>` and, if the table maps the uuid, `vol-<name>`. A volume server that dies is restarted
//! (it remounts from the newest valid superblock), its names republished; clients see `UNAVAILABLE` meanwhile.
//!
//! Discovery follows `mounts::discovery` (T286): a device that is `Busy` (its previous driver winding down) is opened again for up
//! to 5 s; a probe that cannot read the device just after it was `Busy` is tried again for up to 5 s more (the old driver's last
//! moments are not a verdict on the disk); a readable device with no volume is never probed twice. Each stage is said on the
//! console with the clock (`volmgr: t=<ms> ...`) and how long it took, so that a slow start names its slow stage.
//!
//! (The registry's names are flat, so the spec's `/svc/vol/by-uuid/<hex>` and `/svc/vol/<name>` are `vol-uuid-<hex>` and
//! `vol-<name>` for now: work item T118.)
//!
//! Handles: `ServiceDevmgr`, `ServiceRegistry` (a connection with PUBLISH), `BootImage` (for `bin/fsd` and `etc/mounts`).
#![no_std]
#![no_main]

/// Log to the serial line unless the parent asked for quiet (`QUIET=1`: a user's session).
static QUIET: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
macro_rules! say {
    ($($a:tt)*) => {
        if !QUIET.load(core::sync::atomic::Ordering::Relaxed) {
            rt::println!($($a)*);
        }
    };
}

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use mounts::{discovery, Decision, Found};
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
    proc: Option<u64>,
    restarts: u32,
}

/// One OpenBlock exchange: the status and the block channel.
fn open_block(dm: u64, txid: u32, index: u16) -> Option<(devmgr::Status, Option<u64>)> {
    let req = devmgr::encode_open_block_at(txid, index);
    if sys::channel_write(dm, req.as_ptr(), req.len(), core::ptr::null(), 0).status != 0 {
        return None;
    }
    if sys::object_wait(dm, signals::READABLE, sys::clock_get() + 5000).status != 0 {
        return None;
    }
    let mut buf = [0u8; 64];
    let mut hs = [0u64; 2];
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

/// OpenBlock, retrying while the previous driver for the device winds down (`Busy`, up to `discovery::BUSY_MS`).
fn open_block_retry(dm: u64, txid: &mut u32, index: u16) -> Option<(devmgr::Status, Option<u64>)> {
    let mut d = discovery::Device::new(sys::clock_get());
    loop {
        *txid += 1;
        let (st, h) = open_block(dm, *txid, index)?;
        if d.opened(opened(st, h), sys::clock_get()) != discovery::Next::OpenAgain {
            return Some((st, h));
        }
        if let Some(h) = h {
            sys::handle_close(h);
        }
        let _ = sys::object_wait(dm, 1 << 24, sys::clock_get() + 20);
    }
}

/// The device manager's answer, as the discovery decision sees it.
fn opened(st: devmgr::Status, h: Option<u64>) -> discovery::Opened {
    match (st, h) {
        (devmgr::Status::Ok, Some(_)) => discovery::Opened::Ok,
        (devmgr::Status::Busy, _) => discovery::Opened::Busy,
        (devmgr::Status::NoDevice, _) => discovery::Opened::NoDevice,
        _ => discovery::Opened::Failed,
    }
}

/// What discovery found on one device.
enum Discovered {
    /// A volume, and the channel to its driver.
    Volume(fsfmt::Superblock, u64),
    /// Not served (said on the console).
    Ignored,
    /// There is no such device.
    End,
    /// The device manager did not answer.
    Lost,
}

/// Open and probe device `index`, retrying as `mounts::discovery` decides (T286: a probe that fails just after the device was Busy
/// is the old driver's last moments, not a verdict). Each outcome is said with the clock and how long it took, so that a slow start
/// names its slow stage.
fn discover(dm: u64, txid: &mut u32, index: u16) -> Discovered {
    let t0 = sys::clock_get();
    let mut d = discovery::Device::new(t0);
    let mut probes = 0u32;
    loop {
        *txid += 1;
        let Some((st, h)) = open_block(dm, *txid, index) else {
            return Discovered::Lost;
        };
        match d.opened(opened(st, h), sys::clock_get()) {
            discovery::Next::OpenAgain => {
                if let Some(h) = h {
                    sys::handle_close(h);
                }
                let _ = sys::object_wait(dm, 1 << 24, sys::clock_get() + 20);
                continue;
            }
            discovery::Next::End => return Discovered::End,
            discovery::Next::Probe => {}
            _ => {
                let now = sys::clock_get();
                say!(
                    "volmgr: t={now} device {index}: {st:?} after {} ms, ignored",
                    now - t0
                );
                if let Some(h) = h {
                    sys::handle_close(h);
                }
                return Discovered::Ignored;
            }
        }
        let Some(ch) = h else {
            return Discovered::Ignored;
        };
        probes += 1;
        let p0 = sys::clock_get();
        let probe = match BlkDevice::open(ch) {
            Some(mut dev) => fsfmt::probe_volume(&mut dev),
            None => fsfmt::Probe::Unreadable,
        };
        let now = sys::clock_get();
        let kind = match &probe {
            fsfmt::Probe::Volume(_) => discovery::Probed::Volume,
            fsfmt::Probe::NotVolume => discovery::Probed::NotVolume,
            fsfmt::Probe::Unreadable => discovery::Probed::Unreadable,
        };
        let busy = if d.was_busy() { ", after Busy" } else { "" };
        match (d.probed(kind, now), probe) {
            (discovery::Next::Use, fsfmt::Probe::Volume(sb)) => {
                say!(
                    "volmgr: t={now} device {index}: doofs volume {} {:?} (probe {probes} took {} ms, {} ms in all{busy})",
                    mounts::uuid_hex(&sb.uuid),
                    sb.label_str(),
                    now - p0,
                    now - t0
                );
                return Discovered::Volume(sb, ch);
            }
            (discovery::Next::OpenAgain, _) => {
                say!(
                    "volmgr: t={now} device {index}: unreadable (probe {probes} took {} ms{busy}): probing again",
                    now - p0
                );
                sys::handle_close(ch);
                let _ = sys::object_wait(dm, 1 << 24, sys::clock_get() + 20);
            }
            (_, p) => {
                let what = if matches!(p, fsfmt::Probe::Unreadable) {
                    "unreadable"
                } else {
                    "not a doofs volume"
                };
                say!(
                    "volmgr: t={now} device {index}: {what} (probe {probes} took {} ms, {} ms in all{busy}), ignored",
                    now - p0,
                    now - t0
                );
                sys::handle_close(ch);
                return Discovered::Ignored;
            }
        }
    }
}

fn image_file(bi: u64, name: &[u8]) -> Option<Vec<u8>> {
    let total = sys::vmo_get_size(bi).a1;
    let prefix = rt::spawn::read_image(bi, 0, (total as u64).min(128 * 1024))?;
    let entries = bootimg::parse_prefix(&prefix, total).ok()?;
    let e = entries.iter().find(|e| e.name == name)?;
    rt::spawn::read_image(bi, u64::from(e.offset), u64::from(e.size))
}

/// Start the volume server for `v` on block channel `blk`, and publish its names.
fn start(
    v: &mut Volume,
    blk: u64,
    image: &[u8],
    reg: &mut rt::fs::Fs,
    clock_view: &mut Option<rt::fs::Fs>,
) -> bool {
    let mut handles = alloc::vec![blk];
    let mut roles = alloc::vec![(Role::BlockDevice, 0u32)];
    // a connection to a registry view that allows `clock`, for file times (none: the server uses time since boot)
    if let Some(c) = clock_view
        .as_mut()
        .and_then(|c| c.clone_conn(proto::file::rights::READ).ok())
    {
        handles.push(c.ch);
        roles.push((Role::ServiceRegistry, handles.len() as u32 - 1));
    }
    let mut names: Vec<String> = alloc::vec![format!("vol-uuid-{}", v.uuid_hex)];
    if let Some(n) = &v.name {
        names.push(format!("vol-{n}"));
    }
    let mut published: Vec<(String, u64)> = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let c = sys::channel_create();
        if c.status != 0 {
            return false;
        }
        let (mine, theirs) = (c.a1 as u64, c.a2 as u64);
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
    // the server writes a byte here once it has mounted the volume; its exit first closes it
    let rc = sys::channel_create();
    if rc.status != 0 {
        return false;
    }
    let (ready_mine, ready_theirs) = (rc.a1 as u64, rc.a2 as u64);
    handles.push(ready_theirs);
    roles.push((Role::Ready, handles.len() as u32 - 1));
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
    // wait until the server is ready (or gone): a name published before that would hand clients a connection to a server that
    // may then exit (a mount that fails)
    let t0 = sys::clock_get();
    let w = sys::object_wait(
        ready_mine,
        signals::READABLE | signals::PEER_CLOSED,
        t0 + discovery::READY_MS,
    );
    let mut b = [0u8; 1];
    let got = w.status == 0
        && sys::channel_read(ready_mine, b.as_mut_ptr(), 1, core::ptr::null_mut(), 0).status == 0;
    sys::handle_close(ready_mine);
    let now = sys::clock_get();
    if !got {
        say!(
            "volmgr: t={now} the volume server for {} did not come up ({} after {} ms)",
            v.uuid_hex,
            if w.status == 0 { "it ended" } else { "no word" },
            now - t0
        );
        sys::process_kill(p, -1);
        sys::handle_close(p);
        v.proc = None;
        // (the listeners' other ends, never published)
        for (_, theirs) in published {
            sys::handle_close(theirs);
        }
        return false;
    }
    // (after a restart the old listeners closed with the old server and the registry has marked the names stale, which the
    // caller waited for: a publish replaces a stale name; a publish that fails closes the listener, so the server is useless)
    let mut published = published.into_iter();
    while let Some((name, theirs)) = published.next() {
        if let Err(e) = reg.publish(name.as_bytes(), theirs) {
            say!("volmgr: t={now} publish {name}: status {e}");
            sys::process_kill(p, -1);
            sys::handle_close(p);
            v.proc = None;
            // (the failed publish closed its listener; the rest were never handed over)
            for (_, theirs) in published {
                sys::handle_close(theirs);
            }
            return false;
        }
    }
    say!(
        "volmgr: t={} volume {}: ready in {} ms, published",
        sys::clock_get(),
        v.uuid_hex,
        now - t0
    );
    true
}

#[no_mangle]
pub fn app_main() -> i32 {
    let env = match startup::read() {
        Ok(e) => e,
        Err(startup::EnvError::NoMessage) => return EXIT_NO_STARTUP,
        Err(_) => return EXIT_BAD_STARTUP,
    };
    QUIET.store(
        env.var(b"QUIET") == Some(&b"1"[..]),
        core::sync::atomic::Ordering::Relaxed,
    );
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
                say!(
                    "volmgr: etc/mounts is not usable ({:?} at line {}): no names",
                    e.kind,
                    e.line
                );
                Vec::new()
            }
        },
    };
    let mut reg = rt::fs::Fs::new(registry);
    let mut clock_view = reg.view(&[&b"clock"[..]], proto::file::rights::READ).ok();
    // ---- discovery
    let mut txid = 0u32;
    let mut found: Vec<Found> = Vec::new();
    let mut chans: Vec<u64> = Vec::new();
    for index in 0..MAX_DEVICES {
        match discover(dm, &mut txid, index) {
            Discovered::Volume(sb, ch) => {
                found.push(Found {
                    device: index,
                    uuid: sb.uuid,
                    label: String::from(sb.label_str()),
                });
                chans.push(ch);
            }
            Discovered::Ignored => {}
            Discovered::End | Discovered::Lost => break,
        }
    }
    // ---- naming and serving
    let mut volumes: Vec<Volume> = Vec::new();
    for ((f, d), ch) in mounts::plan(&table, &found).into_iter().zip(chans) {
        match d {
            Decision::RefuseDuplicate => {
                say!(
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
                if start(&mut v, ch, &fsd_image, &mut reg, &mut clock_view) {
                    say!("volmgr: serving {} as {:?}", v.uuid_hex, v.name);
                    volumes.push(v);
                }
            }
        }
    }
    say!("volmgr: up, {} volume(s)", volumes.len());
    // ---- supervision
    let p = sys::port_create();
    if p.status != 0 {
        return 5;
    }
    let port = p.a1 as u64;
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
        // restart it, retrying (a start can fail: the old driver still winding down, a mount that did not work yet)
        loop {
            if v.restarts >= MAX_RESTARTS {
                say!("volmgr: volume {} keeps failing: left down", v.uuid_hex);
                break;
            }
            v.restarts += 1;
            say!(
                "volmgr: volume {} ended: restart {}",
                v.uuid_hex,
                v.restarts
            );
            // let the registry notice the old listeners closed, so that the names are stale and can be published again
            let _ = sys::object_wait(dm, 1 << 24, sys::clock_get() + 200);
            let Some((devmgr::Status::Ok, Some(ch))) = open_block_retry(dm, &mut txid, v.device)
            else {
                continue;
            };
            if start(v, ch, &fsd_image, &mut reg, &mut clock_view) {
                if let Some(pr) = v.proc {
                    sys::port_bind(port, pr, signals::TERMINATED, i as u64);
                }
                break;
            }
        }
    }
}
