//! PL8: the backstops follow the memory of the machine, as KERNEL-V1-SPEC 2.5's table says, and a small machine is no worse off than the old constants.

use kcore::limits::Backstops;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

#[test]
fn a_large_machine_has_the_figures_of_the_table() {
    let b = Backstops::for_ram(4 * GIB, 32768);
    assert_eq!(b.objects, (4 * GIB / 4096) as usize);
    assert_eq!(b.ipc_bytes, (4 * GIB / 32) as usize);
    assert_eq!(b.processes, 4096);
    assert_eq!(
        b.user_threads, 32768,
        "RAM / 64 KiB is 65536, the stack window has 32768 slots"
    );
    assert_eq!(b.dma_total_pages, 4 * GIB / 16 / 4096);
}

#[test]
fn processes_stop_at_two_to_the_twenty() {
    assert_eq!(
        Backstops::for_ram(4096 * GIB, usize::MAX).processes,
        1 << 20
    );
}

#[test]
fn a_small_machine_keeps_the_old_figures_and_a_bigger_one_differs() {
    let small = Backstops::for_ram(16 * MIB, 32768);
    assert_eq!(small, Backstops::DEFAULT);
    let mid = Backstops::for_ram(128 * MIB, 32768);
    let big = Backstops::for_ram(4 * GIB, 32768);
    assert!(mid.objects < big.objects && mid.ipc_bytes < big.ipc_bytes);
    assert!(mid.processes < big.processes && mid.dma_total_pages < big.dma_total_pages);
}

#[test]
fn threads_never_exceed_the_stack_window() {
    assert!(Backstops::for_ram(64 * GIB, 1000).user_threads <= 1000);
}
