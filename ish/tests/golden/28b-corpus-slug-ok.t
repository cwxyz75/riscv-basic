# the same tool with good arguments
--- file /lib/text.ish
fn slugify s do ok (lower (replace $s ' ' '-')) end
--- args -u Hello World again
--- script
text = import /lib/text.ish
opts = parse-args { "usage": "slug [-u] NAME...", "flags": { "-u": "upper" }, "rest": "names", "min": 1 } $args
for n in $opts.names do
    s = text.slugify $n
    print (if $opts.upper do (upper $s) else $s end)
end
eprint `slugged ${len $opts.names} names`
--- stdout
HELLO
WORLD
AGAIN
--- stderr
slugged 3 names
--- status
0
