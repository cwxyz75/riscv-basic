# formulas (M3): numbers, comparison, logic, strings; the call/formula rule; precedence
--- script
print (1 + 2 * 3) ((1 + 2) * 3) ((len [1, 2]) + 1)
print (7 / 2) (7 // 2) (-7 // 2) (7 % 3) (0.1 + 0.2)
print (255 + 5)
print (1 < 2 and 2 < 3) (not 1 == 1) ('a' == 'a') (1 == '1')
print (3 in [1, 2, 3]) ('k' in { "k": 1 })
print ('hello' matches 'h*o') ('hello' matches 'h?')
print ('a' ^ 'b' ^ 1)
x = 4
print ($x * $x - 1)
print ($x >= 4 or false)
print (-$x + 1) (-3 + 1)
y = $x + 1
print $y
z = (len [1, 2, 3]) > 2
print $z
print (1 + 2 < 4 and 'ab' == 'a' ^ 'b')
--- stdout
7 9 3
3.5 3 -4 1 0.3
260
true false true false
true true
true false
ab1
15
true
-3 -2
5
true
true
--- stderr
--- status
0
