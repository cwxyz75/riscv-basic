# Review: KT9 guest allocation-failure sweep (Fable, 2026-11-24)

No blocker. Findings and their disposition:

1. `cow_sync` returned early when the list of address spaces could not be allocated, so the caller's own stale translations were not dropped (COW1). **Fixed**: the caller's space is synced first with no allocation; the other spaces stay pending (T246 records the remaining window).
2. `heap::faultalloc::exempt` wrote one global slot from any thread, so a second thread could leave the armed one exempt for good or take the exemption away. **Fixed**: only the armed thread touches the slot.
3. The sweep could pass vacuously (a refused `debug_stat` read as 0, no proof the sweep ran, a VMO-only leak oracle). **Fixed**: a refused call fails the test, `OS_REQUIRE_LINE` makes `make os-allocfail` require the sweep's output, and the live object count is checked beside the VMO count.
4. Allocations in interrupt context count as the armed thread's: test builds only, noted in the item.
5. Docs: the Makefile target name in two comments and the misplaced doc comment were fixed; `NO_MEMORY` from `AsLocks::all` is listed in T246.
