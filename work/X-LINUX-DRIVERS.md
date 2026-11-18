---
id: X-LINUX-DRIVERS
kind: topic
title: Drivers informed by Linux and the BSDs
area: drivers
horizon: someday
---

Not a line-by-line translation: Linux is GPL-2.0, so a translation is a derivative work, and its drivers are tied to kernel frameworks. Preferred shape: extract a prose hardware spec (datasheet plus Linux and BSD behaviour and quirks), write the driver from that spec over a typed MMIO layer, and review it against the sources in a fresh context. BSD-licensed drivers are the easier source. Needs the target board chosen first, and a licence decision.

(Not specified, not scheduled, not committed: a direction from the design conversations of 2026-11-14 and 2026-11-15. Needs a spec, an acceptance check and an OD entry before it becomes a slice.)
