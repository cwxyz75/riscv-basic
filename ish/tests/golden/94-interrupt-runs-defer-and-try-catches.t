# J9: an interrupt is a failure at the next statement boundary: defer runs and try can catch it
--- interrupt-after 3
--- script
fn work do
    defer do print cleanup end
    while true do end
end
try do work end catch e do print caught $e end
print after
--- stdout
cleanup
caught interrupted
after
--- stderr
--- status
0
