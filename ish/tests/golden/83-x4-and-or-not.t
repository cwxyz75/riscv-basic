# X4: ! && || join commands, not and or join values; the wrong one is an error that names the right one
--- script
r1 = try do print (1 < 2 && 2 < 3) end catch e do $e.message end
print $r1
--- stdout
--- stderr
ish: `&&` and `||` join commands; between values write `and` or `or` at line 1 column 26
--- status
2
