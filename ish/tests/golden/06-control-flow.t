# conditions are expressions; else if; for takes words; while, until, break, continue; a block's bindings end at its end
--- script
for x in 1 2 3 do
    if $x == 2 do
        continue
    else if $x == 3 do
        print three
    else
        print $x
    end
end
i = 0
while $i < 3 do
    set i = $i + 1
    if $i == 2 do break end
end
print $i
until $i == 5 do set i = $i + 1 end
print $i
for y in (range 3) do print $y end
v = if 1 < 2 do 'yes' else 'no' end
print $v
w = if $i > 100 and not $i == 7 do 'big' else if $i == 5 do 'five' else 'other' end
print $w
--- stdout
1
three
2
5
0
1
2
yes
five
--- stderr
--- status
0
