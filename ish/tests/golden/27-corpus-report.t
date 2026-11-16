# SHELL-CORPUS script 9 in grammar v2: CSV in, exact decimal totals, TSV out
--- file /sales.csv
region,amount
north,10.25
south,3.10
north,0.05
south,1.9
--- script
rows = from-csv (read-file /sales.csv) { "header": true }
totals = {}
for r in $rows do
    region = $r.region ?? 'unknown'
    amount = to-number ($r.amount ?? '0')
    set totals.($region) = ($totals.($region) ?? 0) + $amount
end
table = map @ k do ok [$k, $totals.($k)] end (sort (keys $totals))
print (to-tsv $table)
print (0.1 + 0.2 == 0.3)
--- stdout
north	10.3
south	5

true
--- stderr
--- status
0
