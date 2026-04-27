# config

`jjpr config` manages config files. Use the field reference in
[Configuration](../configuration.md) for what to put in them.

```
jjpr config init                      # create global config at ~/.config/jjpr/config.toml
jjpr config init --repo               # create repo-local config at .jj/jjpr.toml
```

## init

Writes a config file populated with defaults. Without `--repo`, writes
the global config at `~/.config/jjpr/config.toml` (or
`$XDG_CONFIG_HOME/jjpr/config.toml` if that's set). With `--repo`,
writes `.jj/jjpr.toml` in the current repo.

If the target file already exists, `init` refuses to overwrite it.
