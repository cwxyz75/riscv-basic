# a command that cannot be started is an error, never a status
--- script
try do
    nosuch arg
end catch e do
    print $e.kind $e.command $e.message
end
echo still-running
nosuch2
echo not-reached
--- stdout
spawn nosuch not found
still-running
--- stderr
ish: nosuch2: not found
--- status
127
