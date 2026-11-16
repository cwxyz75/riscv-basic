# Delete removes the character under the cursor and reprints the rest
--- term
echo abc\e[D\e[D\e[3~\r
--- tty
$ echo abc\e[1D\e[1Dc \e[2D\e[1C\n
ac\n
$ 
