# fn, parameters, return values, ok, fail, last-position rule
--- script
fn add a b do
    ok (calc $a + $b)
end
print (add 2 3)
fn last a do
    (calc $a * 2)
end
print (last 21)
fn noargs do
    print $#args $args
end
noargs x y z
fn early a do
    if (is $a > 0) do
        ok positive
    end
    ok nonpositive
end
print (early 5) (early -1)
fn say do
    echo from-external
end
r = (say)
print $r
--- stdout
5
42
3 ["x","y","z"]
positive nonpositive
from-external
null
--- status
0
