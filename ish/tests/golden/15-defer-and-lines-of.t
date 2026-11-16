# defer runs at function exit, even on failure; lines-of captures a block's output
--- script
fn f do
    defer do print cleanup end
    print working
    fail oops 9
end
try do f end catch e do print caught $e end
fn g do
    defer do print g-done end
    print g-body
end
g
lines = (lines-of do echo a; echo b end)
print $lines
print (lines-of do print x; print y end)
defer do print script-end end
print last
--- stdout
working
cleanup
caught oops
g-body
g-done
["a","b"]
["x","y"]
last
script-end
--- status
0
