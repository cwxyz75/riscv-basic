# J1, J3, J5: spawn returns a job at once, poll is null while it runs, wait gives run's map
--- script
job = spawn slow 50
print started
print (poll $job)
r = wait $job
print $r.status $r.out
print (poll $job)
c = spawn { "out": "capture" } slow 10 7
r2 = wait $c
print $r2.status $r2.out
--- stdout
slow 50 done
started
null
0 []
{"status":0,"out":[],"err":""}
7 ["slow 10 done"]
--- stderr
--- status
0
