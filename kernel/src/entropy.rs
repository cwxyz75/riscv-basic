//! The kernel entropy pool (HD1, KERNEL-V1-SPEC 7.1): a ChaCha20 generator with fast key erasure (`rngcore::Drbg`) keyed at boot from the
//! device tree's seeds (`/chosen/rng-seed`, `/chosen/kaslr-seed`), the `seed` CSR on a hart that has Zkr, and the clock. It serves address
//! choices inside the kernel only; programs have `rngd`. Without a seed (none in the tree, no Zkr) the pool still produces bytes from the
//! clock, but `seeded()` is false and the kernel does not randomise (first-fit addresses), says so at boot and in the system data page.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use rngcore::pool::{self, Inputs};
use rngcore::Drbg;

/// The generator, behind a spinlock of its own class (`Entropy`, after `Heap`): taken for a draw and never held across a call out.
static POOL: klock::Spin<Option<Drbg>, crate::bkl::Machine> =
    klock::Spin::new(klock::Class::Entropy, 0, None);
static SEEDED: AtomicBool = AtomicBool::new(false);
/// Test builds only (`aslr=off` on the kernel line, HD2): every placement the kernel makes at random is first-fit instead, so a test can see
/// that the randomisation is what moves an address.
static ASLR_OFF: AtomicBool = AtomicBool::new(false);
/// The first eight bytes the pool produced after it was keyed, for the boot tests (`debug_stat` 107).
static FIRST: AtomicU64 = AtomicU64::new(0);

/// Does `cpu` (the tree's node of the boot hart) list the Zkr extension?
fn hart_has_zkr(nodes: &[kcore::fdt::Node], hart_id: usize) -> bool {
    nodes.iter().any(|n| {
        n.path.starts_with("/cpus/cpu")
            && n.cells("reg").last().map(|&r| r as usize) == Some(hart_id)
            && kcore::isa::extensions_of(n).iter().any(|e| e == "zkr")
    })
}

/// Up to 32 bytes from the `seed` CSR: 16 bits per read that says ES16, retrying while the source is gathering, giving up on a failure or
/// after a bounded number of reads.
fn read_zkr(out: &mut [u8; 32]) -> usize {
    let mut got = 0;
    let mut tries = 0u32;
    while got < out.len() && tries < 100_000 {
        tries = tries.saturating_add(1);
        let v = crate::arch::seed_csr_read();
        match (v >> 30) & 3 {
            0b10 => {
                if let Some(dst) = out.get_mut(got..got.saturating_add(2)) {
                    dst.copy_from_slice(&(v as u16).to_le_bytes());
                }
                got = got.saturating_add(2);
            }
            0b11 => break, // the source has failed
            _ => {}        // still testing or gathering: ask again
        }
    }
    got
}

/// Key the pool (boot hart, after the heap, the trap vector and the clock exist).
#[allow(clippy::large_stack_frames)] // KT13: boot or test only, on a stack nothing else is under
pub fn init(boot_hart_id: usize) {
    let blob = crate::dtb::dtb_blob();
    let nodes = blob
        .and_then(|b| kcore::fdt::parse(b).ok())
        .unwrap_or_default();
    let seeds = blob.and_then(kcore::fdt::chosen_seeds).unwrap_or_default();
    let slice = |r: Option<(usize, usize)>| -> &[u8] {
        match (blob, r) {
            (Some(b), Some((o, l))) => b.get(o..o.saturating_add(l)).unwrap_or(&[]),
            _ => &[],
        }
    };
    let mut zkr = [0u8; 32];
    let zkr_len = if hart_has_zkr(&nodes, boot_hart_id) {
        read_zkr(&mut zkr)
    } else {
        0
    };
    let (key, seeded) = pool::derive_key(&Inputs {
        rng_seed: slice(seeds.rng_seed),
        kaslr_seed: slice(seeds.kaslr_seed),
        zkr: zkr.get(..zkr_len).unwrap_or(&[]),
        time: crate::timer::now(),
    });
    // keyed in place inside the lock (no copy of the generator is left in this frame), then the key and the CSR bytes are wiped
    let mut key = key;
    let mut first = [0u8; 8];
    {
        let mut pool = POOL.lock();
        let d = pool.insert(Drbg::new());
        d.reseed(&key);
        // the first output, for the boot tests (a test build reports it; a release build does not)
        let _ = d.fill(&mut first);
    }
    FIRST.store(u64::from_le_bytes(first), Ordering::Relaxed);
    SEEDED.store(seeded, Ordering::Release);
    if !crate::RELEASE && crate::dtb::boot_arg_value("aslr=").as_deref() == Some("off") {
        ASLR_OFF.store(true, Ordering::Relaxed);
    }
    rngcore::drbg::wipe(&mut key);
    rngcore::drbg::wipe(&mut zkr);
    if seeded {
        if !crate::RELEASE {
            use core::fmt::Write;
            let _ = writeln!(
                crate::console::Console,
                "entropy: seeded (rng-seed {} bytes, kaslr-seed {} bytes, Zkr {zkr_len} bytes); first output {:#018x}",
                seeds.rng_seed.map_or(0, |r| r.1),
                seeds.kaslr_seed.map_or(0, |r| r.1),
                u64::from_le_bytes(first)
            );
        }
    } else {
        use core::fmt::Write;
        let _ = writeln!(
            crate::console::Console,
            "entropy: unseeded (no rng-seed, kaslr-seed or Zkr): the kernel will not randomise addresses"
        );
    }
}

/// Did the pool get a real seed? When false, the kernel chooses addresses first-fit.
pub fn seeded() -> bool {
    SEEDED.load(Ordering::Acquire)
}

/// Fill `out` from the pool (at most 64 KiB; false if the pool is not keyed yet or the request is too long).
pub fn fill(out: &mut [u8]) -> bool {
    POOL.lock().as_mut().is_some_and(|d| d.fill(out))
}

/// A number below `n` for the placement of a mapping: 0 (the first fit) when the pool is not seeded or `aslr=off` was given.
pub fn placement_below(n: u64) -> u64 {
    if ASLR_OFF.load(Ordering::Relaxed) {
        return 0;
    }
    below(n).unwrap_or(0)
}

/// Is `aslr=off` in force (test builds)?
pub fn aslr_off() -> bool {
    ASLR_OFF.load(Ordering::Relaxed)
}

/// A uniform number below `n` (`None` for 0, or while the pool is not keyed).
pub fn below(n: u64) -> Option<u64> {
    let mut pool = POOL.lock();
    let d = pool.as_mut()?;
    pool::below(|b| d.fill(b), n)
}

/// The first output after keying (test builds only).
pub fn first_output() -> u64 {
    if crate::RELEASE {
        0
    } else {
        FIRST.load(Ordering::Relaxed)
    }
}
