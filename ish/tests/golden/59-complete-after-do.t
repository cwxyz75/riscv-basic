# a command name is completed after `do` (the start of a command), not only at the start of the line
--- file /bin/echo
x
--- term
if true do ec\t\x03
--- tty
$ if true do echo ^C\n
$ 
