# not on a terminal: no prompt, no echo, no banner; a script from standard input
--- stdin
print one
echo two
if true do
    print three
end
--- batch
--- stdout
one
two
three
--- status
0
