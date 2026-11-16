# Ctrl-C stops a loop written in shell code
--- interrupt-after 6
--- term
while true do end\r
echo after\r
--- tty
$ while true do end\n
^C\n
$ echo after\n
after\n
$ 
