# A1: a bad flag prints the usage to stderr and fails with code 2
--- script
spec = { "usage": "tool [-v] NAME", "flags": { "-v": "verbose" }, "rest": "names", "min": 1 }
r = parse-args $spec ['-x']
print not-reached
--- stdout
--- stderr
unknown flag -x
usage: tool [-v] NAME
--- status
2
