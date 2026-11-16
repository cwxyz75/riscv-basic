# fail, try/catch, exit status of the script, uncaught failures
--- script
r = try do
    fail { "kind": "x", "why": 1 } 3
end catch e c do
    print caught $e.kind $c
    'handled'
end
print $r
try do false end catch e do print $e.kind $e.status $e.command end
fail 'it broke' 4
print not-reached
--- stdout
caught x 3
handled
exit 1 false
--- stderr
ish: it broke
--- status
4
