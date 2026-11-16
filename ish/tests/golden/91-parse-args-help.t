# A1: --help prints the usage and ends the script
--- script
spec = { "usage": "tool [-v] NAME", "flags": { "-v": "verbose" }, "rest": "names" }
r = parse-args $spec ['--help']
print not-reached
--- stdout
usage: tool [-v] NAME
--- stderr
--- status
0
