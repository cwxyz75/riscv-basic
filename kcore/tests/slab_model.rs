//! The slab allocator against a reference (MM4): random allocation and free of mixed sizes over a fake memory, checking after every step
//! that no two live objects overlap, each is aligned to its class and inside one frame past the header, the bookkeeping agrees with the
//! memory (`audit`), double and stray frees are refused, and when everything is freed the frames come back (all but the one empty slab each
//! class keeps).
mod common;
use common::{Rng, SEEDS, STEPS};
use kcore::slab::{
    class_for, objects_per_slab, run_order, FrameSource, FreeError, Slab, SlabMem, CLASSES,
    CLASS_SIZES, HEADER_BYTES, MAX_SMALL,
};
use std::collections::{BTreeMap, BTreeSet};

const BASE: u64 = 0x4000_0000;
const FRAMES: usize = 64;

/// Memory as words: the whole arena, objects included (the test writes a pattern into each object to see overwrites).
struct Mem {
    words: Vec<u64>,
}

impl Mem {
    fn new() -> Mem {
        Mem {
            words: vec![0; FRAMES * 512],
        }
    }
    fn idx(&self, addr: u64) -> usize {
        assert!(
            addr >= BASE && addr.is_multiple_of(8),
            "bad address {addr:#x}"
        );
        ((addr - BASE) / 8) as usize
    }
}

impl SlabMem for Mem {
    fn read(&self, addr: u64) -> u64 {
        self.words[self.idx(addr)]
    }
    fn write(&mut self, addr: u64, value: u64) {
        let i = self.idx(addr);
        self.words[i] = value;
    }
}

/// Frames from the arena: lowest free first, so a frame given back is reused.
struct Frames {
    free: BTreeSet<u64>,
    out: BTreeSet<u64>,
}

impl Frames {
    fn new() -> Frames {
        Frames {
            free: (0..FRAMES as u64).map(|i| BASE + i * 4096).collect(),
            out: BTreeSet::new(),
        }
    }
}

impl FrameSource for Frames {
    fn alloc_frame(&mut self) -> Option<u64> {
        let f = *self.free.iter().next()?;
        self.free.remove(&f);
        assert!(self.out.insert(f), "a frame handed out twice");
        Some(f)
    }
    fn free_frame(&mut self, addr: u64) {
        assert!(addr.is_multiple_of(4096));
        assert!(self.out.remove(&addr), "a frame freed that was not out");
        assert!(self.free.insert(addr), "a frame freed twice");
    }
}

#[test]
fn classes_and_runs() {
    assert_eq!(class_for(1, 1), Some(0));
    assert_eq!(class_for(16, 8), Some(0));
    assert_eq!(class_for(17, 1), Some(1));
    assert_eq!(class_for(2048, 1), Some(7));
    assert_eq!(class_for(2049, 1), None);
    assert_eq!(class_for(8, 64), Some(2), "alignment raises the class");
    assert_eq!(class_for(8, 4096), None);
    assert_eq!(class_for(8, 3), None, "alignment is a power of two");
    assert_eq!(run_order(2049, 1), Some(0));
    assert_eq!(run_order(4096, 1), Some(0));
    assert_eq!(run_order(4097, 1), Some(1));
    assert_eq!(run_order(64 * 1024, 1), Some(4));
    assert_eq!(run_order(100, 8192), Some(1), "alignment of a run");
    for (class, &size) in CLASS_SIZES.iter().enumerate() {
        let n = objects_per_slab(class);
        assert!(
            n >= 1 && n * size + HEADER_BYTES <= 4096 + size,
            "class {class}"
        );
        assert!(n <= 252, "the bitmap holds 256 slots");
    }
    assert_eq!(MAX_SMALL, 2048);
}

#[test]
fn one_object_is_aligned_inside_its_slab_and_comes_back() {
    let (mut mem, mut frames, mut slab) = (Mem::new(), Frames::new(), Slab::new());
    for (class, &size) in CLASS_SIZES.iter().enumerate() {
        let a = slab.alloc(&mut mem, &mut frames, class).unwrap();
        assert_eq!(a % size as u64, 0, "aligned to the class");
        assert!(a % 4096 >= HEADER_BYTES as u64, "past the header");
        assert_eq!(slab.class_of(&mem, a), Some(class));
        slab.free(&mut mem, &mut frames, a).unwrap();
        slab.audit(&mem).unwrap();
    }
    // the one empty slab of each class is kept
    assert_eq!(slab.frames(), CLASSES);
    assert_eq!(slab.objects(), 0);
}

