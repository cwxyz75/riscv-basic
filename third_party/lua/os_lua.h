/* Build settings for Lua on this OS (S7d), force-included (`-include`) by both the cross build
 * and the host oracle, so the two compile the same configuration. The upstream sources under
 * third_party/lua/src are unmodified. This is a plain ISO C build: no POSIX (so no `popen`,
 * `isatty`, `mkstemp`, `dlopen`, `readline`) and the default `double` / 64-bit integer types. */
#undef LUA_USE_POSIX
#undef LUA_USE_LINUX
#undef LUA_USE_MACOSX
#undef LUA_USE_DLOPEN
#undef LUA_USE_READLINE
