# each, map, filter, fold and take as pipeline stages; early stop ignores the producer's exit status
--- script
seq 6 | filter @ l do ok ((to-number $l) % 2 == 0) end
print (seq 6 | filter @ l do ok ((to-number $l) % 2 == 0) end)
print (many-then-fail 2 | take 2)
print (seq 5 | map @ l do ok (to-number $l) end | take 3)
print (seq 3 | each @ l do print seen $l end)
print a b | upper-stdin
print (print x | count-lines)
--- stdout
2
4
6
["2","4","6"]
["0","1"]
[1,2,3]
seen 1
seen 2
seen 3
null
A B
["1"]
--- stderr
--- status
0
