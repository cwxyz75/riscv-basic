# A1: too few arguments
--- script
spec = { "usage": "tool NAME", "flags": {}, "rest": "names", "min": 1 }
r = parse-args $spec []
--- stdout
--- stderr
missing arguments
usage: tool NAME
--- status
2
