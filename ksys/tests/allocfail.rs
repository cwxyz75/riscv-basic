//! The allocation-failure sweep of the syscall layer (KT9, KERNEL-V1-SPEC 7): each scenario (create an object of each kind, send and receive
//! a message with handles, bind a port, write a VMO, make a job, ...) is run with its N-th allocation made to fail, for N = 1, 2, 3, ... until
//! the scenario completes without reaching the N-th allocation. After each run the call must have succeeded or returned NO_MEMORY, and the
//! state must be as clean as after any other call: the audits of the fuzzer hold, and closing everything leaves nothing alive or charged.
//!
//! Rust's allocation APIs that cannot report failure (`Vec::push`, `Box::new`, `format!`, an infallible `reserve`) abort the process when
//! the allocator returns null. So each (scenario, N) runs in a **child process** (this test binary re-executed with `KALLOC_CHILD`): a
//! child that aborts has found an infallible allocation on that path, and the parent reports the scenario and N. Only allocations made while
//! the scenario's armed region runs are counted (the harness's own are not).
//!
//! `KALLOC_SCENARIO` limits the run to one scenario by name; `KALLOC_MAX` caps N (default 4000).
use abi::syscall as nr;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

mod common;
use common::*;

/// Counts the allocations made while armed and fails the one numbered `FAIL_AT`.
struct Failing;
static ARMED: AtomicUsize = AtomicUsize::new(0);
static COUNT: AtomicUsize = AtomicUsize::new(0);
static FAIL_AT: AtomicUsize = AtomicUsize::new(0);
static PERSIST: AtomicUsize = AtomicUsize::new(0);

fn should_fail() -> bool {
    if ARMED.load(Ordering::Relaxed) == 0 || EXEMPT.load(Ordering::Relaxed) != 0 {
        return false;
    }
    let c = COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    let at = FAIL_AT.load(Ordering::Relaxed);
    // one failure (the N-th), or, in the persistent mode (real exhaustion does not recover), the N-th and every allocation after it
    let fail = if PERSIST.load(Ordering::Relaxed) != 0 {
        c >= at
    } else {
        c == at
    };
    if fail {
        // (the backtrace allocates: stop failing while it is made) say where the failing allocation was, for the parent's report
        let was = ARMED.swap(0, Ordering::Relaxed);
        eprintln!("ALLOCSITE\n{}", std::backtrace::Backtrace::force_capture());
        ARMED.store(was, Ordering::Relaxed);
    }
    fail
}

