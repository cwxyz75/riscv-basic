# J10: a job started inside confine keeps the narrowed table
--- script
j = confine { "/home": "r" } do ok (spawn { "out": "capture" } show-table) end
r = wait $j
print $r.out
--- stdout
["/home r",". r","/dev rw","/tmp rw"]
--- stderr
--- status
0
