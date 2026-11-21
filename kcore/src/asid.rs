//! The address-space identifier allocator (L18, KERNEL-V1-SPEC 5.4, SCHED-SMP-SPEC 3.10).
//!
//! An address space holds a [`AsTag`]: an ASID and the generation it was handed out in. ASIDs are unique among the tags of one generation
//! (0 belongs to the kernel and is never handed out) and are not reused within it: when the hart's ASID space is used up the generation
//! advances and ASIDs start again. A tag of an older generation is **stale**: its ASID may now belong to another address space, so the
//! holder must take a fresh tag ([`Asids::refresh`]) before it is next installed, and a hart that has not flushed since the generation
//! advanced must flush everything before it installs any tag of the new generation ([`HartGen`]). That is TLB1 per generation: an ASID is
//! in use by two address spaces on one hart only after that hart has flushed. There is no mode that flushes on every switch: a hart with a
//! short ASID field only advances generations more often.

/// An address space's identifier and the generation it was allocated in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AsTag {
    pub asid: u16,
    pub gen: u64,
}

impl AsTag {
    /// The kernel's own space: ASID 0, generation 0 (never stale: nothing else gets ASID 0).
    pub const KERNEL: AsTag = AsTag { asid: 0, gen: 0 };
}

/// The allocator: the ASIDs a hart implements, and where the current generation is up to.
#[derive(Debug)]
pub struct Asids {
    /// The highest ASID the harts implement (`2^bits - 1`, at most 0xffff).
    max: u16,
    gen: u64,
    /// The next ASID to hand out in this generation.
    next: u32,
}

impl Asids {
    /// An allocator for harts whose ASID field is `bits` wide (1 to 16: probed at boot). The first generation is 1 (generation 0 is the
    /// kernel's).
    #[allow(clippy::arithmetic_side_effects)] // INVARIANT: bits is clamped to 1..=16 on the next line, so 16 - bits does not underflow
    pub fn new(bits: u32) -> Asids {
        let bits = bits.clamp(1, 16);
        Asids {
            max: u16::MAX >> (16 - bits), // (bits is 1 to 16: 2^bits - 1)
            gen: 1,
            next: 1,
        }
    }

    /// The generation ASIDs are being handed out in now.
    pub fn generation(&self) -> u64 {
        self.gen
    }

    /// The highest ASID.
    pub fn max_asid(&self) -> u16 {
        self.max
    }

    /// A fresh tag. When this generation's ASIDs are used up the generation advances first.
    pub fn alloc(&mut self) -> AsTag {
        if self.next > u32::from(self.max) {
            self.gen = self.gen.saturating_add(1); // (a u64 counter: it does not wrap in any lifetime)
            self.next = 1;
        }
        let asid = self.next as u16; // (<= max <= 0xffff)
        self.next = self.next.saturating_add(1);
        AsTag {
            asid,
            gen: self.gen,
        }
    }

    /// Is `tag` of the current generation (so its ASID is its own)?
    pub fn is_current(&self, tag: AsTag) -> bool {
        tag.gen == self.gen
    }

    /// Give a stale tag a fresh one. Returns whether it changed. The kernel's tag is never stale.
    pub fn refresh(&mut self, tag: &mut AsTag) -> bool {
        if *tag == AsTag::KERNEL || self.is_current(*tag) {
            return false;
        }
        *tag = self.alloc();
        true
    }
}

/// What one hart knows: the generation it last flushed for. Before a tag of a newer generation is installed on the hart it must flush
/// everything ([`HartGen::needs_flush`]), then record it ([`HartGen::flushed`]).
#[derive(Debug, Default)]
pub struct HartGen {
    seen: u64,
}

impl HartGen {
    pub const fn new() -> HartGen {
        HartGen { seen: 0 }
    }

    /// Must the hart flush before it installs a tag of generation `gen`?
    pub fn needs_flush(&self, gen: u64) -> bool {
        gen > self.seen
    }

    /// The hart has flushed everything and now has nothing cached from before `gen`.
    pub fn flushed(&mut self, gen: u64) {
        self.seen = self.seen.max(gen);
    }
}
