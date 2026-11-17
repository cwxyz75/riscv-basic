# SHELL-CORPUS (new): a script that holds a background job and a temporary state; an interrupt runs its defer blocks (J8, J9), in reverse order, and the job is killed
--- interrupt-after 6
--- script
fn work do
    defer do print remove-lock end
    j = spawn { "out": "discard" } hang
    defer do kill $j; print job-killed end
    print working
    while true do end
end
work
print not-reached
--- stdout
working
job-killed
remove-lock
^C
--- stderr
--- status
130
