# N2 and N3: a call statement has no value, and the error names the fix
--- script
fn total xs do
    fold 0 @ a x do ok ($a + $x) end $xs
end
t = total [1, 2, 3]
--- stdout
--- stderr
ish: total ended with a call statement, so it has no value; write (fold ...) or ok (fold ...)
--- status
1
