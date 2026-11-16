# Tab at the start of a command completes a program name from /bin and adds a space; a list on the second Tab
--- file /bin/cat
x
--- file /bin/echo
x
--- file /bin/ls
x
--- term
ec\t\r
l\t\t\x03
--- tty
$ echo \n
\n
$ l\n
len  lines  lines-of  list  lower  ls\n
$ l^C\n
$ 
