# Lua (vendored, unmodified)

- Version: Lua 5.4.8 (released 2025).
- Source: <https://www.lua.org/ftp/lua-5.4.8.tar.gz>, downloaded 2026-11-13.
- SHA-256 of the tarball: `4f18ddae154e793e46eeab727c59ef1c0c0c2b744e7b94219710d76f530629ae` (matches the
  value published on <https://www.lua.org/ftp/>).
- Contents here: `src/` exactly as in the tarball (the Makefile and `luac.c` are not built), `README`,
  and `LICENSE` (the MIT licence text from `src/lua.h`).
- Our only configuration is `os_lua.h`, force-included (`-include`) by both the cross build
  (`init/build.rs`) and the host oracle (`scripts/os_boot_test.py`): plain ISO C, no POSIX.
  Nothing under `src/` is edited. To upgrade, replace `src/`, update the checksum here and run the
  acceptance script (`tests/lua/s7.lua`) through `make os-boot-test`.
