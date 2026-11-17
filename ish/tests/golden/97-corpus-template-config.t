# SHELL-CORPUS (new): render a configuration file from a template and a map of values, with defaults from ?? and a loop
--- file /etc/hosts.json
{ "hosts": [ { "name": "alpha", "port": 8080 }, { "name": "beta" } ], "domain": "example.org" }
--- script
cfg = from-json (cat /etc/hosts.json)
default_port = 80
lines = []
for h in $cfg.hosts do
    port = $h.port ?? $default_port
    set lines = append $lines `server ${$h.name}.${$cfg.domain}:${$port}`
end
text = join "\n" $lines
print $text
print (len $lines)
--- stdout
server alpha.example.org:8080
server beta.example.org:80
2
--- stderr
--- status
0
