# tailpipe

An SSH server that plays back terminal recordings in [asciinema](https://asciinema.org) `.cast` format (both v2 and v3 supported).

    ssh demo@your-server -p 4242

## Controls

- **Space** — pause/resume
- **Left/Right** — seek backward/forward
- **o** — toggle overlay
- **q** / **Ctrl-C** — quit

## Configuration

```toml
[server]
port = 4242
host_key = "/etc/tailpipe_ed25519.key"  # optional, generates ephemeral key if omitted
header = "/srv/casts/header.cast"       # played before every session
footer = "/srv/casts/footer.cast"       # played after every session

[user.demo]
castfile = "/srv/casts/demo.cast"

[user.custom]
castfile = "/srv/casts/custom.cast"
header = "none"                         # skip header/footer for this user
footer = "none"

[user.default]                          # fallback for unknown usernames
castfile = "/srv/casts/default.cast"
```

## Usage

```
tailpipe [config.toml]
```

Defaults to `tailpipe.toml` in the current directory.

## Credits

- SSH server possible thanks to [russh](https://github.com/warp-tech/russh)
- [asciinema's rust code](https://github.com/asciinema/asciinema) was a delight to read, and super helpful
- [ratatui](https://github.com/ratatui/ratatui) for the pause overlay UI
