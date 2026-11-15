//! `netsup`: the network stack's supervisor (S8, NET-SPEC 4.2 and 9). It holds everything that must outlive a child
//! (TIDY-UP T42's rule): the device claim and its DMA memory, the ring VMO, the doorbell events, and starts and restarts
//! the driver `netdrv` (and, in the full system, `netd` and `dhcpc`) under the restart-intensity policy of `supervise`.
//!
//! Startup: `ServiceDevmgr` (a connection to the device manager: the claim lives as long as this connection),
//! `BootImage` (to find the programs) and, in `stackless` mode (the self-tests act as the stack), `ServiceInit`: the
//! channel on which the stack-side handles are handed to whoever started us.
//!
//! The stack-side handles (what `netd` gets): the ring VMO, the receive pool **read-only and without the DMA right**, the
//! transmit pool read-write without it, the doorbell events and the stack's end of the driver control channel. The stack
//! never learns a physical address.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use netring::{ctl, state};
use proto::bootimg;
use proto::devmgr::{self, Status as DStatus};
use proto::startup::{Role, Startup};
use rt::abi::rights::Rights;
use rt::abi::signals;
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};
use rt::sys;
use supervise::{Action, Policy, Supervisor};

#[path = "../netshm.rs"]
mod netshm;
use netshm::Shm;

const PF_RW: u64 = 3;
const RING_VA: u64 = 0x510_0000;

fn dup(h: u32, rights: Rights) -> Option<u32> {
    let r = sys::handle_duplicate(h, rights.0);
    (r.status == 0).then_some(r.a1 as u32)
}

fn read_bin(bi: u32, name: &[u8]) -> Option<Vec<u8>> {
    let total = sys::vmo_get_size(bi).a1;
    let prefix = rt::spawn::read_image(bi, 0, (total as u64).min(128 * 1024))?;
    let entries = bootimg::parse_prefix(&prefix, total).ok()?;
    let e = entries.iter().find(|e| e.name == name)?;
    rt::spawn::read_image(bi, u64::from(e.offset), u64::from(e.size))
}

/// Everything held across child lifetimes.
struct Held {
    mmio: u32,
    rx: u32,
    tx: u32,
    q: u32,
    irq: Option<u32>,
    ring: u32,
    /// The doorbell events: the driver signals `to_stack`, the stack signals `to_driver`.
    to_stack: u32,
    to_driver: u32,
    #[allow(dead_code)] // (read by the status and flight-recorder code of the full supervisor)
    shm: Shm,
}

/// A running child: its process handle.
struct Child {
    proc: Option<u32>,
    name: &'static str,
    restarts: u32,
    last_exit: i32,
}

fn claim(chan: u32) -> Result<(Vec<u32>, devmgr::NetClaim), i32> {
    let req = devmgr::encode_claim_net(1, 0);
    if sys::channel_write(chan, req.as_ptr(), req.len(), core::ptr::null(), 0).status != 0 {
        return Err(10);
    }
    let w = sys::object_wait(chan, signals::READABLE, sys::clock_get() + 5000);
    if w.status != 0 {
        return Err(11);
    }
    let mut buf = [0u8; 64];
    let mut hs = [0u32; 8];
    let r = sys::channel_read(chan, buf.as_mut_ptr(), buf.len(), hs.as_mut_ptr(), hs.len());
    if r.status != 0 {
        return Err(12);
    }
    let handles: Vec<u32> = hs[..r.a2].to_vec();
    match devmgr::decode_claim_net_reply(&buf[..r.a1]) {
        Some((_, DStatus::Ok, Some(c))) if handles.len() >= devmgr::NET_CLAIM_HANDLES => {
            Ok((handles, c))
        }
        Some((_, DStatus::NoDevice, _)) => Err(13),
        Some((_, DStatus::Busy, _)) => Err(14),
        _ => Err(15),
    }
}

/// Start the driver on a fresh control channel; returns its process handle and the stack's end of the channel.
fn spawn_driver(h: &Held, image: &[u8]) -> Result<(u32, u32), i32> {
    let pair = sys::channel_create();
    if pair.status != 0 {
        return Err(20);
    }
    let (drv_end, stack_end) = (pair.a1 as u32, pair.a2 as u32);
    let sig_wait = Rights::SIGNAL.union(Rights::WAIT).union(Rights::TRANSFER);
    let mut handles: Vec<u32> = Vec::new();
    let mut roles: Vec<(Role, u32)> = Vec::new();
    let mut add = |role: Role, v: Option<u32>, handles: &mut Vec<u32>| -> bool {
        match v {
            Some(x) => {
                roles.push((role, handles.len() as u32));
                handles.push(x);
                true
            }
            None => false,
        }
    };
    let ok = add(Role::NetMmio, dup(h.mmio, Rights::SAME), &mut handles)
        & add(Role::NetRxDma, dup(h.rx, Rights::SAME), &mut handles)
        & add(Role::NetTxDma, dup(h.tx, Rights::SAME), &mut handles)
        & add(Role::NetQueueDma, dup(h.q, Rights::SAME), &mut handles)
        & add(Role::NetRing, dup(h.ring, Rights::SAME), &mut handles)
        & add(
            Role::NetEventToStack,
            dup(h.to_stack, Rights::SIGNAL.union(Rights::TRANSFER)),
            &mut handles,
        )
        & add(
            Role::NetEventToDriver,
            dup(h.to_driver, sig_wait),
            &mut handles,
        )
        & add(Role::NetCtl, Some(drv_end), &mut handles);
    if let Some(irq) = h.irq {
        add(Role::NetIrq, dup(irq, Rights::SAME), &mut handles);
    }
    let s = Startup {
        args: alloc::vec![b"netdrv".to_vec()],
        roles,
        ..Startup::default()
    };
    let Ok(bytes) = s.encode() else {
        for x in handles.into_iter().chain([stack_end]) {
            sys::handle_close(x);
        }
        return Err(21);
    };
    if !ok {
        for x in handles.into_iter().chain([stack_end]) {
            sys::handle_close(x);
        }
        return Err(22);
    }
    match rt::spawn::spawn(image, &bytes, &handles) {
        Ok(p) => Ok((p, stack_end)),
        Err(e) => {
            rt::println!("##NETSUP cannot start netdrv: {e:?}");
            sys::handle_close(stack_end);
            Err(23)
        }
    }
}

