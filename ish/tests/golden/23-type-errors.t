# the diagnostics a script author sees
--- script
r = try do if 1 do print x end end catch e do print $e.message end
r = try do (calc 'a' + 1) end catch e do print $e.message end
r = try do (is 1 < 'a') end catch e do print $e.message end
r = try do print $nope end catch e do print $e.message end
r = try do (calc 1 / 0) end catch e do print $e.message end
r = try do ok (get-or 5 ["a"] 0) end catch e do print $e.message end
r = try do print $m.zz end catch e do print $e.kind end
m = {}
r = try do print $m.zz end catch e do print $e.kind $e.message end
r = try do x = len end catch e do print $e.message end
r = try do fn2 end catch e do print $e.message end
--- stdout
`if` needs a boolean, got number
`+` needs numbers, got a string and a number
`<` needs numbers, got a number and a string
`nope` is not set
division by zero
cannot look inside a number
error
missing-key no such key: zz
`len` is a command here: write (len) to call it or 'len' for the text
not found
--- status
0
