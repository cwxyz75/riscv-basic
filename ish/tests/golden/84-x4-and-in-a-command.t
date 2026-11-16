# X4: and/or at the start of a command
--- script
echo a
and echo b
--- stdout
--- stderr
ish: `and` joins values; between commands write `&&` at line 2 column 1
--- status
2
