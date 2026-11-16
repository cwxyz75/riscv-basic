# Down past the newest entry restores what was being typed (here: nothing), and an empty line runs nothing
--- term
echo a\r
\e[A\e[B\r
--- tty
$ echo a\n
a\n
$ echo a\e[6D\e[K\n
$ 
