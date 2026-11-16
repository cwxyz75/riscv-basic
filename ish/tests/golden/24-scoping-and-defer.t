# defer captures by value and runs last-in first-out; a function updates a global but a closure's copy is its own
--- script
fn f do
    x = 1
    defer do print first-registered $x end
    defer do print second-registered $x end
    x = 2
    print body $x
end
f
g = 1
fn setg do g = 2 end
setg
print $g
c = 5
bump = @ do c = 6 ; ok $c end
print (call $bump) $c
fn two a b do ok $a end
r = try do two 1 2 3 end catch e do print $e.message end
r = try do two 1 end catch e do print $e.message end
print (! false)
print (is 1 != 2) (is 1 != 1) (is '*xb' matches '*b') (is 'abc' matches '*b*')
--- stdout
body 2
second-registered 1
first-registered 1
2
6 5
two takes 2 argument(s), got 3
two takes 2 argument(s), got 1
true
true false true true
--- status
0
