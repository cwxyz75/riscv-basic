# a command the terminal stopped (Ctrl-Z) is a numbered job: `jobs` lists the table, `bg` lets it run on, `fg` waits for it
--- script
stopme one
print (jobs)
bg
print (jobs)
fg
print (jobs)
stopme two
stopme three
for j in (jobs) do print $j.id $j.state $j.name end
fg 2
bg 3
print (len (jobs))
fg 9
--- stdout
[{"id":1,"state":"Stopped","name":"stopme one"}]
[{"id":1,"state":"Running","name":"stopme one"}]
[]
2 Stopped stopme two
3 Stopped stopme three
1
--- stderr
ish: fg: 9: no such job
--- status
1