// SAFETY: every method forwards to the system allocator, or returns null (which the contract allows) for the one allocation chosen to fail.
unsafe impl GlobalAlloc for Failing {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if should_fail() {
            return std::ptr::null_mut();
        }
        System.alloc(l)
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        if should_fail() {
            return std::ptr::null_mut();
        }
        System.alloc_zeroed(l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        if should_fail() {
            return std::ptr::null_mut();
        }
        System.realloc(p, l, n)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
}

#[global_allocator]
static A: Failing = Failing;

/// Start a scenario's count: the `n`-th allocation made while armed, across all its armed calls, fails.
fn start(n: usize) {
    COUNT.store(0, Ordering::Relaxed);
    FAIL_AT.store(n, Ordering::Relaxed);
}

fn arm() {
    ARMED.store(1, Ordering::Relaxed);
}

/// Stop counting for now; the allocations made while armed so far.
fn disarm() -> usize {
    ARMED.store(0, Ordering::Relaxed);
    COUNT.load(Ordering::Relaxed)
}

type Reply = ksys::Reply;
/// What a scenario calls to make an armed syscall.
type Go<'a> = &'a mut dyn FnMut(&mut World, usize, [u64; 6]) -> Reply;

/// A scenario: set up unarmed, then make the armed call(s) through `armed`, which returns the replies.
struct Scenario {
    name: &'static str,
    run: fn(&mut World, Go) -> Vec<Reply>,
}

const SAME: u64 = 1 << 31;

fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "channel_create",
            run: |w, go| vec![go(w, nr::CHANNEL_CREATE, [0; 6])],
        },
        Scenario {
            name: "socket_create",
            run: |w, go| vec![go(w, nr::SOCKET_CREATE, [0; 6])],
        },
        Scenario {
            name: "event_create",
            run: |w, go| vec![go(w, nr::EVENT_CREATE, [0; 6])],
        },
        Scenario {
            name: "port_create",
            run: |w, go| vec![go(w, nr::PORT_CREATE, [0; 6])],
        },
        Scenario {
            name: "vmo_create",
            run: |w, go| vec![go(w, nr::VMO_CREATE, [4 * 4096, 0, 0, 0, 0, 0])],
        },
        Scenario {
            name: "handle_duplicate_and_replace",
            run: |w, go| {
                let e = w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap();
                vec![
                    go(w, nr::HANDLE_DUPLICATE, [e, SAME, 0, 0, 0, 0]),
                    go(w, nr::HANDLE_REPLACE, [e, SAME, 0, 0, 0, 0]),
                ]
            },
        },
        Scenario {
            name: "channel_write_with_a_handle",
            run: |w, go| {
                let (x, _y) = w.pair(nr::CHANNEL_CREATE);
                let e = w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap();
                w.poke(BASE, b"hello");
                w.poke(BASE + 0x100, &e.to_le_bytes());
                vec![go(w, nr::CHANNEL_WRITE, [x, BASE, 5, BASE + 0x100, 1, 0])]
            },
        },
        Scenario {
            name: "channel_read_with_a_handle",
            run: |w, go| {
                let (x, y) = w.pair(nr::CHANNEL_CREATE);
                let e = w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap();
                w.poke(BASE, b"hello");
                w.poke(BASE + 0x100, &e.to_le_bytes());
                w.ok(nr::CHANNEL_WRITE, [x, BASE, 5, BASE + 0x100, 1, 0]);
                vec![go(
                    w,
                    nr::CHANNEL_READ,
                    [y, BASE + 0x200, 64, BASE + 0x300, 4, 0],
                )]
            },
        },
        Scenario {
            name: "channel_read_txid",
            run: |w, go| {
                let (x, y) = w.pair(nr::CHANNEL_CREATE);
                w.poke(BASE, &[7, 0, 0, 0, 1, 2, 3, 4]);
                w.ok(nr::CHANNEL_WRITE, [x, BASE, 8, BASE, 0, 0]);
                vec![go(
                    w,
                    nr::CHANNEL_READ_TXID,
                    [y, 7, BASE + 0x200, 64, BASE + 0x300, 4],
                )]
            },
        },
        Scenario {
            name: "socket_write_and_read",
            run: |w, go| {
                let (x, y) = w.pair(nr::SOCKET_CREATE);
                w.poke(BASE, b"abcdef");
                vec![
                    go(w, nr::SOCKET_WRITE, [x, BASE, 6, 0, 0, 0]),
                    go(w, nr::SOCKET_READ, [y, BASE + 0x400, 8, 0, 0, 0]),
                ]
            },
        },
        Scenario {
            name: "port_bind_rearm_unbind",
            run: |w, go| {
                let p = w.ok(nr::PORT_CREATE, [0; 6]).0.unwrap();
                let e = w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap();
                vec![
                    go(w, nr::PORT_BIND, [p, e, 1 << 24, 1, 0, 0]),
                    go(w, nr::PORT_REARM, [p, 1, 0, 0, 0, 0]),
                    go(w, nr::PORT_UNBIND, [p, 1, 0, 0, 0, 0]),
                ]
            },
        },
        Scenario {
            name: "vmo_write_read_commit",
            run: |w, go| {
                let v = w.ok(nr::VMO_CREATE, [4 * 4096, 0, 0, 0, 0, 0]).0.unwrap();
                w.poke(BASE, b"data");
                vec![
                    go(w, nr::VMO_WRITE, [v, 100, BASE, 4, 0, 0]),
                    go(w, nr::VMO_READ, [v, 100, BASE + 0x100, 4, 0, 0]),
                    go(w, nr::VMO_COMMIT, [v, 0, 4 * 4096, 0, 0, 0]),
                ]
            },
        },
        Scenario {
            name: "job_create_set_limits_info",
            run: |w, go| {
                use kcore::ipc::Quota;
                use kcore::job::JobId;
                let jh = {
                    let mut q = Quota {
                        jobs: &mut w.jobs,
                        job: Some(JobId::ROOT),
                    };

                    kcore::ipc::job_object_create(
                        &mut w.objs,
                        &mut w.handles,
                        JobId::ROOT,
                        abi::rights::Rights::JOB,
                        &mut q,
                    )
                    .unwrap()
                    .0
                };
                let limits: Vec<u8> = [u64::MAX; 6].iter().flat_map(|v| v.to_le_bytes()).collect();
                w.poke(BASE, &limits);
                vec![
                    go(w, nr::JOB_CREATE, [jh, BASE, 0, 0, 0, 0]),
                    go(w, nr::JOB_SET_LIMITS, [jh, BASE, 0, 0, 0, 0]),
                    go(w, nr::JOB_INFO, [jh, 0, 0, 0, 0, 0]),
                ]
            },
        },
        // (a tree's node is allocated only when it splits: the sweep must reach the creates that cross a node boundary, whatever the node size)
        Scenario {
            name: "thirty_vmo_creates",
            run: |w, go| {
                (0..30)
                    .map(|_| go(w, nr::VMO_CREATE, [4096, 0, 0, 0, 0, 0]))
                    .collect()
            },
        },
        Scenario {
            name: "fifty_handle_duplicates",
            run: |w, go| {
                let e = w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap();
                (0..50)
                    .map(|_| go(w, nr::HANDLE_DUPLICATE, [e, SAME, 0, 0, 0, 0]))
                    .collect()
            },
        },
        Scenario {
            name: "sixty_events_and_ports",
            run: |w, go| {
                (0..60)
                    .map(|i| {
                        go(
                            w,
                            if i % 2 == 0 {
                                nr::EVENT_CREATE
                            } else {
                                nr::PORT_CREATE
                            },
                            [0; 6],
                        )
                    })
                    .collect()
            },
        },
        Scenario {
            name: "closing_thirty_vmo_handles",
            run: |w, go| {
                let vs: Vec<u64> = (0..30)
                    .map(|_| w.ok(nr::VMO_CREATE, [4096, 0, 0, 0, 0, 0]).0.unwrap())
                    .collect();
                vs.iter()
                    .map(|&v| go(w, nr::HANDLE_CLOSE, [v, 0, 0, 0, 0, 0]))
                    .collect()
            },
        },
        Scenario {
            name: "closing_a_channel_holding_vmo_handles",
            run: |w, go| {
                let (x, y) = w.pair(nr::CHANNEL_CREATE);
                for _ in 0..8 {
                    let v = w.ok(nr::VMO_CREATE, [4096, 0, 0, 0, 0, 0]).0.unwrap();
                    w.poke(BASE + 0x100, &v.to_le_bytes());
                    w.ok(nr::CHANNEL_WRITE, [x, BASE, 0, BASE + 0x100, 1, 0]);
                }
                vec![go(w, nr::HANDLE_CLOSE, [y, 0, 0, 0, 0, 0])]
            },
        },
        Scenario {
            name: "a_garbage_cycle_is_swept",
            run: |w, go| {
                // y holds q and q holds y, in their queues: closing x and p leaves only the cycle
                let (x, y) = w.pair(nr::CHANNEL_CREATE);
                let (p, q) = w.pair(nr::CHANNEL_CREATE);
                w.poke(BASE + 0x100, &q.to_le_bytes());
                w.ok(nr::CHANNEL_WRITE, [x, BASE, 0, BASE + 0x100, 1, 0]);
                w.poke(BASE + 0x100, &y.to_le_bytes());
                w.ok(nr::CHANNEL_WRITE, [p, BASE, 0, BASE + 0x100, 1, 0]);
                vec![
                    go(w, nr::HANDLE_CLOSE, [x, 0, 0, 0, 0, 0]),
                    go(w, nr::HANDLE_CLOSE, [p, 0, 0, 0, 0, 0]),
                ]
            },
        },
        Scenario {
            name: "closing_a_channel_with_a_queued_handle",
            run: |w, go| {
                let (x, y) = w.pair(nr::CHANNEL_CREATE);
                let e = w.ok(nr::EVENT_CREATE, [0; 6]).0.unwrap();
                w.poke(BASE + 0x100, &e.to_le_bytes());
                w.ok(nr::CHANNEL_WRITE, [x, BASE, 0, BASE + 0x100, 1, 0]);
                vec![
                    go(w, nr::HANDLE_CLOSE, [y, 0, 0, 0, 0, 0]),
                    go(w, nr::HANDLE_CLOSE, [x, 0, 0, 0, 0, 0]),
                ]
            },
        },
    ]
}

