# import returns a map of the module's top-level names; qualified calls; siblings; cached
--- file /lib/text.ish
fn shout s do ok (upper $s) end
fn twice s do ok (shout $s ^ $s) end
width = 8
print loading
--- script
text = import /lib/text.ish
print (text.shout 'abc')
print (text.twice 'ab')
again = import /lib/text.ish
print (keys $again)
print $again.width
fn shout s do ok 'mine' end
print (shout 'x') (text.shout 'x')
--- stdout
loading
ABC
ABAB
["shout","twice","width"]
8
mine X
--- stderr
--- status
0
