# a call has a leading bareword; `$f x` is a call; `>` is a comparison in a formula and a redirection in a call; M4
--- script
fn double x do ok ($x * 2) end
f = @ x do ok ($x + 1) end
print (double 4)
v = $f 9
print $v
big = (len [1, 2, 3]) > 2
print $big
if (len [1, 2, 3]) > 2 and not false do print yes end
if has { "k": 1 } 'k' do print has-k end
echo hi > /o.txt
print (read-file '/o.txt')
x = 2
print ($x > 1) ($x > 5)
l = [1, double 3, 2 + 3, (double 5), $x]
print $l
--- stdout
8
10
true
yes
has-k
hi

true false
[1,6,5,10,2]
--- stderr
--- status
0
