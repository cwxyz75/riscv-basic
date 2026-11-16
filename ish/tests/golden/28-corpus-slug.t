# SHELL-CORPUS script 10 (adapted): arguments, an imported library, a usage failure that becomes the exit status
--- file /lib/text.ish
fn slugify s do ok (lower (replace $s ' ' '-')) end
--- args -x
--- script
text = import /lib/text.ish
fn usage do
    eprint 'usage: slug [-u] NAME...'
    fail null 2
end
flags = {}
names = []
for a in $args do
    if (is $a == '-u') do
        flags = set-in $flags ["upper"] true
    else if (starts-with $a '-') do
        usage
    else
        names = append $names $a
    end
end
if (is (len $names) == 0) do usage end
for n in $names do
    s = text.slugify $n
    print (if (get-or $flags ["upper"] false) do upper $s else $s end)
end
--- stdout
--- stderr
usage: slug [-u] NAME...
ish: null
--- status
2
