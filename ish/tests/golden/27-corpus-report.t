# SHELL-CORPUS script 9 (adapted): CSV in, exact decimal totals, TSV out
--- file /sales.csv
region,amount
north,10.25
south,3.10
north,0.05
south,1.9
--- script
rows   = from-csv (read-file /sales.csv) { "header": true }
totals = {}
for r in $rows do
    region = get-or $r ["region"] unknown
    amount = to-number (get-or $r ["amount"] '0')
    totals = set-in $totals [$region] (calc (get-or $totals [$region] 0) + $amount)
end
table = map @ k do ok [$k, (get-or $totals [$k] 0)] end (sort (keys $totals))
print (to-tsv $table)
print (is 0.1 + 0.2 == 0.3)
--- stdout
north	10.3
south	5

true
--- status
0
