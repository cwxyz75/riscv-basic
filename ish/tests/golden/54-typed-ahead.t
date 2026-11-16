# several lines arriving in one read all run, in order
--- term
echo one\recho two\r
--- tty
$ echo one\n
echo two\n
one\n
$ two\n
$ 