fn dispatch(w: &mut World, n: usize, a: [u64; 6]) -> Reply {
    match ksys::wide_argument_error(n, &a) {
        Some(st) => Err((st, None, None)),
        None => w.call(n, a).expect("a handled call"),
    }
}

/// What the kernel does at the end of every syscall, armed like the call: hand the dead to their cleanup (a VMO's pages, a job's record) and
/// give the emptied vector back, and drain the dirty log.
fn armed_cleanup(w: &mut World) {
    // (T177: the sweep of what nothing can reach runs here too, under the failing allocator: it must give up without freeing anything reachable)
    let mut roots: Vec<kcore::object::ObjId> = Vec::new();
    if roots.try_reserve(w.handles.len()).is_ok() {
        roots.extend(w.handles.objects());
        w.objs.sweep(&roots);
    }
    let mut dead = w.objs.drain_dead();
    for o in dead.drain(..) {
        match o {
            kcore::ipc::KObj::Vmo(id) => {
                let _ = w.vmos.release(&mut w.mm, &mut w.jobs, id);
            }
            kcore::ipc::KObj::Job(id) => w.jobs.release(id),
            _ => {}
        }
    }
    w.objs.recycle(dead);
    let d = w.objs.take_dirty();
    w.objs.recycle_dirty(d);
}

