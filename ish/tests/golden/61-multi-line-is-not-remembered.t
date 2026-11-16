# a command typed over several lines does not enter the history (Up finds nothing)
--- term
if true do\r
echo y\r
end\r
\e[A\x03
--- tty
$ if true do\n
> echo y\n
> end\n
y\n
$ ^C\n
$ 
