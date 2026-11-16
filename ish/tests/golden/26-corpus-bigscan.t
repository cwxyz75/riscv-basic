# SHELL-CORPUS script 4 in grammar v2: count ERROR lines per hour from a stream, lossy, folding into a map
--- file /var/log/big.log
10:01 ERROR disk
10:05 INFO ok
10:59 ERROR net
11:00 ERROR cpu
11:30 WARN hot
--- script
counts = cat /var/log/big.log | lossy-lines | fold {} @ acc line do
    if not $line matches '*ERROR*' do ok $acc end
    hour = slice $line 0 2
    set acc.($hour) = ($acc.($hour) ?? 0) + 1
    $acc
end
print (to-json $counts)
--- stdout
{"10":2,"11":1}
--- stderr
--- status
0
