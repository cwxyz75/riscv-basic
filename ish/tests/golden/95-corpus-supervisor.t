# SHELL-CORPUS (new): a supervisor restarts a service that keeps failing, up to a limit, and then gives up with an error that says so
--- script
fn supervise limit do
    tries = 0
    while true do
        set tries = $tries + 1
        r = run { "out": "capture" } exit-with (if $tries < 3 do '1' else '0' end)
        if $r.status == 0 do
            print `up after ${$tries} tries`
            ok $tries
        end
        print `try ${$tries} failed with status ${$r.status}`
        if $tries >= $limit do
            fail `gave up after ${$tries} tries`
        end
    end
end
n = supervise 5
print `supervised: ${$n}`
r = try do supervise 2 end catch e do $e end
print $r
supervise 1
print not-reached
--- stdout
try 1 failed with status 1
try 2 failed with status 1
up after 3 tries
supervised: 3
try 1 failed with status 1
try 2 failed with status 1
gave up after 2 tries
try 1 failed with status 1
--- stderr
ish: gave up after 1 tries
--- status
1
