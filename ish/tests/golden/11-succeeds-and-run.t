# succeeds is a boolean and run never raises for a non-zero exit (C4); both take the rest of the pipeline
--- file /hay.txt
a needle here
--- script
print (succeeds true) (succeeds false)
found = succeeds cat /hay.txt | grep needle
print $found
if succeeds grep zzz /hay.txt do print found else print missing end
r = run exit-with 3
print $r.status $r.out $r.err
r2 = run fail-msg bad 7
print $r2.status $r2.err
r3 = run { "max": 100 } seq 3
print $r3.status $r3.out
r4 = run { "err": "capture" } err oops
print $r4
r5 = run cat /hay.txt | upper-stdin
print $r5.out
print (succeeds nosuchprogram)
print unreachable
--- stdout
true false
a needle here
true
missing
3 [] 
7 bad

0 ["1","2","3"]
{"status":0,"out":[],"err":"oops\n"}
["A NEEDLE HERE"]
--- stderr
ish: nosuchprogram: not found
--- status
127