/// The stack-side handles, in the order `[ring, rx pool (read-only), tx pool, to_stack (wait), to_driver (signal), ctl]`.
fn stack_handles(h: &Held, ctl_end: u32) -> Option<Vec<u32>> {
    let ro = Rights::READ.union(Rights::MAP).union(Rights::TRANSFER);
    let rw = ro.union(Rights::WRITE);
    Some(alloc::vec![
        dup(h.ring, Rights::SAME)?,
        dup(h.rx, ro)?,
        dup(h.tx, rw)?,
        dup(
            h.to_stack,
            Rights::SIGNAL.union(Rights::WAIT).union(Rights::TRANSFER)
        )?,
        dup(h.to_driver, Rights::SIGNAL.union(Rights::TRANSFER))?,
        ctl_end,
    ])
}

#[no_mangle]
pub fn app_main() -> i32 {
    let env = match startup::read() {
        Ok(e) => e,
        Err(startup::EnvError::NoMessage) => return EXIT_NO_STARTUP,
        Err(_) => return EXIT_BAD_STARTUP,
    };
    let (Some(devmgr_ch), Some(bi)) =
        (env.handle(Role::ServiceDevmgr), env.handle(Role::BootImage))
    else {
        return 3;
    };
    let stackless = env
        .startup
        .args
        .get(1)
        .is_some_and(|a| a.as_slice() == b"stackless");
    let parent = env.handle(Role::ServiceInit);

    let (hs, c) = match claim(devmgr_ch) {
        Ok(x) => x,
        Err(code) => {
            rt::println!("##NETSUP no device (code {code})");
            return code;
        }
    };
    let _ = c;
    let irq = (c.flags & devmgr::NET_FLAG_IRQ != 0).then(|| hs[4]);
    // the ring VMO: committed up front, so mapping it in another job is allowed (OD81)
    let ring = sys::vmo_create(2);
    if ring.status != 0 || sys::vmo_commit(ring.a1 as u32, 0, 2 * 4096).status != 0 {
        return 16;
    }
    let ring = ring.a1 as u32;
    let me = sys::process_self().a1 as u32;
    if sys::process_map(me, ring, RING_VA, 0, 2 * 4096, PF_RW).status != 0 {
        return 17;
    }
    let shm = Shm {
        base: RING_VA as usize,
    };
    netring::Mem::store(&shm, ctl::MAGIC, ctl::MAGIC_VALUE);
    netring::Mem::store(&shm, ctl::VERSION, ctl::VERSION_VALUE);
    netring::Mem::store(&shm, ctl::SLOTS, netring::SLOTS as u32);
    netring::Mem::store(&shm, ctl::SLOT_SIZE, netring::SLOT_SIZE as u32);
    netring::Mem::store(&shm, ctl::HDR_LEN, netring::HDR_LEN as u32);
    netring::Mem::store(&shm, ctl::FRAME_MAX, netring::FRAME_MAX as u32);
    netring::Mem::store(&shm, ctl::STATE, state::NONE);
    let (e1, e2) = (sys::event_create(), sys::event_create());
    if e1.status != 0 || e2.status != 0 {
        return 18;
    }
    let held = Held {
        mmio: hs[0],
        rx: hs[1],
        tx: hs[2],
        q: hs[3],
        irq,
        ring,
        to_stack: e1.a1 as u32,
        to_driver: e2.a1 as u32,
        shm,
    };
    let Some(drv_image) = read_bin(bi, b"bin/netdrv") else {
        return 19;
    };
    let mut sup = Supervisor::new(Policy::NET);
    let mut drv = Child {
        proc: None,
        name: "netdrv",
        restarts: 0,
        last_exit: 0,
    };
    let (p, stack_end) = match spawn_driver(&held, &drv_image) {
        Ok(x) => x,
        Err(code) => return code,
    };
    drv.proc = Some(p);
    if stackless {
        // the self-tests are the stack: hand them its handles and let them drive
        let Some(parent) = parent else { return 3 };
        let Some(hs) = stack_handles(&held, stack_end) else {
            return 24;
        };
        let r = sys::channel_write(parent, b"netsup".as_ptr(), 6, hs.as_ptr(), hs.len());
        if r.status != 0 {
            return 25;
        }
        // supervise nothing more: wait until the driver ends or the parent goes away
        loop {
            let w = sys::object_wait(p, signals::TERMINATED, sys::clock_get() + 200);
            if w.status == 0 && w.a1 as u32 & signals::TERMINATED != 0 {
                let code = sys::process_info(p).a1 as i32;
                rt::println!("##NETSUP child=netdrv exit={code}");
                return 0;
            }
            if sys::object_signals(parent).a1 as u32 & signals::PEER_CLOSED != 0 {
                sys::process_kill(p, -1);
                let _ = sys::object_wait(p, signals::TERMINATED, sys::clock_get() + 2000);
                return 0;
            }
        }
    }
    // the full system arrives with `netd` (step 4)
    let _ = (&mut sup, &mut drv, Action::Down);
    drv.restarts += 0;
    drv.last_exit += 0;
    let _ = drv.name;
    let _ = stack_end;
    7
}
