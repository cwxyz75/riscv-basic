# bindings (B1 to B3): `=` creates, `set` changes, a block's bindings end at its end; defer captures by value, last in first out
--- script
fn f do
    x = 1
    defer do print first-registered $x end
    defer do print second-registered $x end
    set x = 2
    print body $x
end
f
g = 1
fn setg do set g = 2 end
r = try do setg end catch e do $e.message end
print $r
c = 5
bump = @ do set c = 6 ; ok $c end
print (call $bump) $c
fn two a b do ok $a end
r2 = try do two 1 2 3 end catch e do $e.message end
print $r2
r3 = try do two 1 end catch e do $e.message end
print $r3
print (! false)
print (1 != 2) (1 != 1) ('*xb' matches '*b') ('abc' matches '*b*')
x = 1
r4 = try do x = 2 end catch e do $e.message end
print $r4
if true do inner = 1 end
r5 = try do print $inner end catch e do $e.message end
print $r5
if true do inner = 2 end
for i in 1 2 do tmp = $i end
for i in 1 2 do tmp = $i end
fn p a do a2 = $a + 1 ; ok $a2 end
print (p 1)
y = if $x == 1 do 'one' else 'other' end
print $y
r6 = try do set nothere = 1 end catch e do $e.message end
print $r6
--- stdout
body 2
second-registered 1
first-registered 1
there is no binding `g` to change here (`set` changes only bindings of the current function; write `g = ...` to make one)
6 5
two takes 2 argument(s), got 3
two takes 2 argument(s), got 1
true
true false true true
`x` is already bound here; to change it write `set x = ...`
`inner` is not set
2
one
there is no binding `nothere` to change here (`set` changes only bindings of the current function; write `nothere = ...` to make one)
--- stderr
--- status
0
