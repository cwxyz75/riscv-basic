-- S7d acceptance script. It is run twice - by `bin/lua` on the OS and by a Lua built from the
-- same sources on the host - and the two transcripts must be identical. So everything printed is
-- deterministic and defined by the language or the C standard: no addresses, no `pairs` order of
-- string keys (sorted first), no NaN signs, no clock values, no last-digit libm results.
local function section(name) print("== " .. name) end
local function show(...)
  local t = table.pack(...)
  for i = 1, t.n do t[i] = tostring(t[i]) end
  -- error messages carry the chunk name (the script path), which differs between the two runs
  for i = 1, t.n do t[i] = t[i]:gsub(arg[0]:gsub("%p", "%%%0"), "SCRIPT") end
  print(table.concat(t, " "))
end
local function sorted_keys(t)
  local ks = {}
  for k in pairs(t) do ks[#ks + 1] = k end
  table.sort(ks, function(a, b) return tostring(a) < tostring(b) end)
  return ks
end

section "types and arithmetic"
show(math.type(1), math.type(1.0), math.type("1"), type(nil), type(print))
show(1 + 2, 1 + 2.0, 7 // 2, 7.0 // 2, -7 // 2, 7 % 3, -7 % 3, 7 % -3, 5.5 % 2)
show(1 / 2, 3 / 1, 2 ^ 10, 2 ^ 0.5 > 1.414 and 2 ^ 0.5 < 1.415)
show(math.maxinteger, math.mininteger, math.maxinteger + 1 == math.mininteger)
show(1 << 62, 1 << 63, 1 << 64, -1 >> 1, 5 & 3, 5 | 3, 5 ~ 3, ~5)
show(10 == 10.0, "10" == 10, 0.1 + 0.2 == 0.3, math.abs(0.1 + 0.2 - 0.3) < 1e-15)
show("10" + 5, "0x10" + 0, "3.5" * 2, 10 .. 20)
show(tostring(1e15), tostring(1e100), tostring(-0.0), 2^53, 2^63, math.tointeger(3.0), math.tointeger(3.5))
show(tonumber("  12  "), tonumber("0x1p4"), tonumber("1e2"), tonumber("abc"), tonumber("10", 2), tonumber("zz", 36), tonumber("8", 8))
show(1e308 * 10 == math.huge, -1e308 * 10 == -math.huge, (0/0) ~= (0/0))
show(string.format("%d %5.2f %s %q", 42, 3.14159, "x", "a\nb"))
show(pcall(function() return 1 // 0 end))
show(pcall(function() return 1 % 0 end))
show(1.0 // 0 == math.huge, math.fmod(7, 3), math.fmod(-7, 3))
show(8 // 3 * 3 + 8 % 3, (-8) // 3 * 3 + (-8) % 3)
show(math.ult(1, -1), math.max(1, 5, 3), math.min(4, 2.5), math.floor(-2.5), math.ceil(-2.5), math.floor(2^40 + 0.5))

section "strings"
local s = "Hello, World"
show(#s, s:upper(), s:lower(), s:sub(1, 5), s:sub(-5), s:sub(3, -3), s:rep(2, "|"), s:reverse())
show(s:find("World"), s:find("o", 6), s:find("xyz"), s:find("l+"), s:find(".", 1, true))
show(s:byte(1, 3), string.char(72, 105), ("x"):byte(), #("\0\0\0"))
show(s:match("(%a+), (%a+)"), s:match("^(%w+)"), s:match("%d"), s:match("()ll()"))
show(s:gsub("o", "0"), s:gsub("%a+", string.upper), s:gsub("(%a)(%a*)", "%2%1"), s:gsub("o", {o = "[o]"}, 1))
for w in ("one two  three"):gmatch("%S+") do io.write(w, ";") end print()
for k, v in ("a=1, b=2, c=3"):gmatch("(%w+)=(%w+)") do io.write(k, v, ";") end print()
show(("%5s|%-5s|%05d|%x|%X|%o|%c|%e|%g"):format("ab", "ab", 42, 255, 255, 8, 65, 12345.6789, 0.0001))
show(("%10.4f|%-10.2f|%+d|% d"):format(math.pi, math.pi, 5, 5))
show(("%q"):format(1/3), ("%q"):format(255), ("%q"):format("tab\there\0nul"))
show(("%s %s %s"):format(nil, true, {} ~= nil), ("%.3s"):format("abcdef"), ("%%"):format())
show(("%a"):format(1.0) == "0x1p+0", ("%i"):format(7))
show(string.format("%-8s|", "é"), #"é", utf8.len("héllo"), utf8.char(233, 8364, 128512), utf8.codepoint("€", 1))
for p, c in utf8.codes("aé€") do io.write(p, ":", c, " ") end print()
show(utf8.offset("aé€", 3), utf8.len("\xff"), utf8.charpattern)
show(("abc"):len(), ("abc") < ("abd"), ("a") < ("B"), ("") == (""), "a" .. 1 .. 2.0)
show(string.pack("<i4", 1):byte(1, -1))
show(string.unpack("<i2 >i2", "\1\0\0\1"))
show(string.pack(">I3", 0x010203):byte(1, -1))
show(string.packsize("i4 i8 d"), string.unpack("z", "hello\0rest"), string.unpack("s1", "\3abcdef"))
show(string.pack("<d", 1.5):byte(1, -1))
show(select("#", string.unpack("<f", string.pack("<f", 0.5))), string.unpack("<f", string.pack("<f", 0.5)))
show(("x"):rep(0) == "", ("ab"):rep(3), table.concat({1, 2, 3}, "-"), table.concat({}, "x"))
show(("  trim  "):match("^%s*(.-)%s*$") .. "|", ("a,b,,c"):find(",,", 1, true))
show(("f(a,b)"):match("%((.-)%)"), ("[[x]]"):match("%[%[(.*)%]%]"), ("key = value"):match("^(%w+)%s*=%s*(%w+)$"))
show(("THE (quick) fox"):find("%((%a+)%)"), ("abc"):gsub("", "-"), ("hello world"):gsub("o", "0", 1))
show(("%bxy"):len(), ("(foo(bar))baz"):match("%b()"), ("THE END"):lower():gsub("^%l", string.upper))
show(pcall(string.rep, "x", -1), pcall(string.format, "%d", 1.5))
show(pcall(string.format, "%d", "x"))
show(tostring(12):rep(2), (""):byte(), ("abc"):sub(0), ("abc"):sub(10), ("abc"):sub(-100, 2))

section "tables"
local t = {10, 20, 30, nil, 50, n = "n", [100] = "far"}
show(#t == 3 or #t == 5, t.n, t[100], rawlen({1, 2}), rawequal(t, t), next({}))
local a = {5, 2, 8, 1, 9, 3}
table.sort(a) show(table.concat(a, ","))
table.sort(a, function(x, y) return x > y end) show(table.concat(a, ","))
local words = {"pear", "apple", "fig", "banana", "cherry"}
table.sort(words) show(table.concat(words, " "))
table.sort(words, function(x, y) return #x < #y or (#x == #y and x < y) end) show(table.concat(words, " "))
table.insert(a, 100) table.insert(a, 1, 0) show(table.concat(a, ","), table.remove(a), table.remove(a, 1), #a)
show(table.unpack({1, 2, 3})) show(table.unpack({1, 2, 3}, 2)) show(table.unpack({1, 2, 3}, 2, 5))
show(table.pack(1, nil, 3).n, select("#", table.unpack({}, 1, 3)))
local m = table.move({1, 2, 3}, 1, 3, 3, {9, 9}) show(table.concat(m, ","))
local big = {} for i = 1, 1000 do big[i] = (i * 7919) % 1009 end table.sort(big)
local ok = true for i = 2, #big do if big[i - 1] > big[i] then ok = false end end show(ok, #big, big[1], big[1000])
show(select(2, pcall(table.sort, {3, 2, 1, 4, 5, 6, 7, 8, 9, 10, 11, 12}, function() return true end)) ~= nil)
local keys = {} for _, k in ipairs(sorted_keys({x = 1, y = 2, [1] = "a", [2] = "b"})) do keys[#keys + 1] = tostring(k) end show(table.concat(keys, ","))
local cnt = 0 for _ in pairs({1, 2, 3, a = 1, b = 2}) do cnt = cnt + 1 end show(cnt)
local u = {} for i = 1, 10 do u[i] = i end for i = 1, 10, 2 do u[i] = nil end
cnt = 0 for k, v in pairs(u) do cnt = cnt + v end show(cnt)
show(#"abc", #{n = 1}, #{1, 2, 3})
show(pcall(table.insert, {1, 2, 3}, 6, 1))
show(pcall(next, {}, "nokey"))
show(table.concat({1, 2.5, "x"}, ", ", 2, 3))

section "control flow and closures"
local function counter() local n = 0 return function() n = n + 1 return n end end
local c1, c2 = counter(), counter() show(c1(), c1(), c2(), c1())
local fs = {} for i = 1, 3 do fs[i] = function() return i end end show(fs[1](), fs[2](), fs[3]())
local function fib(n) if n < 2 then return n end return fib(n - 1) + fib(n - 2) end show(fib(20))
local function va(...) return select("#", ...), ... end show(va()) show(va(nil, nil)) show((va(1, 2, 3)))
for i = 10, 1, -3 do io.write(i, " ") end for i = 1.0, 2 do io.write(i, " ") end print()
local i = 0 repeat local j = i i = i + 1 until j >= 3 show(i)
do local n = 0 while true do n = n + 1 if n > 5 then break end end show(n) end
for i = 1, 3 do for j = 1, 3 do if j == 2 then goto continue end io.write(i, j, " ") ::continue:: end end print()
do local k = 1 ::top:: if k < 4 then k = k + 1 goto top end show(k) end
local function tail(n, acc) if n == 0 then return acc end return tail(n - 1, acc + n) end show(tail(100000, 0))
show(pcall(function() local x <const> = 1 return x + 1 end))
do local closed = {} do local r <close> = setmetatable({}, {__close = function() closed[#closed + 1] = "closed" end}) end show(closed[1]) end
show(select(-1, 1, 2, 3), select(2, "a", "b", "c"))
show((function(...) local a, b = ... return a, b end)(1))
show(not nil, not 0, nil and 1, false or "x", 1 and 2, nil or false)
-- (unbounded recursion is left to S7e: on a 16 MiB heap a Lua stack overflow is a memory error, T53)
local function depth(n) if n == 0 then return 0 end return 1 + depth(n - 1) end show(depth(10000))
show(string.rep("ab", 3, ","))

section "metatables and OOP"
local V = {} V.__index = V
function V.new(x, y) return setmetatable({x = x, y = y}, V) end
V.__add = function(a, b) return V.new(a.x + b.x, a.y + b.y) end
V.__eq = function(a, b) return a.x == b.x and a.y == b.y end
V.__lt = function(a, b) return a.x < b.x end
V.__le = function(a, b) return a.x <= b.x end
V.__tostring = function(a) return "(" .. a.x .. "," .. a.y .. ")" end
V.__len = function() return 2 end
V.__call = function(self, k) return self[k] end
V.__concat = function(a, b) return tostring(a) .. tostring(b) end
V.__unm = function(a) return V.new(-a.x, -a.y) end
function V:len2() return self.x * self.x + self.y * self.y end
local p, q = V.new(1, 2), V.new(3, 4)
show(tostring(p + q), p == V.new(1, 2), p < q, p <= q, p > q, #p, p("x"), p .. q, tostring(-p), q:len2())
local log = {}
local proxy = setmetatable({}, {__index = function(_, k) log[#log + 1] = "get " .. k return k .. "!" end,
  __newindex = function(t, k, v) log[#log + 1] = "set " .. k rawset(t, k, v) end})
show(proxy.a, proxy.b) proxy.c = 1 proxy.c = 2 show(table.concat(log, ","), rawget(proxy, "c"))
local Base = {} Base.__index = Base function Base.new(n) return setmetatable({n = n}, Base) end
function Base:name() return "base" .. self.n end
local Derived = setmetatable({}, {__index = Base}) Derived.__index = Derived
function Derived.new(n) local o = Base.new(n) return setmetatable(o, Derived) end
function Derived:name() return "derived/" .. Base.name(self) end
show(Derived.new(7):name(), getmetatable(Derived.new(1)) == Derived)
show(getmetatable("x").__index == string, getmetatable(setmetatable({}, {__metatable = "locked"})))
show(pcall(setmetatable, setmetatable({}, {__metatable = 1}), {}))
local ro = setmetatable({}, {__newindex = function() error("read-only table", 2) end})
show(pcall(function() ro.x = 1 end))
local mt = {__index = function(t, k) return k * 2 end} show(setmetatable({}, mt)[21])
local gc = setmetatable({}, {__mode = "k"}) show(getmetatable(gc).__mode)
local pairs_mt = setmetatable({}, {__pairs = function(t) return function(_, k) if not k then return 1, "one" end end, t, nil end})
for k, v in pairs(pairs_mt) do show(k, v) end
local idiv = setmetatable({}, {__idiv = function() return "idiv" end, __mod = function() return "mod" end, __band = function() return "band" end, __shl = function() return "shl" end, __bnot = function() return "bnot" end})
show(idiv // 1, idiv % 1, idiv & 1, idiv << 1, ~idiv)

section "errors"
show(pcall(error, "msg")) show(pcall(error, "msg", 0)) show(select("#", pcall(error, {code = 1})), type(select(2, pcall(error, {code = 1}))))
show(select(2, pcall(error, setmetatable({}, {__tostring = function() return "custom" end}))) ~= nil)
show(pcall(function() local x = nil; return x.field end))
show(pcall(function() return #5 end))
show(pcall(function() return {} + 1 end))
show(pcall(function() return "a" < 1 end))
show(pcall(function() undefinedfunction() end))
show(xpcall(function() error("boom") end, function(m) return "handled: " .. m end))
show(xpcall(function(a, b) return a + b end, print, 3, 4))
show(select(2, pcall(error)))
show(pcall(assert, false)) show(pcall(assert, nil, "assertion msg")) show(assert(1, "unused"))
show(pcall(string.rep)) show(pcall(("x").rep, "x", {}))
show(tostring(nil), tostring(true), pcall(tostring))
local ok, e = pcall(function() error(setmetatable({}, {__tostring = function() return "E" end})) end) show(ok, tostring(e))
local function lvl() error("lvl2", 2) end show(pcall(function() lvl() end))
show(pcall(load, "x ="))
show(load("x ="))
show(load("return 1 +"))
show(load("return 1", "=chunk")())
show(load("local a <const> = 1; a = 2"))
show(pcall(load("error('in chunk')", "=mychunk")))
show(select("#", pcall(error, nil)))
show(tostring(select(2, pcall(string.char, 256))):match("value out of range") ~= nil)
show(math.tointeger("8"), math.tointeger({}) == nil)
show(pcall(math.random, 0, -1))
show(pcall(setmetatable, 1, {}))
show(pcall(ipairs))
show(string.format("%s", select(2, pcall(function() local t = {} t[nil] = 1 end))))
show(string.format("%s", select(2, pcall(function() local t = {} t[0/0] = 1 end))))
show(coroutine.close(coroutine.create(print)))

section "coroutines"
local co = coroutine.create(function(a, b)
  show("start", a, b)
  local c = coroutine.yield(a + b)
  show("got", c)
  local d, e = coroutine.yield(c * 2)
  show("got", d, e)
  return "end", 99
end)
show(coroutine.resume(co, 1, 2)) show(coroutine.status(co))
show(coroutine.resume(co, 10)) show(coroutine.resume(co, "x", "y")) show(coroutine.status(co))
show(coroutine.resume(co)) show(coroutine.isyieldable(), select(2, coroutine.running()))
local gen = coroutine.wrap(function() for i = 1, 3 do coroutine.yield(i) end end)
show(gen(), gen(), gen(), gen())
show(pcall(gen))
local function producer() return coroutine.create(function() for i = 1, 5 do coroutine.yield("item" .. i) end return nil end) end
local pr, items = producer(), {} while true do local _, v = coroutine.resume(pr) if not v then break end items[#items + 1] = v end show(table.concat(items, ","))
local ec = coroutine.create(function() error("inside") end) show(coroutine.resume(ec)) show(coroutine.status(ec))
local inner = coroutine.wrap(function() local c = coroutine.wrap(function() coroutine.yield("a") coroutine.yield("b") end) coroutine.yield(c() .. c()) end) show(inner())
show(coroutine.resume(coroutine.create(function() return coroutine.isyieldable() end)))
local cc = coroutine.create(function() local x <close> = setmetatable({}, {__close = function() print("closed on close") end}) coroutine.yield() end)
coroutine.resume(cc) show(coroutine.close(cc), coroutine.status(cc))
local ycall = coroutine.wrap(function() return pcall(function() coroutine.yield(1) error("after") end) end) show(ycall()) show(ycall())
local sorter = coroutine.wrap(function() table.sort({3, 1, 2}, function(a, b) return a < b end) coroutine.yield("sorted") end) show(sorter())
show(select("#", coroutine.resume(coroutine.create(function() return end))))
local tw = coroutine.wrap(function(...) local n = select("#", ...) while true do n = select("#", coroutine.yield(n)) end end) show(tw(1, 2, 3), tw(), tw(nil, nil))
show(pcall(coroutine.resume, nil))

section "math and library"
show(math.floor(3.7), math.ceil(3.2), math.abs(-4), math.sqrt(16), math.pi > 3.14159 and math.pi < 3.1416)
show(string.format("%.6f %.6f %.6f %.6f", math.sin(1), math.cos(1), math.tan(1), math.atan(1, 2)))
show(string.format("%.6f %.6f %.6f %.6f", math.exp(1), math.log(10), math.log(8, 2), math.log(100, 10)))
show(string.format("%.6f %.6f %.6f", math.asin(0.5), math.acos(0.5), math.atan(1)))
show(math.huge, -math.huge, math.huge > math.maxinteger, math.maxinteger + 0.0 == 2^63)
show(math.fmod(5.5, 2), math.modf(3.7), math.modf(-3.7), math.modf(math.huge))
math.randomseed(42) local r1 = math.random(1, 100) math.randomseed(42) show(r1 == math.random(1, 100), math.random() < 1, math.random(5) <= 5)
local seen = {} for _ = 1, 200 do seen[math.random(1, 3)] = true end show(seen[1], seen[2], seen[3], seen[0], seen[4])
show(7 // 0.0, -7 // 0.0, 0.0 / 0.0 ~= 0.0 / 0.0, 3 % math.huge, -3 % math.huge)
show(math.tointeger(2^53), 2^53 == 2^53 + 1, (2^53 | 0), pcall(function() return 2^64 | 0 end))
show(#tostring(math.pi), tostring(1/3), tostring(100.0), tostring(-1e-7), 1e15, 1e16, 123456789012345678)
show(255 // 1 | 0, pcall(function() return "1.5" | 0 end))
show(string.format("%.14g", 0.1), string.format("%.17g", 0.1), string.format("%5.1f|%-8.3e|", 2.25, 12345.678))
show(8 % 3.5, 2^-1, (-2)^2, -2^2, 2^3^2, 1 .. 2, "a" .. "b" == "ab")
show(tostring(3) == "3", tostring(3.0) == "3.0", 3 | 0, math.type(3 // 1), math.type(3 / 1), 7 // 1.0)
show(os.time({year = 2020, month = 1, day = 1, hour = 12}) - os.time({year = 2020, month = 1, day = 2, hour = 12}))
show(os.date("!%Y-%m-%d %H:%M:%S", 86400 * 365), os.date("!%A %B %j", 0), os.date("!*t", 3600).hour, os.date("!*t", 0).year)
show(os.time({year = 2000, month = 13, day = 1, hour = 0}) == os.time({year = 2001, month = 1, day = 1, hour = 0}))
show(type(os.clock()), type(os.time()), os.getenv("S7_NO_SUCH_VARIABLE"), type(os.tmpname()), os.difftime(10, 4))
show(os.date("!%d/%m/%y", 1e9), pcall(os.date, "%Ez"))
show(select("#", os.date("*t")), os.setlocale(), os.setlocale("C"), os.setlocale("xx_NOPE"))
show(type(collectgarbage("count")), collectgarbage("step"), collectgarbage("isrunning"), collectgarbage("incremental"), collectgarbage("generational"), collectgarbage("incremental"))
collectgarbage() collectgarbage("collect")
local weak = setmetatable({}, {__mode = "v"}) weak[1] = {} collectgarbage() show(weak[1])
local fin = {} setmetatable({}, {__gc = function() fin[1] = "finalized" end}) collectgarbage() collectgarbage() show(fin[1])
show(_VERSION, type(_G), _G._G == _G, type(package), type(package.loaded), package.loaded.string == string, type(require))
show(pcall(require, "no.such.module"))
local env = {print = print} local f = load("x = 5 return x", "=env", "t", env) show(f(), env.x, rawget(_G, "x"))
show(select(2, load("return ...", "=va"))) show(load("return ...", "=va")(1, 2))
show(type(load(function() return nil end))) show(pcall(load, 1))
local pieces = {"return ", "1 + ", "41"} local pi = 0
show(load(function() pi = pi + 1 return pieces[pi] end)())
show(string.dump(function() end):sub(1, 4) == "\27Lua", load(string.dump(function() return "dumped" end), "d", "b")())
show(pcall(load, "return 1", "x", "b"))
show(dofile == nil, loadfile == nil, #arg, arg[0] ~= nil)
show(next({}), type(next), select("#", ipairs({})), rawequal("a", "a"), rawlen("abc"))
show(tostring(print):match("^function"))

section "io and os"
local name = "s7_scratch.txt"
-- (opened for reading too: a write-only file connection cannot ask the file server for its size, so
-- `seek("end")` needs read access, T51)
local f = assert(io.open(name, "w+"))
show(io.type(f), io.type(42), io.type(io.stdout))
show(f:write("line one\n", "line two\n", 3, " ", 4.5, "\n", "last line no newline") == f)
show(f:seek("cur"), f:seek("set", 5), f:seek("end"))
f:close() show(io.type(f), pcall(f.write, f, "x"))
for l in io.lines(name) do io.write("[", l, "]") end print()
for l in io.lines(name, "L") do io.write(#l, " ") end print()
local r = assert(io.open(name))
show(r:read("l")) show(r:read("n")) show(r:read("n")) show(r:read("l")) show(r:read(4)) show(r:read("a")) show(r:read("a")) show(r:read("l"), r:read(0))
r:seek("set", 0) show(r:read("L")) show(r:read(0), r:read(100):sub(1, 8)) r:close()
local ap = assert(io.open(name, "a")) ap:write("\nappended") ap:close()
local n = 0 for _ in io.lines(name) do n = n + 1 end show(n)
local rw = assert(io.open(name, "r+")) rw:write("LINE") rw:seek("set", 0) show(rw:read("l")) rw:close()
show(io.open("does_not_exist_s7.txt"))
show(select("#", io.open("does_not_exist_s7.txt")), select(3, io.open("does_not_exist_s7.txt")) ~= nil)
show(pcall(io.lines, "does_not_exist_s7.txt"))
show(pcall(io.open, name, "bad mode"))
local nums = assert(io.open(name, "w")) nums:write("10 20.5 0x10 -3 abc") nums:close()
nums = assert(io.open(name)) show(nums:read("n", "n", "n", "n", "n")) nums:close()
local big = assert(io.open(name, "w")) for i = 1, 2000 do big:write(string.format("%04d-%s\n", i, ("x"):rep(i % 50))) end big:close()
local total, lines = 0, 0 for l in io.lines(name) do lines = lines + 1 total = total + #l end show(lines, total)
local bf = assert(io.open(name, "rb")) local data = bf:read("a") bf:close() show(#data, data:sub(1, 10), data:sub(-4) == "xxx\n" or data:sub(-4))
local out = assert(io.open(name, "wb")) out:setvbuf("full", 1024) out:write(("z"):rep(5000)) out:flush() out:setvbuf("no") out:write("!") out:close()
bf = assert(io.open(name, "rb")) show(#bf:read("a")) bf:close()
io.output():write("via io.output\n") io.write("via io.write\n") io.stdout:write("via stdout\n")
io.stdout:setvbuf("line") show(io.output() == io.stdout, io.input() == io.stdin)
local tf = io.tmpfile() tf:write("tmp data") tf:seek("set") show(tf:read("a")) tf:close()
show(os.remove(name)) show(select("#", os.remove(name)), (select(2, os.remove(name))):match("^" .. name .. ": ") ~= nil)
show(io.open(name))
io.output("s7_scratch2.txt") io.write("redirected") io.close() io.output(io.stdout)
io.input("s7_scratch2.txt") show(io.read("a")) io.close(io.input()) io.input(io.stdin)
show(os.remove("s7_scratch2.txt"))
show(pcall(io.close))
show(type(os.exit), type(os.getenv))
io.stderr:write("")

section "goto, integer for loops, misc"
for i = math.maxinteger - 2, math.maxinteger do io.write(i - math.maxinteger, " ") end print()
for i = 1, 0 do io.write("never") end for i = 3, 1 do io.write("never") end
show(pcall(function() for i = 1, 10, 0 do end end))
for i = 1, 3 do local x = i * 2 if x == 4 then goto done end io.write(x, " ") end ::done:: print()
local sum = 0 for i = 1, 100 do if i % 2 == 0 then goto skip end sum = sum + i ::skip:: end show(sum)
show(("x"):rep(100):len(), #table.concat({("a"):rep(1000), ("b"):rep(1000)}))
local long = {} for i = 1, 300 do long[i] = i end show(select("#", table.unpack(long)), (select(300, table.unpack(long))))
show(string.format("%s|%s", 1, 1.0), 2^31, 2^31 | 0, 1e3 // 1, -0.0 == 0.0, 3 == 3.0000000000000001)
show(#{n = 1, 2, 3}, #"\u{10FFFF}", "\x41\65\u{41}\z
       B", '\'', "\\")
show([[
long
string]], [==[with ]] inside]==])
local t2 = {f = function(self, x) return x end, [1.0] = "one"} show(t2:f(5), t2[1], t2.f(nil, 6))
show(type(os.getenv), -2^63 == math.mininteger, math.mininteger // -1, pcall(function() return math.mininteger % -1 end))
print("done")
