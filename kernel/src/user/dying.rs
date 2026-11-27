//! The processes on their way out (T278): from the kill or the whole-process exit that ends one (`Ending`, still in the process table)
//! through its teardown (`Taken`: out of the table, its handles closing; `Iommu`: the device drain; `Memory`: its memory going back), until
//! its job is credited. A leak report reads this to say how long each surviving or vanished process has been ending and where its
//! teardown is, so a teardown that is slow is told from one that is stuck, and a process that is out of the table but still charged to
//! its job is named. Lock-free (a few atomics per slot), so any path can note a step whatever it holds; a diagnostic, never a decision.

use core::sync::atomic::{AtomicU64, Ordering};

/// How many processes can be ending at once before the record overflows (counted, not recorded).
const SLOTS: usize = 64;

/// Where a process is on its way out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Stage {
    /// Killed, or one of its threads ended the whole process; still in the table.
    Ending = 1,
    /// Out of the table (`teardown_take`): its handles are being closed and the unreachable objects swept.
    Taken = 2,
    /// The device domains it held being drained (`iommu::drain`).
    Iommu = 3,
    /// Its mappings, tables and VMOs going back (`teardown_memory`), then its job's credit.
    Memory = 4,
}

impl Stage {
    fn name(code: u64) -> &'static str {
        match code {
            1 => "ending",
            2 => "closing handles",
            3 => "draining devices",
            4 => "freeing memory",
            _ => "?",
        }
    }
}

struct Slot {
    /// The process (0: a free slot).
    pid: AtomicU64,
    /// The tick of the first note (the kill, the exit, or the teardown's start for a process that ended without either).
    since: AtomicU64,
    /// The tick the teardown took it out of the table (0: not yet).
    taken: AtomicU64,
    /// The stage, and the thread that noted it (`tid << 8 | stage`).
    stage: AtomicU64,
}

static RECORD: [Slot; SLOTS] = [const {
    Slot {
        pid: AtomicU64::new(0),
        since: AtomicU64::new(0),
        taken: AtomicU64::new(0),
        stage: AtomicU64::new(0),
    }
}; SLOTS];

/// Notes that could not be recorded because every slot was in use.
static OVERFLOW: AtomicU64 = AtomicU64::new(0);

/// Note that process `pid` has reached `stage` (a later stage replaces an earlier one; an earlier one never replaces a later, so a second
/// kill of a process already in teardown changes nothing).
pub(crate) fn note(pid: u64, stage: Stage) {
    let now = crate::thread::ticks();
    let word = (crate::thread::current() << 8) | stage as u64;
    let slot = RECORD
        .iter()
        .find(|s| s.pid.load(Ordering::Acquire) == pid)
        .or_else(|| {
            RECORD.iter().find(|s| {
                s.pid
                    .compare_exchange(0, pid, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                    && {
                        s.since.store(now, Ordering::Release);
                        s.taken.store(0, Ordering::Release);
                        s.stage.store(0, Ordering::Release);
                        true
                    }
            })
        });
    let Some(slot) = slot else {
        OVERFLOW.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if stage as u64 >= Stage::Taken as u64 {
        let _ = slot
            .taken
            .compare_exchange(0, now.max(1), Ordering::AcqRel, Ordering::Acquire);
    }
    let mut old = slot.stage.load(Ordering::Acquire);
    while old & 0xff <= stage as u64 {
        match slot
            .stage
            .compare_exchange(old, word, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => break,
            Err(now) => old = now,
        }
    }
}

/// Process `pid` is gone: its teardown has credited its job.
pub(crate) fn done(pid: u64) {
    for s in &RECORD {
        if s.pid.load(Ordering::Acquire) == pid {
            s.pid.store(0, Ordering::Release);
        }
    }
}

/// One line per process on its way out, for a leak report: `in_table` says whether the process is still in the table (the caller holds
/// the state to know). Times are in milliseconds (ticks) before now.
#[allow(clippy::large_stack_frames)] // KT13: audited, a failure-only report of the leak checks (the formatting's frame), run on the boot thread's stack after init has ended
pub(crate) fn report(in_table: impl Fn(u64) -> bool) -> alloc::vec::Vec<alloc::string::String> {
    let now = crate::thread::ticks();
    let mut out = alloc::vec::Vec::new();
    for s in &RECORD {
        let pid = s.pid.load(Ordering::Acquire);
        if pid == 0 {
            continue;
        }
        let (since, taken, stage) = (
            s.since.load(Ordering::Acquire),
            s.taken.load(Ordering::Acquire),
            s.stage.load(Ordering::Acquire),
        );
        out.push(alloc::format!(
            "leak: dying {pid}: {} (in the table: {}), noted by thread {}, ending for {} ms, out of the table for {}",
            Stage::name(stage & 0xff),
            in_table(pid),
            stage >> 8,
            now.saturating_sub(since),
            if taken == 0 {
                alloc::string::String::from("-")
            } else {
                alloc::format!("{} ms", now.saturating_sub(taken))
            }
        ));
    }
    let lost = OVERFLOW.load(Ordering::Relaxed);
    if lost != 0 {
        out.push(alloc::format!(
            "leak: dying: {lost} notes found the record full"
        ));
    }
    out
}

/// How many processes are on their way out (for the line a failed leak check prints after its wait).
pub(crate) fn count() -> usize {
    RECORD
        .iter()
        .filter(|s| s.pid.load(Ordering::Acquire) != 0)
        .count()
}
