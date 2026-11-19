//! `clockd`: the wall-clock service (FS-SPEC section 7, step C1). It claims the real-time clock from the device manager (a
//! goldfish RTC on QEMU `virt`: nanoseconds since the epoch in two registers), keeps a `clockmodel` that combines each reading
//! with the monotonic clock (so the answer never goes backwards and a correction is slewed), re-reads it every minute, and
//! serves `/svc/clock` (`proto::clock`). With no RTC the quality is `Unset` and the time counts from the epoch at boot.
//!
//! Handles: `ServiceDevmgr` (to claim the device), `ServiceRegistry` (a connection with PUBLISH, to publish `clock`).
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

use clockmodel::{ClockModel, Quality};
use proto::clock as pc;
use proto::devmgr;
use proto::startup::Role;
use rt::service::{self, Core, Service};
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};
use rt::sys;
use svc::Limits;

const RTC_VA: u64 = 0x600_0000;
const PF_R: u64 = 1;
/// Goldfish RTC registers: reading TIME_LOW latches TIME_HIGH.
const TIME_LOW: usize = 0x00;
const TIME_HIGH: usize = 0x04;
/// Read the device again this often (ticks, which are milliseconds).
const RESYNC: u64 = 60_000;

enum Conn {
    Listener,
    Client,
}

/// Nanoseconds from the monotonic clock (the kernel's own unit).
fn mono_ns() -> u64 {
    sys::clock_ns()
}

fn read_rtc() -> u64 {
    // SAFETY: RTC_VA maps the device's register page read-only for this process (`app_main`); the two 32-bit reads are
    // what the device defines (a read of the low word latches the high word).
    unsafe {
        let low = core::ptr::read_volatile((RTC_VA as usize + TIME_LOW) as *const u32);
        let high = core::ptr::read_volatile((RTC_VA as usize + TIME_HIGH) as *const u32);
        (u64::from(high) << 32) | u64::from(low)
    }
}

struct Clockd {
    core: Core<Conn>,
    model: ClockModel,
    rtc: bool,
    next_sync: u64,
}

impl Clockd {
    fn resync(&mut self) {
        if self.rtc {
            let wall = read_rtc();
            self.model.sync(wall, mono_ns(), Quality::FromRtc);
        }
        self.next_sync = sys::clock_get() + RESYNC;
    }
}

fn quality_code(q: Quality) -> u8 {
    match q {
        Quality::Unset => pc::quality::UNSET,
        Quality::FromRtc => pc::quality::FROM_RTC,
        Quality::Synced => pc::quality::SYNCED,
    }
}

impl Service for Clockd {
    type Conn = Conn;

    fn core(&mut self) -> &mut Core<Conn> {
        &mut self.core
    }

    fn request(&mut self, key: u64, msg: &[u8], hs: &[u32]) {
        if matches!(self.core.conn(key), Some(Conn::Listener)) {
            // a forwarded Open: the client's endpoint becomes a connection in an account of its own
            let ok = proto::file::decode_request(msg)
                .is_ok_and(|r| r.op == proto::file::op::OPEN && r.payload.is_empty())
                && hs.len() == 1;
            if !ok {
                for &h in hs {
                    sys::handle_close(h);
                }
                return;
            }
            let limits = Limits {
                conns: 4,
                held_bytes: 4 * 1024,
                inflight: 4,
                stream_bytes: 0,
            };
            let Ok(account) = self.core.accounts().open(svc::ROOT, limits) else {
                sys::handle_close(hs[0]);
                return;
            };
            if self.core.add_conn(hs[0], account, Conn::Client).is_none() {
                sys::handle_close(hs[0]);
                let _ = self.core.accounts().close(account);
            }
            return;
        }
        for &h in hs {
            sys::handle_close(h);
        }
        let txid = match pc::decode_request(msg) {
            Ok(t) => t,
            Err(Some(t)) => {
                let _ = self.core.send(
                    key,
                    pc::encode_reply(t, pc::status::BAD_REQUEST, 0, 0),
                    None,
                );
                return;
            }
            Err(None) => return self.core.violation(key),
        };
        let reply = pc::encode_reply(
            txid,
            pc::status::OK,
            self.model.now(mono_ns()),
            quality_code(self.model.quality()),
        );
        let _ = self.core.send(key, reply, None);
    }

    fn closed(&mut self, _conn: Conn) {}

    fn next_deadline(&mut self) -> Option<u64> {
        Some(self.next_sync)
    }

    fn tick(&mut self) {
        if sys::clock_get() >= self.next_sync {
            self.resync();
        }
    }
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
    let (Some(devmgr_ch), Some(registry)) = (
        env.handle(Role::ServiceDevmgr),
        env.handle(Role::ServiceRegistry),
    ) else {
        return 3;
    };
    // claim the clock device (none: the service still runs, with quality Unset)
    let mut rtc = false;
    let req = devmgr::encode_claim_rtc(1, 0);
    if sys::channel_write(devmgr_ch, req.as_ptr(), req.len(), core::ptr::null(), 0).status == 0 {
        let w = sys::object_wait(
            devmgr_ch,
            rt::abi::signals::READABLE,
            sys::clock_get() + 3000,
        );
        if w.status == 0 {
            let mut b = [0u8; 16];
            let mut hs = [0u32; 2];
            let r = sys::channel_read(
                devmgr_ch,
                b.as_mut_ptr(),
                b.len(),
                hs.as_mut_ptr(),
                hs.len(),
            );
            if r.status == 0 && r.a2 == 1 {
                if let Some((_, devmgr::Status::Ok)) = devmgr::decode_reply(&b[..r.a1]) {
                    let me = sys::process_self().a1 as u32;
                    rtc = sys::process_map(me, hs[0], RTC_VA, 0, 4096, PF_R).status == 0;
                }
            }
        }
    }
    let p = sys::port_create();
    if p.status != 0 {
        return 5;
    }
    let pool = Limits {
        conns: 32,
        held_bytes: 32 * 1024,
        inflight: 32,
        stream_bytes: 0,
    };
    let mut d = Clockd {
        core: Core::new(
            p.a1 as u32,
            pool,
            rt::abi::limits::MAX_MSG_BYTES,
            rt::abi::limits::MAX_MSG_HANDLES,
            30_000,
        ),
        model: ClockModel::unset(mono_ns()),
        rtc,
        next_sync: 0,
    };
    d.resync();
    // publish `clock` (a published name is served as a listener: forwarded opens arrive on `mine`)
    let mut fs = rt::fs::Fs::new(registry);
    let c = sys::channel_create();
    if c.status != 0 {
        return 6;
    }
    let (mine, theirs) = (c.a1 as u32, c.a2 as u32);
    if fs.publish(b"clock", theirs).is_err() {
        return 7;
    }
    if d.core.add_conn(mine, svc::ROOT, Conn::Listener).is_none() {
        return 8;
    }
    say!("clockd: up, rtc={}", u8::from(rtc));
    service::run(&mut d)
}
