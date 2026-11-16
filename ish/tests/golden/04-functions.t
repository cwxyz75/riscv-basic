# fn, parameters, results (N2), ok, fail, and a call statement has no value (N3)
--- script
fn add a b do
    ok ($a + $b)
end
print (add 2 3)
fn last a do
    $a * 2
end
print (last 21)
fn noargs do
    print $#args $args
end
noargs x y z
fn early a do
    if $a > 0 do
        ok 'positive'
    end
    ok 'nonpositive'
end
print (early 5) (early -1)
fn say do
    echo from-external
end
r1 = try do (say) end catch e do $e.message end
print $r1
fn total xs do
    fold 0 @ a x do ok ($a + $x) end $xs
end
r2 = try do (total [1, 2]) end catch e do $e.message end
print $r2
fn total2 xs do
    ok (fold 0 @ a x do ok ($a + $x) end $xs)
end
print (total2 [1, 2, 3])
--- stdout
5
42
3 ["x","y","z"]
positive nonpositive
from-external
say ended with a call statement, so it has no value; write (echo ...) or ok (echo ...)
total ended with a call statement, so it has no value; write (fold ...) or ok (fold ...)
6
--- stderr
--- status
0
