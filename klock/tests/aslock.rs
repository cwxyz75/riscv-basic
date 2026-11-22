//! The address-space lock's holder (L17): the lifetime transmute in `hold`, the drop order, and `take` against a live holder.
use kcore::aspace::AddressSpace;
use kcore::pagetable::PageTable;
use kcore::Mm;
use klock::aslock::AsCell;
use std::sync::Arc;

mod mem {
    use kcore::pagetable::PhysMem;
    use std::collections::HashMap;
    #[derive(Default)]
    pub struct Mem(HashMap<u64, u64>);
    impl PhysMem for Mem {
        fn read_u64(&self, pa: u64) -> u64 {
            *self.0.get(&pa).unwrap_or(&0)
        }
        fn write_u64(&mut self, pa: u64, v: u64) {
            self.0.insert(pa, v);
        }
    }
}

fn cell(pid: u64) -> Arc<AsCell> {
    let mut mm = Mm {
        db: kcore::frame::FrameDb::new(64, 8),
        mem: mem::Mem::default(),
    };
    let table = PageTable::new(&mut mm).unwrap();
    Arc::new(AsCell::new(pid, AddressSpace::new(table)))
}

#[test]
fn a_holder_gives_access_and_lets_go_in_order() {
    let c = cell(3);
    let root = c.root();
    {
        let _h = AsCell::hold(&c);
        assert!(c.held());
        assert_eq!(c.held_ref().table().root(), root);
        assert_eq!(c.held_mut().region_count(), 0);
        assert_eq!(Arc::strong_count(&c), 2, "the holder keeps the cell alive");
    }
    assert_eq!(Arc::strong_count(&c), 1, "and lets it go");
    // the lock is free again: a second hold works
    let _again = AsCell::hold(&c);
}

#[test]
fn a_cell_nobody_holds_can_be_taken() {
    let c = cell(4);
    let space = AsCell::take(c);
    drop(space);
}

#[test]
#[should_panic(expected = "another holder")]
fn a_cell_with_a_live_holder_cannot_be_taken() {
    let c = cell(5);
    let h = AsCell::hold(&c);
    let _ = AsCell::take(c); // the holder still has a clone
    drop(h);
}
