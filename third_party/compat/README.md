# compat: small original shims for ported programs

Not third-party code: short files written for this repository so that unmodified fetched sources (`third_party/fetched/`)
build against our C library without running their `configure` or vendoring gnulib.

- `gzip/`: `config.h` and the gnulib interfaces gzip 1.13 uses (`xalloc`, `dirname`, `yesno`, `stat-time`, `utimens`, `savedir`, ...).
