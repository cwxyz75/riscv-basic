# SHELL-CORPUS script 4 (adapted): count ERROR lines per hour from a stream, folding into a map
--- file /var/log/big.log
10:01 ERROR disk
10:05 INFO ok
10:59 ERROR net
11:00 ERROR cpu
11:30 WARN hot
--- script
counts = (cat /var/log/big.log | fold {} @ acc line do
    if (is not $line matches '*ERROR*') do ok $acc end
    hour = slice $line 0 2
    ok (set-in $acc [$hour] (calc 1 + (get-or $acc [$hour] 0)))
end)
print (to-json $counts)
--- stdout
{"10":2,"11":1}
--- status
0
