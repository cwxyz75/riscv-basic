# $env is an ordinary binding: children get its value at start (A2), with-env adds for one command (A3)
--- env HOME=/home/ann
--- script
print $env.HOME
set env.EDITOR = 'kilo'
printenv EDITOR
with-env { "EDITOR": "vi", "N": "1" } printenv EDITOR N
printenv EDITOR
print ($env.NOPE ?? 'none')
set env.BAD = [1]
printenv EDITOR
print not-reached
--- stdout
/home/ann
EDITOR=kilo
EDITOR=vi
N=1
EDITOR=kilo
none
--- stderr
ish: $env.BAD is a list, not text: a program's environment holds only text
--- status
1
