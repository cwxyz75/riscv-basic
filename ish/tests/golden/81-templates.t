# T1 and T2: `$name` stops at the name, ${expr} inserts an expression, escapes, a leading newline drops the indentation
--- script
base = 'notes'
n = 3
xs = ['a', 'b']
print `$base.txt`
print `${$base}.txt has ${len $xs} items and ${$n + 1} next; $n$n`
print `a\tb \$base \` \u00e9`
print `
    line one
      indented $base
    last
`
r = try do print `${$xs}` end catch e do $e.message end
print $r
print `${true} ${null} ${1.50}`
--- stdout
notes.txt
notes.txt has 2 items and 4 next; 33
a	b $base ` é
line one
  indented notes
last

a list cannot go in a template hole (join a list explicitly)
true null 1.5
--- stderr
--- status
0
