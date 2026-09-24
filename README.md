# rift-scratchpad

An i3-style scratchpad for [rift](https://github.com/acsandmann/rift), built on its window claim
API ([acsandmann/rift#519](https://github.com/acsandmann/rift/pull/519), not merged yet).

- **Park** claims the focused window from rift and moves it just offscreen, the way rift hides
  windows on inactive workspaces.
- **Toggle** hides the focused scratchpad window. Otherwise it brings back a visible one, or the
  next parked one, where it last sat. With nothing parked it parks the focused window.
- While a scratchpad window is shown, focus follows mouse is paused so hovering other windows
  doesn't bury it.
- Quitting puts parked windows back and hands every claimed window back to rift.

## Setup

```sh
cargo install --path .
```

In rift's config (the `pkill` lines make sure only one instance answers the keys):

```toml
[settings]
run_on_start = [
  "sh -c 'pkill -x rift-scratchpad; sleep 1; pkill -9 -x rift-scratchpad; exec ~/.cargo/bin/rift-scratchpad'",
]

[keys]
"Alt + Escape"         = { exec = ["/usr/bin/pkill", "-USR1", "-x", "rift-scratchpad"] } # toggle
"Alt + Shift + Escape" = { exec = ["/usr/bin/pkill", "-USR2", "-x", "rift-scratchpad"] } # park
```

It moves windows through the Accessibility API. If it exits with an Accessibility error, grant
the binary access under System Settings > Privacy & Security > Accessibility.

## Limits

- Parked windows don't survive a restart of the plugin or rift; they go back to rift instead.
- Focus follows mouse is paused globally while a scratchpad window is shown, not per display.
