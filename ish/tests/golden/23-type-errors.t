# the diagnostics a script author sees
--- script
try do if 1 do print x end end catch e do print $e.message end
try do 'a' + 1 end catch e do print $e.message end
try do print (1 < 'a') end catch e do print $e.message end
try do print $nope end catch e do print $e.message end
try do print (1 / 0) end catch e do print $e.message end
try do print (get-or 5 ["a"] 0) end catch e do print $e.message end
m = {}
try do print $m.zz end catch e do print $e.kind $e.message end
try do print ($m.zz ?? 'fine') end catch e do print $e.kind end
try do print (nosuchprogram) end catch e do print $e.message end
try do print (print 'x') end catch e do print $e.message end
--- stdout
`if` needs a boolean, got number
`+` needs numbers, got a string and a number
`<` needs numbers, got a number and a string
`nope` is not set
division by zero
cannot look inside a number
missing-key no such key: zz
fine
not found
x
null
--- stderr
--- status
0
