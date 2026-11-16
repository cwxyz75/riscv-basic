# external pipelines, stream stages, redirection
--- script
seq 3 | upper-stdin
seq 5 | each @ l do print got $l end
seq 5 | take 2
x = seq 4 | map @ l do ok ((to-number $l) * 10) end
print $x
print (seq 4 | fold 0 @ a l do ok ($a + (to-number $l)) end)
print a b c | count-lines
echo hi > /out.txt
echo there >> /out.txt
cat /out.txt
count-lines < /out.txt
err oops >[2=1] > /both.txt
cat /both.txt
--- stdout
1
2
3
got 1
got 2
got 3
got 4
got 5
1
2
[10,20,30,40]
10
1
hi
there
2
oops
--- stderr
--- status
0