/// The child: run one scenario with the `n`-th allocation failing (and, in the persistent mode, every one after it) and check what is left.
/// Prints `ALLOCS=<made>` and exits 0 on success.
struct Named {
    name: &'static str,
}

fn all_names() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = scenarios().iter().map(|s| s.name).collect();
    v.extend(pscenarios().iter().map(|s| s.name));
    v
}

fn child(name: &str, n: usize, persist: bool) {
    if pscenarios().iter().any(|s| s.name == name) {
        return pchild(name, n, persist);
    }
    let sc = scenarios()
        .into_iter()
        .find(|s| s.name == name)
        .expect("scenario");
    let mut w = world();
    PERSIST.store(usize::from(persist), Ordering::Relaxed);
    let mut made = 0;
    start(n);
    let mut go = |w: &mut World, nr: usize, a: [u64; 6]| -> Reply {
        arm();
        let r = dispatch(w, nr, a);
        armed_cleanup(w);
        let this = disarm() - made;
        // the call during which the failing allocation was reached must say NO_MEMORY (a later call may fail for what the failure left undone:
        // a read of a write that did not happen), and must have lost nothing: the same call made again, with memory, succeeds
        if made < n && n <= made + this {
            if let Err((st, _, _)) = &r {
                assert_eq!(*st, abi::status::Status::NoMemory, "scenario {name}, failing allocation {n}: the call returned {st:?}, not NO_MEMORY");
                made += this;
                let was = PERSIST.swap(0, Ordering::Relaxed);
                let again = dispatch(w, nr, a);
                PERSIST.store(was, Ordering::Relaxed);
                assert!(again.is_ok(), "scenario {name}, failing allocation {n}: after NO_MEMORY the same call, with memory, returned {again:?}: work was lost (call {nr} {a:?}, {} handles, room {})", w.handles.len(), w.handles.room());
                return r;
            }
        }
        made += this;
        r
    };
    let _ = (sc.run)(&mut w, &mut go);
    PERSIST.store(0, Ordering::Relaxed);
    cleanup(&mut w);
    audit(&mut w).unwrap_or_else(|e| panic!("scenario {name}, failing allocation {n}: {e}"));
    baseline(&mut w).unwrap_or_else(|e| panic!("scenario {name}, failing allocation {n}: {e}"));
    println!("ALLOCS={}", COUNT.load(Ordering::Relaxed));
}

#[test]
fn child_entry() {
    // (run by the parent below with KALLOC_CHILD=name:n; a plain `cargo test` skips it)
    if let Ok(v) = std::env::var("KALLOC_CHILD") {
        let mut parts = v.split(':');
        let (name, n) = (parts.next().unwrap(), parts.next().unwrap());
        child(name, n.parse().unwrap(), parts.next() == Some("persist"));
    }
}

