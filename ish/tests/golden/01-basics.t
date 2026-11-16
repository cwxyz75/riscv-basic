# commands, comments, separators, quoting
--- script
echo hello world   # a comment
echo one; echo two
echo 'it''s'  "a\tb" 'sp  ace'
echo a'b'"c"d
echo ''
--- stdout
hello world
one
two
it's a	b sp  ace
abcd

--- status
0
