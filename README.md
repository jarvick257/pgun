# pgun

Portal gun for SSH tunnels. TUI tunnel manager.

Spawn, track, and open local-forward SSH tunnels to services on remote hosts from a terminal UI.

## Install

```sh
cargo install --path .
```

Requires `ssh` on `PATH`.

## Usage

```sh
pgun                    # use $XDG_CONFIG_HOME/pgun/config.toml
pgun --config ./my.toml
```

## Config

`config.toml`:

```toml
[[hosts]]
name = "prod"
ssh  = "user@prod.example.com"

  [[hosts.services]]
  name   = "grafana"
  port   = 3000
  scheme     = "http"   # optional, default "http"
  path       = "/"      # optional, default "/"
  local_port = 3000     # optional, pin local port (default: ephemeral)

  [[hosts.services]]
  name = "postgres"
  port = 5432
```

`ssh` is passed straight to the `ssh` command, so `~/.ssh/config` aliases work.

## Keys

| key | action |
|---|---|
| `j`/`k` `↑`/`↓` | move |
| `h`/`l` `←`/`→` | collapse / expand |
| `Enter` | drill into host / toggle service tunnel |
| `c` | toggle tunnel under cursor |
| `o` | open service URL in browser |
| `a` / `A` | add service / add host |
| `e` | edit |
| `d` | delete |
| `r` | reload config |
| `L` | toggle log pane |
| `?` | help |
| `q` | quit |

Started tunnels pick a free local port and forward it to `service.port` on the remote. `o` opens `scheme://127.0.0.1:<local>/path`.
