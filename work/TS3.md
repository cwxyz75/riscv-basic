---
id: TS3
kind: slice
title: Move test logic out of QEMU
area: testing
horizon: long
---

- [ ] Step 3: move logic out of QEMU, incrementally: the syscall layer behind a small `Machine` trait so argument errors, rights, quotas and accounting run on the host against the real code with a fake machine (moving the matching suite tests over as each piece lands); host harnesses driving server protocols over in-memory channels (with slice R's `Service` trait); optional bounded model checking of small pure pieces (handle table, the slice T pool)
