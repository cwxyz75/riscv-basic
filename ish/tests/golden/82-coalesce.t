# E5: `p ?? d` gives d only when the key or index is missing; a key holding null gives null
--- script
m = { "a": null, "b": 5, "l": [10, 20] }
print ($m.a ?? 'd') ($m.zz ?? 'd') ($m.b ?? 'd') ($m.l.1 ?? 'd') ($m.l.7 ?? 'd')
print ($m.zz ?? $m.b ?? 0) ($m.zz ?? $m.yy ?? 'last')
print (-$m.b ?? 0)
x = { "k": { "j": 1 } }
print ($x.k.j ?? 0) ($x.q.j ?? 0)
--- stdout
null d 5 20 d
5 last
-5
1 0
--- stderr
--- status
0
