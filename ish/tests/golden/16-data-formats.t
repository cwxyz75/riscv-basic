# JSON, CSV and TSV primitives
--- script
j = from-json '{"a": [1, 2.50, {"b": null}], "c": "x\\ny"}'
print $j.a.1 $j.a.2.b
print (to-json $j)
print (to-json { "k": [1, 2] } { "indent": 2 })
rows = from-csv "name,age\nann,30\nbo,\"4,5\"\n" { "header": true }
print $rows.1.age
print (to-csv $rows { "header": true })
print (to-tsv [["a", "b\tc"], [1, true]])
t = from-tsv "x\ty\n1\t2\n"
print $t
print (to-number '12.50') (str 5) (format 'n=%s s=%s %%' 7 hi)
fn tojson v do ok (to-json $v) end
print (tojson [1])
r = try do from-json '{bad' end catch e do $e.kind end
print $r
--- stdout
2.5 null
{"a":[1,2.5,{"b":null}],"c":"x\\ny"}
{
  "k": [
    1,
    2
  ]
}
4,5
name,age
ann,30
bo,"4,5"

a	b\tc
1	true

[["x","y"],["1","2"]]
12.5 5 n=7 s=hi %
[1]
bad-json
--- status
0
