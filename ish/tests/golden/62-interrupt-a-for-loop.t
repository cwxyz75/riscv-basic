# the loop itself is a place Ctrl-C is noticed, even with an empty body
--- interrupt-after 3
--- term
for i in (range 100000) do end\r
--- tty
$ for i in (range 100000) do end\n
^C\n
$ 
