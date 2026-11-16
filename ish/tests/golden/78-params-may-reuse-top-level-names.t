# B1: the script's top level counts as one function, and a parameter may reuse a top-level name
--- script
x = 'top'
fn f x do
    y = $x
    ok $y
end
print (f 'param') $x
--- stdout
param top
--- stderr
--- status
0
