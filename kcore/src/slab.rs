//! The slab allocator's core (MM4, KERNEL-V1-SPEC 4.2): size classes of 16 to 2048 bytes, one frame per slab, a header at the start of each
//! slab frame, a list of the slabs that have free objects per class. The module is pure: it reaches memory only through [`SlabMem`] (the
//! kernel implements it over the direct map, the host tests over an array) and gets frames from a [`FrameSource`]. It never allocates.
//!
//! **Layout.** A slab is one 4096-byte frame, aligned to 4096, split into `4096 / class` slots of `class` bytes; slot `i` is at offset
//! `i * class`, so an object is aligned to its class size (a power of two). The first `ceil(HEADER_BYTES / class)` slots hold the header and
//! are never handed out. The header is eight words: the class, the number of free objects, the next and previous slab on the class's list,
//! and a 256-bit allocation bitmap (a set bit is an object in use; the reserved slots count as in use). From an object's address the slab
//! is `addr & !4095`, and the object's slot is the offset divided by the class size.
//!
//! **Lists.** A slab with at least one free object and at least one in use is *partial*; one with none free is *full* and on no list; one
//! with all free is *empty*. Allocation takes from the head of the class's partial list (a new slab when there is none). A free puts a full
//! slab back on the list; a slab that becomes empty is returned to the frame source unless it is the class's only slab with room (one empty
//! slab per class is kept, so a loop that allocates and frees one object does not bounce a frame each time).
//!
//! Requests that no class can serve (above 2048 bytes, or an alignment above 2048) are not the slab's: the kernel gives them whole runs of
//! frames from the buddy allocator ([`run_order`]).

#![allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)] // INVARIANT: addresses are 4096-aligned frame addresses plus offsets below 4096 and small header-word offsets, far below u64::MAX; class indices are checked against CLASSES before use, slots against the slab's slot count, and the counters are bounded by the frames and objects in existence

use crate::frame::PAGE_SIZE;

/// Bytes of the frame, as a usize.
const FRAME: usize = PAGE_SIZE as usize;
/// The size classes, in bytes.
pub const CLASS_SIZES: [usize; 8] = [16, 32, 64, 128, 256, 512, 1024, 2048];
/// The number of size classes.
pub const CLASSES: usize = CLASS_SIZES.len();
/// The largest request a class serves.
pub const MAX_SMALL: usize = 2048;
/// Bytes of the slab header (eight words).
pub const HEADER_BYTES: usize = 64;
/// A tag in the high half of the header's first word: a slab frame says so (`free` finds a header that is not one, a stray or double free).
const MAGIC: u64 = 0x5148_4c41; // "SLAB"-ish; any fixed pattern

const W_CLASS: u64 = 0;
const W_FREE: u64 = 8;
const W_NEXT: u64 = 16;
const W_PREV: u64 = 24;
const W_BITMAP: u64 = 32;
/// Words of bitmap: 256 slots (a 16-byte class).
const BITMAP_WORDS: usize = 4;

/// Memory as the slab sees it: words at addresses (the header and nothing else; objects are the caller's).
pub trait SlabMem {
    fn read(&self, addr: u64) -> u64;
    fn write(&mut self, addr: u64, value: u64);
}

/// Where the slabs' frames come from and go back to.
pub trait FrameSource {
    /// A frame: its 4096-aligned address, content unspecified. `None` if there is none.
    fn alloc_frame(&mut self) -> Option<u64>;
    /// Give a frame back (it was returned by `alloc_frame` and holds nothing live).
    fn free_frame(&mut self, addr: u64);
}

/// Why a free was refused (a bug in the caller, reported, never a panic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreeError {
    /// The address is not inside a slab (the frame has no slab header, or the address is in the header).
    NotASlab,
    /// The address is not the start of an object of its class.
    Misaligned,
    /// The object is not in use.
    NotAllocated,
}

/// The size class for a request, if a class serves it: the smallest class that holds `size` bytes at `align`. `None` for a request above
/// 2048 bytes or aligned above 2048 (and for an alignment that is not a power of two).
pub fn class_for(size: usize, align: usize) -> Option<usize> {
    if !align.is_power_of_two() {
        return None;
    }
    let need = size.max(align).max(1);
    CLASS_SIZES.iter().position(|&c| c >= need)
}

/// The order (a power of two of frames) of the run of frames a request too big for the slab gets: enough frames for `size` bytes, aligned
/// to `align`. `None` if `align` is not a power of two or the run would be absurd (above order 30).
pub fn run_order(size: usize, align: usize) -> Option<usize> {
    if !align.is_power_of_two() {
        return None;
    }
    let bytes = size.max(align).max(1).checked_next_power_of_two()?;
    let frames = bytes.div_ceil(FRAME);
    let order = frames.trailing_zeros() as usize;
    (order <= 30).then_some(order)
}

