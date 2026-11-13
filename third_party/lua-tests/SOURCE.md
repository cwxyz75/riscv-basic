# The Lua test suite (vendored, unmodified)

- Version: lua-5.4.8-tests, for Lua 5.4.8.
- Source: <https://www.lua.org/tests/lua-5.4.8-tests.tar.gz>, downloaded 2026-11-13.
- SHA-256 of the tarball: `9581d5a7c39ffbf29b8ccde2709083c380f7bbddbd968dcb15712d2f2e33f4e5` (matches the value
  published on <https://www.lua.org/tests/>).
- Contents here: the `.lua` files exactly as in the tarball. The C parts (`libs/`, `ltests/`) are not
  vendored: they need a dynamic loader and Lua's internal test build. The licence (MIT) is the notice at
  the end of `all.lua`.
- Run by the OS suite as `lua -e "package.path=...; _port=true; _soft=true" /etc/lua-tests/NAME.lua`
  for the files listed in `init/src/bin/suite.rs` (`lua_upstream_*`); each is its own `##TEST`.
