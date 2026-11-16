# calc and is
--- script
print (calc 1 + 2 * 3)
print (calc (calc 1 + 2) * 3)
print (calc 7 / 2) (calc 7 // 2) (calc -7 // 2) (calc 7 % 3) (calc 0.1 + 0.2)
print (calc 0xff + 0b101)
print (is 1 < 2 and 2 < 3) (is not 1 == 1) (is 'a' == 'a') (is 1 == '1')
print (is 3 in [1, 2, 3]) (is 'k' in { "k": 1 })
print (is 'hello' matches 'h*o') (is 'hello' matches 'h?')
print ('a' ^ 'b' ^ 1)
print (is ('a' ^ 'b') == 'ab')
x = 4
print (calc $x * $x - 1)
print (is $x >= 4 or false)
--- stdout
7
9
3.5 3 -4 1 0.3
260
true false true false
true true
true false
ab1
true
15
true
--- status
0
