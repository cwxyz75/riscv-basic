//! The program loader: `start` and its locked body (S3b).

use super::*;

/// Load `image` into a new process and start it with start argument `arg`. Returns the
/// process id and the id of its thread.
/// Build a process from `image` and start its first thread. The whole construction runs under the
/// big kernel lock (it touches the frame database, the VMO store and the process table).
pub fn start(image: &[u8], arg: u64) -> Result<(u64, Tid), LoadError> {
    irq::without(|| start_locked(image, arg))
}

#[allow(clippy::large_stack_frames)]
// KT13: audited, a frame over 1 KiB; T172 shrinks them (the boot-time high-water budget check bounds the sum)
#[allow(clippy::redundant_closure_call)]
// (the closures stand for the critical sections `irq::without` made: the caller holds the big lock, which must not be taken again after the memory locks)
#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing
)]
// INVARIANT: boot-time process start: the segments were checked by `elf::parse` (offsets and sizes inside the file, addresses in the user half); a pid is a u64 counter that never wraps (a start at its end is refused)
pub(super) fn start_locked(image: &[u8], arg: u64) -> Result<(u64, Tid), LoadError> {
    if image.is_empty() {
        return Err(LoadError::NoProgram);
    }
    let parsed = elf::parse(image).map_err(LoadError::Elf)?;
    if parsed.pie {
        return Err(LoadError::Elf(elf::ElfError::BadType)); // (placement of a static-PIE is HD2 part b)
    }
    if !kmem::kernel_half_unchanged() {
        return Err(LoadError::KernelHalfChanged);
    }
    // (the caller took the big kernel lock: the closures below run under it, and must not take it again after the memory locks `k` holds)
    let k = kmem::get();
    let table =
        PageTable::new_with_kernel_half(k.mm, &kmem::kernel_table()).ok_or(LoadError::NoMemory)?;
    let mut aspace = AddressSpace::new(table);
    let mut vmo_ids: Vec<u64> = Vec::new();
    let built = (|| -> Result<(), LoadError> {
        for seg in &parsed.segments {
            let (first, end) = seg.pages();
            let id = k.vmos.create(k.jobs, end - first);
            vmo_ids.push(id);
            let bytes = &image[seg.offset as usize..(seg.offset + seg.filesz) as usize];
            k.vmos
                .write(k.mm, k.jobs, id, seg.vaddr % 4096, bytes)
                .map_err(|_| LoadError::NoMemory)?;
            aspace
                .map_region(k.vmos, first * 4096, end - first, id, 0, seg.perms)
                .map_err(|_| LoadError::NoMemory)?;
        }
        let stack = k.vmos.create(k.jobs, STACK_PAGES);
        vmo_ids.push(stack);
        aspace
            .map_region(
                k.vmos,
                STACK_TOP - STACK_PAGES * 4096,
                STACK_PAGES,
                stack,
                0,
                Perms::RW.user(),
            )
            .map_err(|_| LoadError::NoMemory)?;
        Ok(())
    })();
    let (pid, tag, first_gen) = (|| {
        {
            let s = state_locked();
            let pid = s.next_pid;
            // (CNT1: the counter never wraps or repeats: at its end no process is started)
            // (the process backstop: the syscall layer checks it, and so does this)
            let ok = s.procs.len() < s.backstops.processes
                && pid.checked_add(1).map(|n| s.next_pid = n).is_some();
            (ok.then_some(pid), s.asids.alloc(), s.first_handle_gen)
        }
    })();
    let built = built.and_then(|()| pid.map(|_| ()).ok_or(LoadError::NoMemory));
    let pid = pid.unwrap_or(0);
    if let Err(e) = built {
        aspace.destroy_user(k.mm, k.vmos, k.jobs);
        for id in vmo_ids {
            let _ = k.vmos.release(k.mm, k.jobs, id);
        }
        return Err(e);
    }
    let process = Process {
        live_tids: Default::default(),
        aspace: alloc::sync::Arc::new(klock::aslock::AsCell::new(pid, aspace)),
        handles: HandleTable::new(MAX_HANDLES),
        tag,
        vmos: vmo_ids,
        entry: parsed.entry,
        arg,
        exit_code: None,
        kernel_end: None,
        exc_channel: None,
        start_sp: 0,
        obj: None,
        state: ProcState::Running,
        detached: false,
        killed: None,
        stopped: false,
        threads_alive: 1,
        thread_objs: BTreeMap::new(),
        job: JobId::ROOT,
        table_charge: BTreeMap::new(),
        charge_end: BTreeMap::new(),
        stacks: BTreeMap::new(),
        stack_hint: abi::user::STACK_ARENA_BASE,
        privileged: true,
    };
    // Every process starts with one handle, value 1: the bootstrap channel (SYSCALLS.md).
    let mut process = process;
    let made = (|| -> bool {
        let s = state_locked();
        let Ok((kernel_end, process_end)) = ipc::channel_create_in(
            &mut s.objs,
            &mut s.ktable,
            &mut ipc::Quota {
                jobs: &mut *k.jobs,
                job: Some(JobId::ROOT),
            },
        ) else {
            return false;
        };
        let entry = s
            .ktable
            .remove_for_transfer(&mut s.objs, process_end)
            .expect("a fresh handle can be transferred");
        let first = process
            .handles
            .insert(&mut s.objs, entry.obj, entry.rights)
            .expect("an empty table has room");
        assert_eq!(
            first,
            abi::BOOTSTRAP_HANDLE,
            "the bootstrap channel must be handle 1"
        );
        process.kernel_end = Some(kernel_end);
        // (AB3, test builds: the generations the later handles start at; the bootstrap handle above is value 1 by the ABI)
        process.handles.set_first_generation(first_gen);
        true
    })();
    if !made {
        klock::aslock::AsCell::take(process.aspace).destroy_user(k.mm, k.vmos, k.jobs);
        for id in process.vmos {
            let _ = k.vmos.release(k.mm, k.jobs, id);
        }
        return Err(LoadError::NoMemory);
    }
    let token = (|| {
        {
            let s = state_locked();
            let (entry, arg) = (process.entry, process.arg);
            // (the root job has no limits: these charges only keep its usage honest)
            let jobs = &mut *k.jobs;
            let _ = jobs.charge(JobId::ROOT, Resource::Processes, 1);
            let _ = charge_thread(jobs, JobId::ROOT);
            s.procs.insert(pid, Box::new(process));
            new_start(s, pid, entry, STACK_TOP, arg, None)
        }
    })();
    // (the thread's stack comes from the same locks)
    drop(k);
    // user threads may run on any hart (kernel threads stay on the boot hart by default)
    let tid = thread::spawn_free("user", 10, user_thread_main, token);
    Ok((pid, tid))
}
