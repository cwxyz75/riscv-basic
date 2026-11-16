# SHELL-CORPUS script 2 (adapted): change nested keys, write the file back
--- file /etc/alice.json
{ "shell": { "history": { "size": 100 } }, "env": { "EDITOR": "vi" } }
--- script
path = /etc/alice.json
cfg  = from-json (read-file $path)
cfg  = set-in $cfg ["shell", "history", "size"] 5000
cfg.env.EDITOR = kilo
write-file ($path ^ '.new') (to-json $cfg { "indent": 2 })
print (read-file /etc/alice.json.new)
--- stdout
{
  "shell": {
    "history": {
      "size": 5000
    }
  },
  "env": {
    "EDITOR": "kilo"
  }
}
--- status
0
