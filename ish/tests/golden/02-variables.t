# `=` creates a binding, `$x` reads it, `$#x` counts, `^` and adjacency join
--- script
x = 'hello'
y = 5
echo $x $y
echo $x^'-'^$y
echo pre$x
z = 'a b'
echo $z
l = [1, 2, 3]
echo $#l $l
print $l
print $#x
--- stdout
hello 5
hello-5
prehello
a b
3 1 2 3
[1,2,3]
5
--- stderr
--- status
0
