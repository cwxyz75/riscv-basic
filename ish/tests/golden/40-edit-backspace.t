# a typed line is echoed as it is typed; Backspace erases with BS SP BS; the command runs after Enter
--- term
echo hel\x7flo\r
--- tty
$ echo hel\b \blo\n
helo\n
$ 