/// Header slots a class reserves (the slots the header overlaps).
fn reserved_slots(class: usize) -> usize {
    let size = CLASS_SIZES.get(class).copied().unwrap_or(FRAME);
    HEADER_BYTES.div_ceil(size)
}

/// Objects one slab of the class holds.
pub fn objects_per_slab(class: usize) -> usize {
    let size = CLASS_SIZES.get(class).copied().unwrap_or(FRAME);
    FRAME / size - reserved_slots(class)
}

/// The partial-slab list heads and counters, one per class. All-zero (`new`) is the empty allocator.
#[derive(Debug, Clone, Copy, Default)]
pub struct Slab {
    /// The first slab with free objects, by class (0: none).
    partial: [u64; CLASSES],
    /// Slabs in existence, by class.
    slabs: [usize; CLASSES],
    /// Objects in use, by class.
    in_use: [usize; CLASSES],
}

impl Slab {
    pub const fn new() -> Slab {
        Slab {
            partial: [0; CLASSES],
            slabs: [0; CLASSES],
            in_use: [0; CLASSES],
        }
    }

    /// Slab frames held, over all classes.
    pub fn frames(&self) -> usize {
        self.slabs.iter().sum()
    }

    /// Objects in use, over all classes.
    pub fn objects(&self) -> usize {
        self.in_use.iter().sum()
    }

    /// Slab frames of one class.
    pub fn frames_of(&self, class: usize) -> usize {
        self.slabs.get(class).copied().unwrap_or(0)
    }

    /// Objects in use of one class.
    pub fn objects_of(&self, class: usize) -> usize {
        self.in_use.get(class).copied().unwrap_or(0)
    }

    /// Allocate one object of `class`: its address, aligned to the class size. `None` if there is no free object and no frame for a new
    /// slab.
    pub fn alloc<M: SlabMem, F: FrameSource>(
        &mut self,
        mem: &mut M,
        frames: &mut F,
        class: usize,
    ) -> Option<u64> {
        if class >= CLASSES {
            return None;
        }
        if self.partial[class] == 0 {
            let frame = frames.alloc_frame()?;
            self.new_slab(mem, frame, class);
        }
        let slab = self.partial[class];
        let size = CLASS_SIZES[class] as u64;
        // the first free slot
        let mut slot = None;
        for w in 0..BITMAP_WORDS {
            let bits = mem.read(slab + W_BITMAP + 8 * w as u64);
            if bits != u64::MAX {
                slot = Some(w * 64 + (!bits).trailing_zeros() as usize);
                break;
            }
        }
        let slot = slot?;
        let word = slab + W_BITMAP + 8 * (slot / 64) as u64;
        mem.write(word, mem.read(word) | (1u64 << (slot % 64)));
        let free = mem.read(slab + W_FREE) - 1;
        mem.write(slab + W_FREE, free);
        if free == 0 {
            self.unlink(mem, class, slab); // full: on no list
        }
        self.in_use[class] += 1;
        Some(slab + slot as u64 * size)
    }

    /// Free the object at `addr`. The class is read from the slab's header. A slab that becomes empty goes back to `frames` unless it is the
    /// only one of its class with room.
    pub fn free<M: SlabMem, F: FrameSource>(
        &mut self,
        mem: &mut M,
        frames: &mut F,
        addr: u64,
    ) -> Result<(), FreeError> {
        let slab = addr & !(PAGE_SIZE - 1);
        let tag = mem.read(slab + W_CLASS);
        if tag >> 32 != MAGIC {
            return Err(FreeError::NotASlab);
        }
        let class = (tag & 0xffff_ffff) as usize;
        if class >= CLASSES {
            return Err(FreeError::NotASlab);
        }
        let size = CLASS_SIZES[class] as u64;
        let offset = addr - slab;
        if !offset.is_multiple_of(size) {
            return Err(FreeError::Misaligned);
        }
        let slot = (offset / size) as usize;
        if slot < reserved_slots(class) {
            return Err(FreeError::NotASlab);
        }
        let word = slab + W_BITMAP + 8 * (slot / 64) as u64;
        let bits = mem.read(word);
        if bits & (1u64 << (slot % 64)) == 0 {
            return Err(FreeError::NotAllocated);
        }
        mem.write(word, bits & !(1u64 << (slot % 64)));
        let free = mem.read(slab + W_FREE) + 1;
        mem.write(slab + W_FREE, free);
        self.in_use[class] -= 1;
        if free == 1 {
            self.push(mem, class, slab); // it was full: it has room again
        }
        if free as usize == objects_per_slab(class) {
            // empty: keep it only if it is the one slab of the class with room
            let others = self.partial_head_other_than(mem, class, slab);
            if others {
                self.unlink(mem, class, slab);
                self.slabs[class] -= 1;
                mem.write(slab + W_CLASS, 0); // no longer a slab: a stray free of one of its objects is caught
                frames.free_frame(slab);
            }
        }
        Ok(())
    }

