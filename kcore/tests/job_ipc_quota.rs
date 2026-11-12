//! Per-job accounting of kernel objects and queued IPC bytes (S4.5c). An object's creator pays
//! for the object; the job that created an endpoint pays for what is queued in it.
use abi::status::Status;
use abi::Handle;
use kcore::handle::HandleTable;
use kcore::ipc::{self, Kobjs, Quota};
use kcore::job::{JobId, Jobs, Limits, Resource, UNLIMITED};

fn limits(objects: u64, ipc_bytes: u64) -> Limits {
    Limits {
        objects,
        ipc_bytes,
        ..Limits::UNLIMITED
    }
}

struct W {
    reg: Kobjs,
    table: HandleTable,
    jobs: Jobs,
}

fn world() -> W {
    W {
        reg: Kobjs::new(),
        table: HandleTable::new(4000),
        jobs: Jobs::new(),
    }
}

impl W {
    fn channel(&mut self, job: JobId) -> Result<(Handle, Handle), Status> {
        let mut q = Quota {
            jobs: &mut self.jobs,
            job: Some(job),
        };
        ipc::channel_create_in(&mut self.reg, &mut self.table, &mut q)
    }
    fn socket(&mut self, job: JobId) -> Result<(Handle, Handle), Status> {
        let mut q = Quota {
            jobs: &mut self.jobs,
            job: Some(job),
        };
        ipc::socket_create_in(&mut self.reg, &mut self.table, &mut q)
    }
    fn send(&mut self, ch: Handle, n: usize) -> Result<(), Status> {
        ipc::channel_write_in(
            &mut self.reg,
            &mut self.table,
            ch,
            &vec![7u8; n],
            &[],
            &mut self.jobs,
        )
    }
    fn recv(&mut self, ch: Handle) {
        ipc::channel_read_in(&mut self.reg, &mut self.table, ch, 4096, 0, &mut self.jobs)
            .expect("a message was queued");
    }
    fn used(&self, job: JobId, r: Resource) -> u64 {
        self.jobs.usage(job).unwrap().get(r)
    }
    /// Apply the credits of destroyed objects, as every charging call does first.
    fn settle(&mut self) {
        ipc::settle(&mut self.reg, &mut self.jobs);
    }
}

#[test]
fn objects_are_counted_refused_at_the_limit_and_credited_when_destroyed() {
    let mut w = world();
    let j = w.jobs.create(JobId::ROOT, limits(4, UNLIMITED)).unwrap();
    let (a, b) = w.channel(j).unwrap();
    let (_c, _d) = w.channel(j).unwrap();
    assert_eq!(w.used(j, Resource::Objects), 4);
    assert_eq!(w.channel(j).err(), Some(Status::NoMemory), "a fifth object");
    assert_eq!(w.used(j, Resource::Objects), 4, "a refusal charges nothing");
    assert_eq!(
        w.used(JobId::ROOT, Resource::Objects),
        4,
        "an ancestor counts its subtree"
    );
    // one end closed: that object dies, its credit is applied by the next charging call, but one
    // object of room is not enough for a pair
    w.table.close(&mut w.reg, a).unwrap();
    assert_eq!(w.channel(j).err(), Some(Status::NoMemory));
    w.table.close(&mut w.reg, b).unwrap();
    assert!(
        w.channel(j).is_ok(),
        "room for a pair once both ends are gone"
    );
    assert_eq!(w.used(j, Resource::Objects), 4);
}

#[test]
fn another_jobs_objects_do_not_count_and_uncharged_objects_never_credit() {
    let mut w = world();
    let small = w.jobs.create(JobId::ROOT, limits(2, UNLIMITED)).unwrap();
    let other = w
        .jobs
        .create(JobId::ROOT, limits(UNLIMITED, UNLIMITED))
        .unwrap();
    for _ in 0..50 {
        w.channel(other).unwrap();
    }
    assert!(
        w.channel(small).is_ok(),
        "50 objects elsewhere are not this job's"
    );
    // an uncharged object (the pure tests' convenience calls) is invisible to the ledger
    let (u, v) = ipc::channel_create(&mut w.reg, &mut w.table).unwrap();
    w.table.close(&mut w.reg, u).unwrap();
    w.table.close(&mut w.reg, v).unwrap();
    w.settle();
    assert_eq!(w.used(small, Resource::Objects), 2);
}

#[test]
fn queued_channel_bytes_are_charged_to_the_endpoint_job_and_credited_on_read() {
    let mut w = world();
    let j = w.jobs.create(JobId::ROOT, limits(UNLIMITED, 1000)).unwrap();
    let (tx, rx) = w.channel(j).unwrap();
    w.send(tx, 100).unwrap();
    let one = w.used(j, Resource::IpcBytes);
    assert!(one >= 100, "a message costs at least its bytes ({one})");
    let mut sent = 1;
    while w.send(tx, 100).is_ok() {
        sent += 1;
        assert!(sent < 20, "the job's budget never applied");
    }
    assert_eq!(w.send(tx, 100), Err(Status::NoMemory));
    assert!(w.used(j, Resource::IpcBytes) <= 1000);
    assert_eq!(sent as u64, 1000 / one, "as many as the budget holds");
    // reading gives each message's cost back, and then there is room again
    w.recv(rx);
    assert_eq!(w.used(j, Resource::IpcBytes), one * (sent - 1) as u64);
    assert!(w.send(tx, 100).is_ok());
    // destroying the receiving end gives back everything still queued in it
    w.table.close(&mut w.reg, rx).unwrap();
    w.table.close(&mut w.reg, tx).unwrap();
    w.settle();
    assert_eq!(w.used(j, Resource::IpcBytes), 0);
    assert_eq!(w.used(j, Resource::Objects), 0);
}

