# B2: set needs an existing binding
--- script
set nope = 1
--- stdout
--- stderr
ish: there is no binding `nope` to change here (`set` changes only bindings of the current function; write `nope = ...` to make one)
--- status
1
