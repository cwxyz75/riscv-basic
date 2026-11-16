# JSON literals, accessors, builtins
--- script
m = { "name": "ann", "tags": ["a", "b"], "n": { "k": 7 } }
print $m.name
print $m.tags.1
print $m.n.k
print $m.'name'
k = name
print $m.($k)
print (get-or $m ["n", "k"] 0) (get-or $m ["zz"] 'dflt')
print (keys $m)
print (len $m) (len $m.tags) (len 'héllo')
m = set-in $m ["n", "k"] 8
print $m.n.k
m.n.k = 9
print $m.n.k
print (has $m ["n"]) (has $m ["x"])
n = (del-in $m ["tags"])
print $n
--- stdout
ann
b
7
ann
ann
7 dflt
["name","tags","n"]
3 2 5
8
9
true false
{"name":"ann","n":{"k":9}}
--- status
0
