# a spawned shell command runs now to its end (no threads in the shell yet); a job is a handle: stored, never serialized
--- script
j = spawn { "out": "capture" } print hello
r = wait $j
print $r.out
store = { "job": $j }
t = try do print (to-json $store) end catch e do $e.message end
print $t
print (type $j)
--- stdout
["hello"]
a job cannot be serialized
job
--- stderr
--- status
0
