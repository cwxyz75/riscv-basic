# single quotes keep text literally; double quotes are JSON strings; ^ joins; a list splices into a command
--- script
echo '$x' "\u00e9\n" 'a'^'b'
xs = ["a b", "c"]
echo $xs
echo [1, 2]
m = { "k": 1 }
echo $m
--- stdout
$x é
 ab
a b c
1 2
--- stderr
ish: a map cannot be an argument
--- status
1
