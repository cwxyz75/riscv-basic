# A1: parse-args
--- script
spec = { "usage": "tool [-v] [--out FILE] NAME...", "flags": { "-v": "verbose", "--out": { "name": "out", "value": true } }, "rest": "names", "min": 1 }
print (to-json (parse-args $spec ['a', 'b']))
print (to-json (parse-args $spec ['-v', '--out', 'f.txt', 'x']))
print (to-json (parse-args $spec ['--out=g.txt', '--', '-v']))
--- stdout
{"verbose":false,"out":false,"names":["a","b"]}
{"verbose":true,"out":"f.txt","names":["x"]}
{"verbose":false,"out":"g.txt","names":["-v"]}
--- stderr
--- status
0
