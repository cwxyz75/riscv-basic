# ns lists the table; confine narrows what programs started inside can reach, and only narrows (SHI2)
--- script
for e in (ns) do print $e.path $e.rights end
confine { "/home": "r", "/dev": "rw" } do show-table end
r = confine { "/home": "r" } do
    ok (len (ns))
end
print $r
confine { "/home": "rw" } do
    show-table
    confine { "/home/sub": "r" } do show-table end
    r = try do
        confine { "/etc": "r" } do show-table end
    end catch e do $e.message end
    print $r
    r = try do
        confine { "/home": "r" } do
            confine { "/home": "rw" } do show-table end
        end
    end catch e do $e.message end
    print $r
end
print (len (ns))
--- stdout
/ rw
. r
/dev rw
/tmp rw
/home r
/dev rw
. r
/tmp rw
4
/home rw
. r
/dev rw
/tmp rw
/home/sub r
. r
/dev rw
/tmp rw
confine: permission denied
confine: permission denied
4
--- status
0
