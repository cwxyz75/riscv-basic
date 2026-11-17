# BusyBox (UV8)

BusyBox 1.36.1 (GPLv2), fetched by `make fetch-third-party` into `third_party/fetched/busybox/` (not committed; the hash is pinned in
`fetch-manifest.txt`). The sources are built **unmodified**. What is committed here is only what the build needs besides them:

* `config`: the BusyBox configuration (`.config`) for the applets in `scripts/busybox_configure.sh`, NOMMU, no shell.
* `files.txt`: the source files that configuration builds (computed from BusyBox's own `Kbuild` files).
* `gen/`: the headers BusyBox's Kbuild generates for that configuration (`autoconf.h`, `applet_tables.h`, `usage_compressed.h`, ...).

`scripts/busybox_configure.sh` regenerates all three from the fetched sources. `init/build.rs` compiles `files.txt` with the RISC-V clang
against `libc/include` and links the result with the C library into the `busybox` program; without the fetched sources `busybox` is a
stub that exits 99. The applet is chosen by the first argument (`busybox wc -l file`).
