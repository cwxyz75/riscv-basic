# a capture past the cap fails loudly instead of exhausting memory
--- script
r = try do
    x = (loud 100000)
end catch e do
    print $e.kind
end
print (len (loud 3))
r = try do
    x = (lines-of do for i in (range 40000) do print 0123456789012345678901234567890123456789 end end)
end catch e do print $e.kind end
--- stdout
capture-limit
3
capture-limit
--- status
0
