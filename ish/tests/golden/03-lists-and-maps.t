# JSON literals, paths, `??`, set, and the data builtins
--- script
m = { "name": "ann", "tags": ["a", "b"], "n": { "k": 7 } }
print $m.name
print $m.tags.1
print $m.n.k
print $m.'name'
k = 'name'
print $m.($k)
print ($m.n.k ?? 0) ($m.zz ?? 'dflt') ($m.n.zz ?? $m.name)
print (get-or $m ["n", "k"] 0)
print (keys $m)
print (len $m) (len $m.tags) (len 'héllo')
set m = set-in $m ["n", "k"] 8
print $m.n.k
set m.n.k = 9
print $m.n.k
print (has $m ["n"]) (has $m ["x"])
n = del-in $m ["tags"]
print $n
print ((keys $m).1) (split ',' 'a,b,c').2
--- stdout
ann
b
7
ann
ann
7 dflt ann
7
["name","tags","n"]
3 2 5
8
9
true false
{"name":"ann","n":{"k":9}}
tags c
--- stderr
--- status
0