#[test]
fn bad_frees_are_refused_and_change_nothing() {
    let (mut mem, mut frames, mut slab) = (Mem::new(), Frames::new(), Slab::new());
    let a = slab.alloc(&mut mem, &mut frames, 3).unwrap(); // 128 bytes
    let b = slab.alloc(&mut mem, &mut frames, 3).unwrap();
    assert_eq!(
        slab.free(&mut mem, &mut frames, a + 8),
        Err(FreeError::Misaligned)
    );
    assert_eq!(
        slab.free(&mut mem, &mut frames, a & !4095),
        Err(FreeError::NotASlab),
        "the header is not an object"
    );
    assert_eq!(
        slab.free(&mut mem, &mut frames, BASE + 30 * 4096),
        Err(FreeError::NotASlab),
        "a frame that is no slab"
    );
    slab.free(&mut mem, &mut frames, a).unwrap();
    assert_eq!(
        slab.free(&mut mem, &mut frames, a),
        Err(FreeError::NotAllocated)
    );
    assert_eq!(slab.objects(), 1);
    slab.audit(&mem).unwrap();
    slab.free(&mut mem, &mut frames, b).unwrap();
}

#[test]
fn a_full_slab_leaves_the_list_and_returns_when_an_object_is_freed() {
    let (mut mem, mut frames, mut slab) = (Mem::new(), Frames::new(), Slab::new());
    let class = 7; // 2048: one object per slab
    let n = objects_per_slab(class);
    assert_eq!(n, 1);
    let a = slab.alloc(&mut mem, &mut frames, class).unwrap();
    let b = slab.alloc(&mut mem, &mut frames, class).unwrap();
    assert_ne!(a & !4095, b & !4095, "a second slab");
    assert_eq!(slab.frames_of(class), 2);
    slab.audit(&mem).unwrap();
    slab.free(&mut mem, &mut frames, a).unwrap();
    slab.free(&mut mem, &mut frames, b).unwrap();
    assert_eq!(slab.frames_of(class), 1, "one empty slab kept");
    slab.audit(&mem).unwrap();
}

#[test]
fn no_frame_means_none_and_nothing_changes() {
    let (mut mem, mut frames, mut slab) = (Mem::new(), Frames::new(), Slab::new());
    frames.free.clear(); // none left
    assert_eq!(slab.alloc(&mut mem, &mut frames, 0), None);
    assert_eq!(slab.frames(), 0);
}

#[test]
fn random_allocation_and_free_agree_with_a_reference() {
    for seed in 0..SEEDS {
        let mut rng = Rng::new(seed);
        let (mut mem, mut frames, mut slab) = (Mem::new(), Frames::new(), Slab::new());
        let mut live: BTreeMap<u64, (usize, u64)> = BTreeMap::new(); // addr -> (class, pattern)
        for step in 0..STEPS {
            let ctx = format!("seed {seed} step {step}");
            if rng.below(100) < 55 || live.is_empty() {
                let class = rng.below(CLASSES as u64) as usize;
                match slab.alloc(&mut mem, &mut frames, class) {
                    Some(a) => {
                        let size = CLASS_SIZES[class] as u64;
                        assert_eq!(a % size, 0, "{ctx}: aligned");
                        assert!(a % 4096 >= HEADER_BYTES as u64, "{ctx}: past the header");
                        assert!(a % 4096 + size <= 4096, "{ctx}: inside the frame");
                        // no overlap with any live object
                        let lo = live.range(..=a).next_back();
                        if let Some((&pa, &(pc, _))) = lo {
                            assert!(
                                pa + CLASS_SIZES[pc] as u64 <= a,
                                "{ctx}: overlaps the one before"
                            );
                        }
                        if let Some((&na, _)) = live.range(a + 1..).next() {
                            assert!(a + size <= na, "{ctx}: overlaps the one after");
                        }
                        let pattern = rng.next();
                        for w in 0..size / 8 {
                            mem.write(a + w * 8, pattern);
                        }
                        live.insert(a, (class, pattern));
                    }
                    None => assert!(frames.free.is_empty(), "{ctx}: refused with frames left"),
                }
            } else {
                let k = rng.below(live.len() as u64) as usize;
                let (&a, &(class, pattern)) = live.iter().nth(k).unwrap();
                for w in 0..CLASS_SIZES[class] as u64 / 8 {
                    assert_eq!(
                        mem.read(a + w * 8),
                        pattern,
                        "{ctx}: an object's contents were overwritten"
                    );
                }
                slab.free(&mut mem, &mut frames, a).unwrap();
                live.remove(&a);
                if rng.below(8) == 0 {
                    // a double free is refused
                    let again = slab.free(&mut mem, &mut frames, a);
                    assert!(
                        matches!(again, Err(FreeError::NotAllocated | FreeError::NotASlab)),
                        "{ctx}: {again:?}"
                    );
                }
            }
            assert_eq!(slab.objects(), live.len(), "{ctx}");
            assert_eq!(frames.out.len(), slab.frames(), "{ctx}: frames held");
            if step % 16 == 0 {
                slab.audit(&mem).unwrap_or_else(|e| panic!("{ctx}: {e}"));
            }
        }
        for (a, _) in std::mem::take(&mut live) {
            slab.free(&mut mem, &mut frames, a).unwrap();
        }
        slab.audit(&mem).unwrap();
        assert_eq!(slab.objects(), 0, "seed {seed}");
        assert!(
            slab.frames() <= CLASSES,
            "seed {seed}: more than one empty slab per class kept ({})",
            slab.frames()
        );
        assert_eq!(frames.out.len(), slab.frames(), "seed {seed}");
    }
}
