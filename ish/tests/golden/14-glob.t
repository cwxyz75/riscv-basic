# patterns expand in a word; no match is an error
--- script
echo /d/*.txt
for f in /d/?.log do echo f=$f end
echo /d/*.none
--- file /d/a.txt
x
--- file /d/b.txt
y
--- file /d/1.log
z
--- stdout
/d/a.txt /d/b.txt
f=/d/1.log
--- stderr
ish: no match for /d/*.none
--- status
1
