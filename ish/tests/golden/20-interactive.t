# at the prompt a top-level call to a builtin or shell function that gives a value shows it (I1); lists of strings as lines
--- lines
len [1, 2, 3]
x = 4
$x * 2
["a", "b"]
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
