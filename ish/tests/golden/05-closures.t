# closures capture by value; lambdas; higher-order builtins
--- script
n = 10
add_n = @ x do ok ($x + $n) end
set n = 99
print (call $add_n 1)
print (map @ x do ok ($x * $x) end [1, 2, 3])
print (filter @ x do ok ($x > 1) end [1, 2, 3])
print (fold 0 @ a x do ok ($a + $x) end [1, 2, 3, 4])
each @ x do print item $x end ["a", "b"]
$add_n 5
print ($add_n 5)
--- stdout
11
[1,4,9]
[2,3]
10
item a
item b
15
--- stderr
--- status
0
