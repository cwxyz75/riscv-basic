# Tab completes a path from the shell's table: a file gets a space, a directory a slash
--- file /home/hello.txt
hi
--- file /home/lines.txt
x
--- file /home/sub/inner.txt
y
--- term
cd /home\r
cat he\t\r
cat s\tin\t\r
--- tty
$ cd /home\n
$ cat hello.txt \n
hi\n
$ cat sub/inner.txt \n
y\n
$ 
