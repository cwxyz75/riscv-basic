# B3: a binding made inside a block ends at its end; a value chosen in a branch is written with `=` and `if`
--- script
if true do
    inner = 1
    print $inner
end
inner = 2
print $inner
x = if $inner == 2 do 'two' else 'other' end
print $x
--- stdout
1
2
two
--- stderr
--- status
0
