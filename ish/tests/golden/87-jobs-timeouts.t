# J7: wait with a timeout raises `timeout` and leaves the job running; run with a timeout kills the command first
--- script
h = spawn { "out": "discard" } hang
try do wait { "timeout": 100 } $h end catch e do print $e.kind $e.message end
print (poll $h)
kill $h
print (wait $h).status
try do run { "timeout": 50 } hang end catch e do print caught $e.kind end
r = run { "timeout": 500 } slow 20
print $r.status
--- stdout
timeout timed out after 100 ms
null
137
hanging
caught timeout
slow 20 done
0
--- stderr
--- status
0
