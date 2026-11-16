# import returns a map of the module's functions; qualified calls; siblings; cached
--- file /lib/text.ish
fn shout s do ok (upper $s) end
fn twice s do ok (shout $s ^ $s) end
print loading
--- script
text = import /lib/text.ish
print (text.shout abc)
print (text.twice ab)
again = import /lib/text.ish
print (keys $again)
fn shout s do ok mine end
print (shout x) (text.shout x)
--- stdout
loading
ABC
ABAB
["shout","twice"]
mine X
--- status
0
