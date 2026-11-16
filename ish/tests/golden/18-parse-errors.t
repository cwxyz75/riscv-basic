# errors name the place; nothing runs from a script that does not parse
--- script
echo before
if true do
echo never
--- stderr
ish: expected `end` at line 4 column 1
--- status
2
