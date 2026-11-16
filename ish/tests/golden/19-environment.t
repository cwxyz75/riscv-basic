# export, $env, the host environment
--- env HOME=/home/ann
--- script
print $env.HOME
export EDITOR kilo
print $env.EDITOR
print (get-or $env ["NOPE"] none)
cd /d
print (pwd)
--- file /d/x
1
--- stdout
/home/ann
kilo
none
/d
--- status
0
