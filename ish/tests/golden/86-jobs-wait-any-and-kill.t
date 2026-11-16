# J4, J6: wait-any returns the first job to end; kill ends a job
--- script
a = spawn { "out": "discard" } slow 30
b = spawn { "out": "discard" } slow 10 4
first = wait-any [$a, $b]
print ($first.job == $b) $first.result.status
h = spawn { "out": "discard" } hang
kill $h
r = wait $h
print $r.status
rest = wait $a
print $rest.status
--- stdout
true 4
137
0
--- stderr
--- status
0
