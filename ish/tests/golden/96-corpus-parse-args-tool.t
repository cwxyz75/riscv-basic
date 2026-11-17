# SHELL-CORPUS (new): a command-line tool built on parse-args: flags with values, a rest list, defaults, and the usage errors
--- args --out report.txt -v a b
--- script
spec = { "usage": "tool [-v] [--out FILE] NAME...", "flags": { "-v": "verbose", "--out": { "name": "out", "value": true } }, "rest": "names", "min": 1 }
o = parse-args $spec $args
print (to-json $o)
if $o.verbose do eprint `writing ${len $o.names} names to ${$o.out}` end
for n in $o.names do print (upper $n) end
--- stdout
{"verbose":true,"out":"report.txt","names":["a","b"]}
A
B
--- stderr
writing 2 names to report.txt
--- status
0
