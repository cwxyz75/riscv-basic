# SHELL-CORPUS script 10 in grammar v2: parse-args, an imported library, a usage failure that becomes the exit status
--- file /lib/text.ish
fn slugify s do ok (lower (replace $s ' ' '-')) end
--- args -x
--- script
text = import /lib/text.ish
opts = parse-args { "usage": "slug [-u] NAME...", "flags": { "-u": "upper" }, "rest": "names", "min": 1 } $args
for n in $opts.names do
    s = text.slugify $n
    print (if $opts.upper do (upper $s) else $s end)
end
eprint `slugged ${len $opts.names} names`
--- stdout
--- stderr
unknown flag -x
usage: slug [-u] NAME...
--- status
2
