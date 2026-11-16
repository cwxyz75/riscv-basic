# Left moves the cursor; a character typed in the middle reprints the rest and moves back; Enter moves to the end first
--- term
echo ac\e[Db\r
--- tty
$ echo ac\e[1Dbc\e[1D\e[1C\n
abc\n
$ 
