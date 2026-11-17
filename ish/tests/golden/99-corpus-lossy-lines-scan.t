# SHELL-CORPUS (new): scan a log that is not valid UTF-8 with lossy-lines (A4): the bad bytes become U+FFFD, the lines are kept
--- file-hex /var/log/app.log
494e464f20737461727465640a
4552524f5220626164206279746520ff6865726520282066660a
494e464f20646f6e650a
4552524f52206c6173740a
--- script
stats = cat /var/log/app.log | lossy-lines | fold { "n": 0, "errors": 0, "bad": 0 } @ acc line do
    set acc.n = $acc.n + 1
    if $line matches 'ERROR*' do set acc.errors = $acc.errors + 1 end
    if $line matches "*\ufffd*" do set acc.bad = $acc.bad + 1 end
    $acc
end
print `${$stats.n} lines, ${$stats.errors} errors, ${$stats.bad} with bad bytes`
--- stdout
4 lines, 2 errors, 1 with bad bytes
--- stderr
--- status
0
