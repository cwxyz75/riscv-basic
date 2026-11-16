# Up past the oldest entry does nothing
--- term
echo a\r
\e[A\e[A\r
--- tty
$ echo a\n
a\n
$ echo a\n
a\n
$ 