#[test]
fn every_failing_allocation_is_reported_as_no_memory_and_leaves_the_state_clean() {
    let only = std::env::var("KALLOC_SCENARIO").ok();
    let max: usize = std::env::var("KALLOC_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4000);
    let exe = std::env::current_exe().unwrap();
    let mut findings = Vec::new();
    let mut summary = Vec::new();
    // (the persistent mode, where every allocation from the N-th on fails, doubles the run: it is the overnight target's, `KALLOC_PERSIST=1`)
    let modes: &[&str] = if std::env::var("KALLOC_PERSIST").is_ok() {
        &["", ":persist"]
    } else {
        &[""]
    };
    for (sc, mode) in modes
        .iter()
        .copied()
        .flat_map(|m| all_names().into_iter().map(move |s| (Named { name: s }, m)))
    {
        if only.as_deref().is_some_and(|o| o != sc.name) {
            continue;
        }
        let mut n = 1;
        loop {
            let out = std::process::Command::new(&exe)
                .args(["child_entry", "--exact", "--nocapture", "--test-threads=1"])
                .env("KALLOC_CHILD", format!("{}:{n}{mode}", sc.name))
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !out.status.success() {
                // the first frames in kcore or ksys of the failing allocation's backtrace name the site (the function, and the line when known)
                let frames: Vec<&str> = stderr
                    .lines()
                    .skip_while(|l| !l.starts_with("ALLOCSITE"))
                    .collect();
                let mut site: Vec<String> = Vec::new();
                for (k, l) in frames.iter().enumerate() {
                    let name = l.trim().split_once(": ").map(|x| x.1).unwrap_or("");
                    if (name.contains("kcore::") || name.contains("ksys::"))
                        && !name.contains("allocfail")
                        && !name.contains("alloc::")
                        && site.len() < 2
                    {
                        let at = frames
                            .get(k + 1)
                            .map(|a| {
                                a.trim()
                                    .trim_start_matches("at ")
                                    .rsplit("riscv-basic/")
                                    .next()
                                    .unwrap_or("")
                            })
                            .unwrap_or("");
                        site.push(format!(
                            "{} at {at}",
                            name.split("::<").next().unwrap_or(name)
                        ));
                    }
                }
                let why = if out.status.code().is_none() {
                    format!(
                        "the process aborted: an allocation on this path cannot fail ({})",
                        site.join(" <- ")
                    )
                } else {
                    stderr
                        .lines()
                        .find(|l| l.starts_with("scenario "))
                        .unwrap_or("the child failed")
                        .to_string()
                };
                findings.push(format!(
                    "scenario {}, failing allocation {n}: {why}",
                    sc.name
                ));
                n += 1;
                if n > max {
                    break;
                }
                continue;
            }
            // (libtest prints "test child_entry ... " before what the test prints, on the same line)
            let made: usize = stdout
                .lines()
                .find_map(|l| l.split_once("ALLOCS=").map(|(_, v)| v.trim().to_string()))
                .and_then(|v| v.parse().ok())
                .expect("the child reports how many allocations the scenario made");
            if made < n {
                // the scenario finished without reaching allocation n: every one of its allocations has been failed in turn
                summary.push(format!(
                    "{}: {made} allocations, each failed in turn",
                    sc.name
                ));
                break;
            }
            n += 1;
            if !mode.is_empty() && n > 14 {
                summary.push(format!(
                    "{}{mode}: the first 14 allocations, each failed from there on",
                    sc.name
                ));
                break;
            }
            assert!(n <= max, "scenario {} still allocating at {max}", sc.name);
        }
    }
    for s in &summary {
        eprintln!("{s}");
    }
    assert!(
        findings.is_empty(),
        "{} failing allocations:\n{}",
        findings.len(),
        findings.join("\n")
    );
}

// ---- the process-table calls (T177): a world with two processes, as `proc_calls.rs` has ----

struct Tlb;
impl ksys::Machine for Tlb {
    fn shootdown(&mut self) {}
    fn flush_tlb(&mut self) {}
    fn icache_sync(&mut self, _pages: u64) {}
    fn random_below(&mut self, _n: u64) -> u64 {
        0
    }
    fn thread_cpu_ns(&self, tid: kcore::sched::Tid) -> u64 {
        tid * 1000
    }
}

struct PW {
    objs: kcore::ipc::Kobjs,
    procs: ksys::ProcTable,
    mm: kcore::Mm<FakeMem>,
    vmos: kcore::aspace::Vmos,
    jobs: kcore::job::Jobs,
}

fn pprocess(
    mm: &mut kcore::Mm<FakeMem>,
    kernel: &kcore::pagetable::PageTable,
    pid: u64,
) -> ksys::Process {
    let mut space = kcore::aspace::AddressSpace::new(
        kcore::pagetable::PageTable::new_with_kernel_half(mm, kernel).unwrap(),
    );
    space.set_owner_fresh(pid);
    ksys::Process {
        live_tids: Default::default(),
        aspace: std::sync::Arc::new(klock::aslock::AsCell::new(pid, space)),
        handles: kcore::handle::HandleTable::new(64),
        tag: kcore::asid::AsTag { asid: 1, gen: 1 },
        vmos: Vec::new(),
        entry: 0,
        arg: 0,
        exit_code: None,
        kernel_end: None,
        exc_channel: None,
        start_sp: 0,
        obj: None,
        state: ksys::ProcState::Running,
        detached: false,
        killed: None,
        stopped: false,
        threads_alive: 0,
        job: kcore::job::JobId::ROOT,
        table_charge: kcore::fmap::FMap::new(),
        charge_end: kcore::fmap::FMap::new(),
        stacks: Default::default(),
        stack_hint: 0,
        privileged: false,
        libsys: Default::default(),
    }
}

fn pworld() -> PW {
    let mut mm = kcore::Mm {
        db: kcore::frame::FrameDb::new(3000, 400),
        mem: FakeMem::default(),
    };
    let kernel = kcore::pagetable::PageTable::new(&mut mm).unwrap();
    let mut procs = kcore::fmap::FMap::new();
    ksys::put_process(&mut procs, 1, pprocess(&mut mm, &kernel, 1));
    ksys::put_process(&mut procs, 2, pprocess(&mut mm, &kernel, 2));
    PW {
        objs: kcore::ipc::Kobjs::new(),
        procs,
        mm,
        vmos: kcore::aspace::Vmos::new(),
        jobs: kcore::job::Jobs::new(),
    }
}

impl PW {
    fn quota(jobs: &mut kcore::job::Jobs) -> kcore::ipc::Quota<'_> {
        kcore::ipc::Quota {
            jobs,
            job: Some(kcore::job::JobId::ROOT),
        }
    }
    /// A handle in process 1 to process 2, and one to a fresh VMO of `pages` pages.
    fn handles(&mut self, pages: u64) -> (u64, u64) {
        let p = self.procs.get_mut(&1).unwrap();
        let (ph, _) = kcore::ipc::process_object_create_in(
            &mut self.objs,
            &mut p.handles,
            2,
            false,
            &mut Self::quota(&mut self.jobs),
        )
        .unwrap();
        let id = self
            .vmos
            .create_in(&mut self.jobs, pages, kcore::job::JobId::ROOT);
        let vh = kcore::ipc::vmo_object_create_in(
            &mut self.objs,
            &mut p.handles,
            id,
            &mut Self::quota(&mut self.jobs),
        )
        .unwrap();
        (ph.0, vh.0)
    }
    fn call(&mut self, n: usize, a: [u64; 6]) -> Reply {
        let mut tlb = Tlb;
        let mut c = ksys::PCtx {
            objs: &mut self.objs,
            procs: &mut self.procs,
            me: 1,
            mm: &mut self.mm,
            vmos: &mut self.vmos,
            jobs: &mut self.jobs,
            machine: &mut tlb,
        };
        ksys::proc_call(n, a, &mut c).expect("a handled call")
    }
    /// What the kernel does at the end of every syscall, armed like the call: hand the dead to their cleanup.
    fn cleanup(&mut self) {
        for _ in 0..3 {
            let mut dead = self.objs.drain_dead();
            if dead.is_empty() {
                break;
            }
            for o in dead.drain(..) {
                match o {
                    kcore::ipc::KObj::Vmo(id) => {
                        let _ = self.vmos.release(&mut self.mm, &mut self.jobs, id);
                    }
                    kcore::ipc::KObj::Job(id) => self.jobs.release(id),
                    _ => {}
                }
            }
            self.objs.recycle(dead);
        }
        let d = self.objs.take_dirty();
        self.objs.recycle_dirty(d);
    }
}

