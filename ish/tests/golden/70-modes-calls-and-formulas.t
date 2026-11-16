# M1 to M5: commands take words, expressions compute; barewords that read as numbers are numbers in expressions and text for programs
--- script
echo 1.10 true null 'x'
n = 1.10
print $n
print (echo 1.10)
print (1.10 + 0) (true and not false) (null == null)
--- stdout
1.10 true null x
1.1
["1.10"]
1.1 true true
--- stderr
--- status
0
