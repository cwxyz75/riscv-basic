# M6: the right side of = and set and a literal slot reject a lone bareword; parentheses and conditions run it
--- script
r1 = try do (date) end catch e do $e.message end
print $r1
print (len ['a', 'b'])
--- stdout
not found
2
--- stderr
--- status
0