#[test]
fn a_writer_from_another_job_does_not_pay_but_cannot_exceed_the_receivers_budget() {
    // The creator of the conversation pays for both buffers: a service that is handed an end
    // cannot be made to spend more than the client's job allows, and cannot spend the client's
    // allowance on anything but that client's own queues.
    let mut w = world();
    let client = w.jobs.create(JobId::ROOT, limits(UNLIMITED, 500)).unwrap();
    let server = w
        .jobs
        .create(JobId::ROOT, limits(UNLIMITED, UNLIMITED))
        .unwrap();
    let (mine, theirs) = w.channel(client).unwrap();
    // the server writes replies into the client's queue: they are the client's to pay for
    let mut replies = 0;
    while w.send(theirs, 100).is_ok() {
        replies += 1;
        assert!(replies < 20);
    }
    assert!((1..6).contains(&replies));
    assert_eq!(
        w.used(server, Resource::IpcBytes),
        0,
        "the server paid nothing"
    );
    w.recv(mine);
    assert!(w.send(theirs, 100).is_ok(), "reading made room");
}

#[test]
fn a_socket_write_takes_what_the_job_has_room_for() {
    let mut w = world();
    let j = w.jobs.create(JobId::ROOT, limits(UNLIMITED, 1000)).unwrap();
    let (a, b) = w.socket(j).unwrap();
    let wr = |w: &mut W, n: usize| {
        ipc::socket_write_in(&mut w.reg, &w.table, a, &vec![1u8; n], &mut w.jobs)
    };
    assert_eq!(wr(&mut w, 600), Ok(600));
    assert_eq!(wr(&mut w, 600), Ok(400), "clipped to the room left");
    assert_eq!(wr(&mut w, 1), Err(Status::NoMemory));
    assert_eq!(w.used(j, Resource::IpcBytes), 1000);
    let got = ipc::socket_read_in(&mut w.reg, &w.table, b, 500, &mut w.jobs).unwrap();
    assert_eq!(got.len(), 500);
    assert_eq!(w.used(j, Resource::IpcBytes), 500);
    assert_eq!(wr(&mut w, 800), Ok(500));
    // dropping both ends credits what was left in the buffer
    w.table.close(&mut w.reg, a).unwrap();
    w.table.close(&mut w.reg, b).unwrap();
    w.settle();
    assert_eq!(w.used(j, Resource::IpcBytes), 0);
}

#[test]
fn random_operations_keep_the_ledger_equal_to_the_registrys_own_counts() {
    // Independent check: the objects and bytes the ledger says are charged equal what the
    // registry actually holds, after any mix of creating, writing, reading and closing.
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut rnd = move |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    let mut w = world();
    let j = w.jobs.create(JobId::ROOT, limits(60, 3000)).unwrap();
    let mut chans: Vec<(Handle, Handle)> = Vec::new();
    let mut socks: Vec<(Handle, Handle)> = Vec::new();
    for step in 0..30_000 {
        match rnd(7) {
            0 => {
                if let Ok(p) = w.channel(j) {
                    chans.push(p);
                }
            }
            1 => {
                if let Ok(p) = w.socket(j) {
                    socks.push(p);
                }
            }
            2 if !chans.is_empty() => {
                let (t, _) = chans[rnd(chans.len() as u64) as usize];
                let _ = w.send(t, rnd(300) as usize);
            }
            3 if !chans.is_empty() => {
                let (_, r) = chans[rnd(chans.len() as u64) as usize];
                let _ = ipc::channel_read_in(&mut w.reg, &mut w.table, r, 4096, 0, &mut w.jobs);
            }
            4 if !socks.is_empty() => {
                let (a, _) = socks[rnd(socks.len() as u64) as usize];
                let _ = ipc::socket_write_in(
                    &mut w.reg,
                    &w.table,
                    a,
                    &vec![2u8; rnd(400) as usize],
                    &mut w.jobs,
                );
            }
            5 if !socks.is_empty() => {
                let (_, b) = socks[rnd(socks.len() as u64) as usize];
                let _ = ipc::socket_read_in(&mut w.reg, &w.table, b, 300, &mut w.jobs);
            }
            _ if !chans.is_empty() || !socks.is_empty() => {
                if rnd(2) == 0 && !chans.is_empty() {
                    let (a, b) = chans.swap_remove(rnd(chans.len() as u64) as usize);
                    let _ = w.table.close(&mut w.reg, a);
                    if rnd(2) == 0 {
                        let _ = w.table.close(&mut w.reg, b);
                    } else {
                        chans.push((b, b)); // keep the other end around alone
                    }
                } else if !socks.is_empty() {
                    let (a, b) = socks.swap_remove(rnd(socks.len() as u64) as usize);
                    let _ = w.table.close(&mut w.reg, a);
                    let _ = w.table.close(&mut w.reg, b);
                }
            }
            _ => {}
        }
        w.settle();
        assert_eq!(
            w.used(j, Resource::Objects),
            w.reg.live() as u64,
            "step {step}: objects"
        );
        assert_eq!(
            w.used(j, Resource::IpcBytes),
            w.reg.meter() as u64,
            "step {step}: queued bytes"
        );
        assert!(w.used(j, Resource::Objects) <= 60 && w.used(j, Resource::IpcBytes) <= 3000);
    }
}
