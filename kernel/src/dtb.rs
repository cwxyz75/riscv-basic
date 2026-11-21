//! The device tree OpenSBI hands the kernel (in `a1`), read from the top of RAM.

use core::sync::atomic::{AtomicUsize, Ordering};

/// The device tree's physical address (the pointer firmware passed in `a1`).
pub static DTB: AtomicUsize = AtomicUsize::new(0);
/// Its length in bytes, from its header (0 until `locate` has read it).
static DTB_LEN: AtomicUsize = AtomicUsize::new(0);

/// Read the tree's header through the early boot table's direct map (which reaches all physical space the kernel supports, `boot.rs`) and
/// remember the extent: `kmem::init` keeps the tree's frames out of the allocator and maps them read-only. Returns `(address, length)`, or
/// `None` for no tree or one that does not look like a tree.
pub fn locate(dtb: usize) -> Option<(u64, usize)> {
    DTB.store(dtb, Ordering::Relaxed);
    // (a pointer the early table does not map would fault: refuse it with a message instead; 8 bytes of header must lie below the limit)
    if dtb == 0 || (dtb as u64).saturating_add(8) > abi::layout::DIRECT_MAP_PHYS_LIMIT {
        return None;
    }
    let head = (abi::layout::DIRECT_BASE + dtb as u64) as *const u8;
    // SAFETY: the early table maps physical addresses below 64 GiB through the direct map; a firmware pointer is a physical address inside
    // RAM, so the 8 header bytes are readable. The length is bounded before anything else reads further.
    let (magic, total) = unsafe {
        (
            u32::from_be_bytes([*head, *head.add(1), *head.add(2), *head.add(3)]),
            u32::from_be_bytes([*head.add(4), *head.add(5), *head.add(6), *head.add(7)]) as usize,
        )
    };
    if magic != 0xd00d_feed || !(40..=abi::limits::MAX_DTB_BYTES).contains(&total) {
        return None;
    }
    if (dtb as u64).saturating_add(total as u64) > abi::layout::DIRECT_MAP_PHYS_LIMIT {
        return None; // the tree would run past the span the early table maps
    }
    DTB_LEN.store(total, Ordering::Relaxed);
    Some((dtb as u64, total))
}

/// The device tree OpenSBI handed over, as bytes (None if there is none or its header was not sound). Reads through the direct map, which
/// the early table provides and the kernel table keeps for the tree's frames (read-only).
pub fn dtb_blob() -> Option<&'static [u8]> {
    let len = DTB_LEN.load(Ordering::Relaxed);
    if len == 0 {
        return None;
    }
    let head = (abi::layout::DIRECT_BASE + DTB.load(Ordering::Relaxed) as u64) as *const u8;
    // SAFETY: `locate` checked the header and bounded `len`; the tree's frames are direct-mapped read-only and never freed (they are not
    // in the frame allocator).
    Some(unsafe { core::slice::from_raw_parts(head, len) })
}

/// The tree's `memory` ranges, its reserved ranges (`/reserved-memory`, the loader's reservation block) and its own extent: what
/// `kmem::init` needs to decide which frames the allocator may have. `None` without a readable tree.
/// A list of `(address, size)` byte ranges.
pub type Ranges = alloc::vec::Vec<(u64, u64)>;

pub fn memory_map() -> Option<(Ranges, Ranges)> {
    let blob = dtb_blob()?;
    let nodes = kcore::fdt::parse(blob).ok()?;
    let memory = kcore::fdt::memory_ranges(&nodes);
    let mut reserved = kcore::fdt::reserved_ranges(&nodes);
    reserved.extend(kcore::fdt::reservation_block(blob));
    // the tree itself (its frames are mapped read-only, never handed out)
    reserved.push((DTB.load(Ordering::Relaxed) as u64, blob.len() as u64));
    Some((memory, reserved))
}

/// Set the clock's rate from the tree's `/cpus/timebase-frequency` (PL4b). A tree without one, or with a rate the kernel cannot convert, stops
/// the boot: nothing in the kernel assumes a rate.
#[allow(clippy::expect_used)] // INVARIANT: boot-time refusal by design: without a rate the clock and every timer are meaningless
pub fn init_timebase() {
    let hz = dtb_blob()
        .and_then(|b| kcore::fdt::parse(b).ok())
        .and_then(|nodes| kcore::fdt::timebase_frequency(&nodes))
        .expect("a timebase-frequency in the device tree");
    assert!(
        crate::timer::set_frequency(hz),
        "a timebase-frequency the kernel can convert (1 Hz to 4 GHz)"
    );
}

