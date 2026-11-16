# the prompt echoes a non-null value; lists of strings as lines; errors do not end the session
--- lines
len [1, 2, 3]
x = 4
(calc $x * 2)
[ "a", "b" ]
{ "k": 1 }
echo hi
nosuch
true
'text'
--- stdout
3
8
a
b
{"k":1}
hi
text
--- stderr
ish: nosuch: not found
--- status
0
