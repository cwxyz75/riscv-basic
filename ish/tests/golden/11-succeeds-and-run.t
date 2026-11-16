# succeeds is a boolean, run never raises for a non-zero exit
--- script
print (succeeds (true)) (succeeds (false))
if (succeeds (cat /hay.txt | grep needle)) do print found else print missing end
r = run (exit-with 3)
print $r.status $r.out $r.err
r = run (fail-msg bad 7) 
print $r.status $r.err
r = run (seq 3) { "max": 100 }
print $r.status $r.out
r = run (echo out; err oops)
print $r
print (succeeds (nosuchprogram))
print unreachable
--- file /hay.txt
a needle here
--- stdout
true false
a needle here
found
3 [] 
7 bad

0 ["1","2","3"]
{"status":0,"out":["out"],"err":"oops\n"}
--- stderr
ish: nosuchprogram: not found
--- status
127
