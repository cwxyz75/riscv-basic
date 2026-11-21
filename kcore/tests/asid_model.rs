//! L18: the ASID allocator across generation changes against TLB1 per generation, as a model of what the harts cache.
//!
//! The model is a TLB per hart: a set of (asid, owner) pairs, filled when an address space runs on the hart and emptied by a flush. The
//! rule the allocator and `HartGen` must keep: a hart never has two different owners cached under one ASID, because a hart that is about to
//! install a tag of a newer generation flushes first. Also: ASIDs are unique among current tags, never 0, not reused within a generation.
use kcore::asid::{AsTag, Asids, HartGen};
use std::collections::{BTreeMap, BTreeSet};

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

#[test]
fn asids_are_unique_in_a_generation_never_zero_and_a_new_generation_starts_afresh() {
    for bits in [1u32, 2, 3, 5, 16] {
        let mut a = Asids::new(bits);
        let per_gen = (1u64 << bits) - 1;
        let mut seen = BTreeSet::new();
        let g0 = a.generation();
        for i in 0..per_gen {
            let t = a.alloc();
            assert_eq!(t.gen, g0, "bits {bits}, alloc {i}");
            assert_ne!(t.asid, 0, "0 is the kernel's");
            assert!(seen.insert(t.asid), "no ASID twice in a generation");
        }
        let t = a.alloc();
        assert_eq!(t.gen, g0 + 1, "the space is used up: a new generation");
        assert_eq!(t.asid, 1, "and starts afresh");
    }
}

#[test]
fn a_stale_tag_is_refreshed_and_the_kernels_never_is() {
    let mut a = Asids::new(2); // 3 ASIDs per generation
    let mut t = a.alloc();
    assert!(!a.refresh(&mut t), "a current tag stays");
    for _ in 0..3 {
        a.alloc();
    }
    assert!(!a.is_current(t));
    let old = t;
    assert!(a.refresh(&mut t));
    assert!(a.is_current(t) && t != old);
    let mut k = AsTag::KERNEL;
    for _ in 0..10 {
        a.alloc();
    }
    assert!(!a.refresh(&mut k), "the kernel's space is never stale");
}

#[test]
fn a_hart_never_caches_two_owners_under_one_asid_when_it_flushes_before_a_newer_generation() {
    for seed in 1..=40u64 {
        let mut rng = Lcg(seed);
        let bits = 1 + (rng.next() % 3) as u32; // 1 to 3 bits: generations turn over fast
        let mut a = Asids::new(bits);
        let harts = 3usize;
        let mut hg: Vec<HartGen> = (0..harts).map(|_| HartGen::new()).collect();
        // what each hart has cached: asid -> owner
        let mut tlb: Vec<BTreeMap<u16, u32>> = vec![BTreeMap::new(); harts];
        let mut spaces: Vec<(u32, AsTag)> = vec![];
        let mut next_owner = 1u32;
        for _ in 0..3000 {
            match rng.next() % 4 {
                0 => {
                    // a process is made
                    spaces.push((next_owner, a.alloc()));
                    next_owner += 1;
                }
                1 if !spaces.is_empty() => {
                    // a process ends
                    let i = (rng.next() as usize) % spaces.len();
                    spaces.swap_remove(i);
                }
                _ if !spaces.is_empty() => {
                    // an address space runs on a hart: the kernel's switch path
                    let h = (rng.next() as usize) % harts;
                    let i = (rng.next() as usize) % spaces.len();
                    let (owner, tag) = &mut spaces[i];
                    a.refresh(tag);
                    if hg[h].needs_flush(tag.gen) {
                        tlb[h].clear();
                        hg[h].flushed(tag.gen);
                    }
                    // the space fills the hart's TLB under its ASID: nobody else may be there
                    let had = tlb[h].insert(tag.asid, *owner);
                    assert!(
                        had.is_none_or(|o| o == *owner),
                        "seed {seed}: hart {h} cached owner {had:?} and owner {owner} under ASID {}",
                        tag.asid
                    );
                }
                _ => {}
            }
        }
    }
}

#[test]
fn without_the_flush_two_owners_do_share_an_asid_so_the_flush_is_what_the_rule_needs() {
    // the negative control of the model: a hart that never flushes at a generation change caches two owners under one ASID
    let mut a = Asids::new(1); // one ASID per generation
    let mut tlb: BTreeMap<u16, u32> = BTreeMap::new();
    let first = a.alloc();
    tlb.insert(first.asid, 1);
    let second = a.alloc(); // a new generation, the same ASID
    assert_eq!(first.asid, second.asid);
    assert_ne!(first.gen, second.gen);
    assert!(
        tlb.insert(second.asid, 2).is_some(),
        "stale entry of owner 1 under owner 2's ASID"
    );
}
