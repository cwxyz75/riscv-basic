# stdin goes to the first command; take stops early and ignores the producer's status
--- stdin
alpha
beta
gamma
--- script
cat | each @ l do print '>' $l end
many-then-fail 5 | take 2
print (many-then-fail 3 | take 1)
many-then-fail 2
--- stdout
> alpha
> beta
> gamma
0
1
["0"]
0
1
--- stderr
ish: many-then-fail: exit status 141
--- status
141