type PGo<'a> = &'a mut dyn FnMut(&mut PW, usize, [u64; 6]) -> Reply;

struct PScenario {
    name: &'static str,
    run: fn(&mut PW, PGo),
}

const VA: u64 = 0x40_0000;
const RWX: u64 = 3;

fn pscenarios() -> Vec<PScenario> {
    vec![
        PScenario {
            name: "process_map",
            run: |w, go| {
                let (ph, v) = w.handles(4);
                let _ = go(w, nr::PROCESS_MAP, [ph, v, VA, 0, 4 * 4096, RWX]);
            },
        },
        PScenario {
            name: "process_map_unmap_protect",
            run: |w, go| {
                let (ph, v) = w.handles(4);
                let _ = go(w, nr::PROCESS_MAP, [ph, v, VA, 0, 4 * 4096, RWX]);
                let _ = go(w, nr::PROCESS_PROTECT, [ph, VA, 4096, 1, 0, 0]);
                let _ = go(w, nr::PROCESS_UNMAP, [ph, VA, 4 * 4096, 0, 0, 0]);
            },
        },
        PScenario {
            name: "process_map_info",
            run: |w, go| {
                let (ph, v) = w.handles(2);
                w.call(nr::PROCESS_MAP, [ph, v, VA, 0, 2 * 4096, RWX])
                    .unwrap();
                let _ = go(w, nr::PROCESS_INFO, [ph, 0, 0, 0, 0, 0]);
                let _ = go(w, nr::PROCESS_MAP_INFO, [ph, BASE_OUT, 4, 0, 0, 0]);
            },
        },
    ]
}

