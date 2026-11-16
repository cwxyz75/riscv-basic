# Up walks back through the history, replacing the line; commands that were run are remembered
--- term
echo a\r
echo b\r
\e[A\e[A\r
--- tty
$ echo a\n
a\n
$ echo b\n
b\n
$ echo b\e[6Decho a\n
a\n
$ 
