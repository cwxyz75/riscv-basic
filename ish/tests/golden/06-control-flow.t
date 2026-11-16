# if, else if, else, for, while, until, break, continue
--- script
for x in 1 2 3 do
    if (is $x == 2) do
        continue
    else if (is $x == 3) do
        print three
    else
        print $x
    end
end
i = 0
while (is $i < 3) do
    i = (calc $i + 1)
    if (is $i == 2) do break end
end
print $i
until (is $i == 5) do i = (calc $i + 1) end
print $i
for y in (range 3) do print $y end
v = (if (is 1 < 2) do 'yes' else 'no' end)
print $v
--- stdout
1
three
2
5
0
1
2
yes
--- status
0