/// (an address the call would write to; the scenarios that use it are not required to succeed, only to say NO_MEMORY or succeed)
const BASE_OUT: u64 = 0x1_0000;

fn pchild(name: &str, n: usize, persist: bool) {
    let sc = pscenarios()
        .into_iter()
        .find(|s| s.name == name)
        .expect("scenario");
    let mut w = pworld();
    PERSIST.store(usize::from(persist), Ordering::Relaxed);
    let mut made = 0;
    start(n);
    let mut go = |w: &mut PW, nr: usize, a: [u64; 6]| -> Reply {
        arm();
        let r = match ksys::wide_argument_error(nr, &a) {
            Some(st) => Err((st, None, None)),
            None => w.call(nr, a),
        };
        w.cleanup();
        let this = disarm() - made;
        if made < n && n <= made + this {
            if let Err((st, _, _)) = &r {
                // (a bad argument of the scenario itself is its own status; only the call that hit the failure must say NO_MEMORY)
                assert!(
                    matches!(*st, abi::status::Status::NoMemory),
                    "scenario {name}, failing allocation {n}: the call returned {st:?}, not NO_MEMORY"
                );
                made += this;
                let was = PERSIST.swap(0, Ordering::Relaxed);
                let again = w.call(nr, a);
                w.cleanup();
                PERSIST.store(was, Ordering::Relaxed);
                assert!(
                    again.is_ok(),
                    "scenario {name}, failing allocation {n}: after NO_MEMORY the same call, with memory, returned {again:?}: work was lost (call {nr} {a:?})"
                );
                return r;
            }
        }
        made += this;
        r
    };
    (sc.run)(&mut w, &mut go);
    PERSIST.store(0, Ordering::Relaxed);
    // everything closed: nothing alive, nothing charged
    for p in w.procs.values_mut() {
        p.handles.drain(&mut w.objs);
    }
    w.cleanup();
    w.objs.sweep(&[]);
    w.cleanup();
    kcore::ipc::settle(&mut w.objs, &mut w.jobs);
    assert_eq!(
        w.objs.live(),
        0,
        "scenario {name}, failing allocation {n}: objects alive after every handle was closed"
    );
    for r in [
        kcore::job::Resource::Objects,
        kcore::job::Resource::IpcBytes,
    ] {
        let used = w
            .jobs
            .usage(kcore::job::JobId::ROOT)
            .map_or(0, |u| u.get(r));
        assert_eq!(
            used, 0,
            "scenario {name}, failing allocation {n}: the root job still holds {used} of {r:?}"
        );
    }
    println!("ALLOCS={}", COUNT.load(Ordering::Relaxed));
}
