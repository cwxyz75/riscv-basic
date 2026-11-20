//! The device tree OpenSBI hands the kernel (in `a1`), read from the top of RAM.

use core::sync::atomic::{AtomicUsize, Ordering};

/// The device tree's physical address.
pub static DTB: AtomicUsize = AtomicUsize::new(0);

/// The device tree OpenSBI handed over, as bytes (None if the address is not inside the top 2 MiB
/// of RAM that `kmem` direct-maps read-only, or does not hold a tree header).
pub fn dtb_blob() -> Option<&'static [u8]> {
    let dtb = DTB.load(Ordering::Relaxed) as u64;
    let ram_top = abi::layout::RAM_BASE + abi::layout::RAM_SIZE;
    if dtb < ram_top - 2 * 1024 * 1024 || dtb >= ram_top - 64 {
        return None;
    }
    let head = (abi::layout::DIRECT_BASE + dtb) as *const u8;
    // SAFETY: `dtb` lies inside the direct-mapped top 2 MiB (checked above), so the 8 header bytes
    // exist; the total size is then clamped to what is left of that region before the slice is formed.
    unsafe {
        if u32::from_be_bytes([*head, *head.add(1), *head.add(2), *head.add(3)]) != 0xd00d_feed {
            return None;
        }
        let total =
            u32::from_be_bytes([*head.add(4), *head.add(5), *head.add(6), *head.add(7)]) as u64;
        if total < 40 {
            return None;
        }
        let len = total.min(ram_top - dtb) as usize;
        Some(core::slice::from_raw_parts(head, len))
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
#[cfg_attr(not(feature = "chaos"), allow(dead_code))]
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