/// Refuse to run on a hart that lacks an extension the kernel relies on (PL1, OD47, OD127): `kcore::isa::REQUIRED_EXTENSIONS` and Sv39,
/// read from the device tree's `cpu` nodes (every hart is listed, so the boot hart checks them all). Ends in `##PANIC missing extension
/// <name> (hart <id>)`, which the harness expects on a CPU without the profile (`make os-boot-test-cpu-neg`).
#[allow(clippy::panic)] // INVARIANT: boot-time refusal by design: a kernel that cannot rely on its extensions must not run
pub fn require_extensions() {
    let Some(blob) = dtb_blob() else {
        panic!("missing extension: no device tree to read the harts' extensions from");
    };
    let Ok(nodes) = kcore::fdt::parse(blob) else {
        panic!("missing extension: the device tree cannot be read");
    };
    if let Some((hart, name)) = kcore::isa::missing_extensions(&nodes).first() {
        panic!("missing extension {name} (hart {hart}): the kernel requires RVA23S64 and the AIA (OD127)");
    }
}

/// Does the kernel command line (`/chosen/bootargs`, from QEMU's `-append`) contain the word `arg`? Used by the
/// benchmark switch (`bench=full`, SCHED-SMP-SPEC 6.1); false if there is no tree or no such property.
pub fn boot_arg_present(arg: &str) -> bool {
    let Some(blob) = dtb_blob() else {
        return false;
    };
    let Ok(nodes) = kcore::fdt::parse(blob) else {
        return false;
    };
    nodes
        .iter()
        .find(|n| n.path == "/chosen")
        .and_then(|n| n.prop("bootargs"))
        .is_some_and(|v| {
            v.split(|&b| b == 0 || b == b' ')
                .any(|w| w == arg.as_bytes())
        })
}

/// The value of the `name` word of the kernel command line, for words like `chaos=17` (`name` is `"chaos="`): the text after it.
pub fn boot_arg_value(name: &str) -> Option<alloc::string::String> {
    let blob = dtb_blob()?;
    let nodes = kcore::fdt::parse(blob).ok()?;
    let v = nodes
        .iter()
        .find(|n| n.path == "/chosen")?
        .prop("bootargs")?;
    v.split(|&b| b == 0 || b == b' ')
        .filter_map(|w| core::str::from_utf8(w).ok())
        .find_map(|w| w.strip_prefix(name))
        .map(alloc::string::String::from)
}

/// Test builds only (`ff=<name>:<value>,...` on the kernel line, AB3): the value a counter starts at, so a boundary is crossed in a few steps.
/// Names: `tid`, `koid`, `objgen`, `handlegen`, `fptoken`. Values are decimal or `0x` hex, with `_` allowed.
pub fn ff_value(name: &str) -> Option<u64> {
    if crate::RELEASE {
        return None;
    }
    let list = boot_arg_value("ff=")?;
    list.split(',').find_map(|kv| {
        let (k, v) = kv.split_once(':')?;
        if k != name {
            return None;
        }
        let v: alloc::string::String = v.chars().filter(|&c| c != '_').collect();
        match v.strip_prefix("0x") {
            Some(h) => u64::from_str_radix(h, 16).ok(),
            None => v.parse().ok(),
        }
    })
}

/// The names `ff_value` knows.
const FF_NAMES: [&str; 5] = ["tid", "koid", "objgen", "handlegen", "fptoken"];

/// Test builds only: a line saying what `ff=` asked for and what the kernel understood, so a run that was meant to start counters near their end
/// cannot pass vacuously on a typo or a dropped boot argument (T189): `##FF tid=0x... koid=0x...` for the values it parsed, `##FF error ...` for
/// a name it does not know or a value it cannot read. `None` when there is no `ff=` (or in a release build).
#[allow(clippy::large_stack_frames)] // KT13: audited, a frame over 1 KiB in a test-only boot diagnostic (the formatter's frames), called once from `kmain`
pub fn ff_report() -> Option<alloc::string::String> {
    use core::fmt::Write;
    if crate::RELEASE {
        return None;
    }
    let list = boot_arg_value("ff=")?;
    let mut out = alloc::string::String::from("##FF");
    let mut bad = false;
    for kv in list.split(',') {
        let name = kv.split_once(':').map(|(k, _)| k).unwrap_or(kv);
        if !FF_NAMES.contains(&name) {
            let _ = write!(out, " error unknown name {name:?}");
            bad = true;
        } else if let Some(v) = ff_value(name) {
            let _ = write!(out, " {name}={v:#x}");
        } else {
            let _ = write!(out, " error unreadable value for {name}");
            bad = true;
        }
    }
    if bad {
        out.push_str(" (the counters did not start where the run asked)");
    }
    Some(out)
}
