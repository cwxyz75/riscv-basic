# Ctrl-C abandons the line (^C and a new prompt); the console's own interrupt event does the same at an idle prompt
--- term
echo abc\x03
@interrupt
echo ok\r
--- tty
$ echo abc^C\n
$ ^C\n
$ echo ok\n
ok\n
$ 
