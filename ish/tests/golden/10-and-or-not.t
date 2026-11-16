# success-based flow: &&, ||, !, and what a failing test does not do
--- script
true && echo yes
false && echo no
false || echo fallback
true || echo skipped
! false && echo negated
! true || echo negated-false
false && true || echo chain
print after
true && false
print not-reached
--- stdout
yes
fallback
negated
negated-false
chain
after
--- stderr
ish: false: exit status 1
--- status
1
