---
id: Q-SMOLTCP
kind: decision
title: Allow smoltcp as a test-only dependency of netcore
area: net
horizon: long
---

T86: NET-SPEC H5 asks for smoltcp as an independent peer for `netcore`; the crates.io download was refused during an unattended run (no permission recorded). Owner decides: allow smoltcp as a dev-dependency of `netcore` (a test-only crate, OD18/OD27) and write H5.

Source: T86.
