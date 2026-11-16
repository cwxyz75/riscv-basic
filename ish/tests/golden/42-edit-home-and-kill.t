# Ctrl-A goes home, Ctrl-K kills to the end of the line (erase to end of line)
--- term
echo one two\x01\x0becho new\r
--- tty
$ echo one two\e[12D\e[Kecho new\n
new\n
$ 
