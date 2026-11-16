# Ctrl-W deletes the word before the cursor
--- term
echo hello world\x17\x17x\r
--- tty
$ echo hello world\e[5D\e[K\e[6D\e[Kx\n
x\n
$ 