    /// The class of the slab holding `addr`, if it is a slab object address (for `dealloc`, which must tell a slab object from a frame run).
    pub fn class_of<M: SlabMem>(&self, mem: &M, addr: u64) -> Option<usize> {
        let tag = mem.read((addr & !(PAGE_SIZE - 1)) + W_CLASS);
        (tag >> 32 == MAGIC && ((tag & 0xffff_ffff) as usize) < CLASSES)
            .then_some((tag & 0xffff_ffff) as usize)
    }

    /// Start a slab of `class` in `frame` and put it on the class's list.
    fn new_slab<M: SlabMem>(&mut self, mem: &mut M, frame: u64, class: usize) {
        mem.write(frame + W_CLASS, (MAGIC << 32) | class as u64);
        mem.write(frame + W_FREE, objects_per_slab(class) as u64);
        let mut bits = [0u64; BITMAP_WORDS];
        let total = FRAME / CLASS_SIZES[class];
        // slots the header covers are in use; so are the slots past the end (there are none: every class fills the frame exactly)
        for slot in 0..reserved_slots(class) {
            bits[slot / 64] |= 1u64 << (slot % 64);
        }
        // a class with fewer than 256 slots has no slot past `total`: mark the unused high bits as in use so the first-free scan never picks one
        for slot in total..BITMAP_WORDS * 64 {
            bits[slot / 64] |= 1u64 << (slot % 64);
        }
        for (w, b) in bits.iter().enumerate() {
            mem.write(frame + W_BITMAP + 8 * w as u64, *b);
        }
        self.slabs[class] += 1;
        self.push(mem, class, frame);
    }

    /// Put `slab` at the head of the class's partial list.
    fn push<M: SlabMem>(&mut self, mem: &mut M, class: usize, slab: u64) {
        let head = self.partial.get(class).copied().unwrap_or(0);
        mem.write(slab + W_NEXT, head);
        mem.write(slab + W_PREV, 0);
        if head != 0 {
            mem.write(head + W_PREV, slab);
        }
        if let Some(h) = self.partial.get_mut(class) {
            *h = slab;
        }
    }

    /// Take `slab` off the class's partial list.
    fn unlink<M: SlabMem>(&mut self, mem: &mut M, class: usize, slab: u64) {
        let next = mem.read(slab + W_NEXT);
        let prev = mem.read(slab + W_PREV);
        if prev != 0 {
            mem.write(prev + W_NEXT, next);
        } else if let Some(h) = self.partial.get_mut(class) {
            *h = next;
        }
        if next != 0 {
            mem.write(next + W_PREV, prev);
        }
        mem.write(slab + W_NEXT, 0);
        mem.write(slab + W_PREV, 0);
    }

    /// Does the class's partial list hold a slab other than `slab`?
    fn partial_head_other_than<M: SlabMem>(&self, mem: &M, class: usize, slab: u64) -> bool {
        let head = self.partial.get(class).copied().unwrap_or(0);
        if head == 0 {
            return false;
        }
        head != slab || mem.read(slab + W_NEXT) != 0
    }

    /// Check the allocator's books against the memory (for tests and diagnostics): every list is well formed, the counts agree.
    pub fn audit<M: SlabMem>(&self, mem: &M) -> Result<(), alloc::string::String> {
        use alloc::format;
        for class in 0..CLASSES {
            let mut on_list = 0usize;
            let mut at = self.partial[class];
            let mut prev = 0u64;
            while at != 0 {
                on_list += 1;
                if on_list > 1 << 24 {
                    return Err(format!("class {class}: the partial list loops"));
                }
                if mem.read(at + W_PREV) != prev {
                    return Err(format!("class {class}: a broken back link at {at:#x}"));
                }
                let free = mem.read(at + W_FREE) as usize;
                if free == 0 {
                    return Err(format!("class {class}: a full slab {at:#x} is on the list"));
                }
                if free > objects_per_slab(class) {
                    return Err(format!("class {class}: slab {at:#x} says {free} free"));
                }
                let mut used = 0usize;
                for w in 0..BITMAP_WORDS {
                    used += mem.read(at + W_BITMAP + 8 * w as u64).count_ones() as usize;
                }
                let total = BITMAP_WORDS * 64;
                if total - used != free {
                    return Err(format!(
                        "class {class}: slab {at:#x} counts {free} free but its bitmap says {}",
                        total - used
                    ));
                }
                prev = at;
                at = mem.read(at + W_NEXT);
            }
            if on_list > self.slabs[class] {
                return Err(format!("class {class}: more slabs on the list than exist"));
            }
        }
        Ok(())
    }
}
