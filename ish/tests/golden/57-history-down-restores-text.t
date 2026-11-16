# Down past the newest entry brings back what was being typed
--- term
echo a\r
print zz\e[A\e[B\r
--- tty
$ echo a\n
a\n
$ print zz\e[8Decho a\e[K\e[6Dprint zz\n
zz\n
$ 
