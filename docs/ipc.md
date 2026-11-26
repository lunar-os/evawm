# EvaWM IPC (`evawm msg`)

EvaWM exposes a native newline-delimited JSON Unix socket.

## Socket

- Path: `$XDG_RUNTIME_DIR/evawm-$WAYLAND_DISPLAY.sock`
- Exported as `EVAWM_SOCKET` in child processes and the systemd/D-Bus activation environment.
- Mode `0600`, removed on compositor exit.
- Protocol version: `1`.

`evawm msg` discovers the socket from `EVAWM_SOCKET` or `WAYLAND_DISPLAY`.

## Wire Protocol

Requests are one JSON object per line:

```json
{"id":1,"cmd":"version"}
{"id":2,"cmd":"outputs"}
{"id":3,"cmd":"workspaces"}
{"id":4,"cmd":"windows"}
{"id":5,"cmd":"focused"}
{"id":6,"cmd":"subscribe"}
{"id":7,"cmd":"action","action":"focus-workspace","args":[2]}
{"id":8,"cmd":"action","action":"quit","yes":true}
{"id":9,"cmd":"focus-window","args":[12]}
```

Replies:

```json
{"id":1,"ok":true,"data":{...}}
{"id":1,"ok":false,"error":"..."}
```

Parse errors on malformed JSON omit `"id"` (`{"ok":false,"error":"..."}`).
`subscribe` acknowledgements include `"subscribed":true` alongside the initial
snapshot. Unknown commands, malformed JSON, and invalid arguments return an
error reply without affecting the running compositor.

## Queries & State Model

- `version` returns `{protocol, version, pid, socket}`.
- `outputs` returns a list of `{name, width, height, refresh_mhz, x, y, transform, active, active_workspace_id, active_workspace_index}`.
- `workspaces` returns a list of `{id, index, name, output, active, focused, mode, window_count, active_window_id}` (`name` is currently `null`).
- `windows` returns a list of `{id, title, app_id, output, workspace_id, workspace_index, focused, fullscreen, mode, geometry}`.
- `focused` returns the focused window object or `null`.
- **Stable IDs:** Windows and workspaces receive monotonic `u64` IDs that are
  never reused within a session. Workspace `index` is the live 1-based position
  on its output (dynamic workspaces grow/shrink automatically).
- **Workspace flags:** `active` is `true` for the visible workspace on *each*
  monitor; `focused` is `true` only for the active workspace on the currently
  focused monitor.
- **Window geometry:** `geometry` reports `{x, y, width, height}` in workspace
  coordinates (Scroll content rect inset by borders, Canvas world rect, or
  output bounds when fullscreen).
- X11 windows opened via `xwayland-satellite` appear identically to native
  Wayland windows.

## Actions

Every keybind action (including the aliases from [`config/config.kdl`](../config/config.kdl)
such as `focus-column-left/right`, `focus-window-up/down`, `move-column-left/right`,
`center-column`, `consume-or-expel-window-left/right`, `set-column-width`, and
`set-window-height`) is also available via IPC:

- `spawn "cmd" "arg"...` / `spawn-sh "shell command"`
- `close-window`
- `focus-left/right/up/down`, `focus-column-first/last`
- `move-left/right`, `move-window-left/right`, `move-up/down`
- `move-column-to-first/last`
- `focus-monitor-left/right/up/down`, `move-to-monitor-left/right/up/down`
- `resize-width "PERCENT"`, `resize-height "PERCENT"` (JSON string, e.g. `"+10%"`, `"-10%"`)
- `maximize-column`, `expand-column-to-available-width`, `center-focused`, `fullscreen-window`
- `focus-workspace N`, `move-window-to-workspace N`, `move-column-to-workspace N`
- `consume-or-expel "left"|"right"`
- `cycle-window "next"|"previous"` (`"prev"` also accepted)
- `toggle-canvas`
- `toggle-overview`, `open-overview`, `close-overview`
- `pan-camera "left"|"right"|"up"|"down" [amount]`
- `home-canvas`
- `reload-config`
- `power-off-monitors`
- `focus-window ID`, `focus-workspace-id ID`
- `quit` (requires `--yes` on CLI or `"yes": true` in JSON)

`power-off-monitors` clears DRM CRTCs; real input activity (key press, pointer
motion/button/axis, touch/tablet/gesture) wakes monitors back up. Key releases
and device hotplug do not wake monitors.

## Event Stream (`subscribe`)

On the raw socket, `subscribe` returns an acknowledgement
`{"id":...,"ok":true,"subscribed":true,"data":{...}}`, followed by one JSON line
per state change (workspace, focus, window, output, mode, fullscreen):

```json
{"event":"state","data":{"outputs":[...],"workspaces":[...],"windows":[...],"focused":null}}
```

`evawm msg subscribe --json` normalizes the initial snapshot into the same
`{"event":"state","data":{...}}` format so downstream scripts only parse one
line shape. Subscribers use bounded queues; slow or disconnected clients are
dropped and never block rendering.

## CLI Examples

```sh
evawm msg version
evawm msg outputs
evawm msg workspaces --json
evawm msg windows --json
evawm msg focused
evawm msg action focus-workspace 2
evawm msg action toggle-overview
evawm msg action power-off-monitors
evawm msg subscribe --json
```

## Status Bars (Yambar Example)

[`examples/yambar-workspaces.sh`](../examples/yambar-workspaces.sh) adapts
`evawm msg subscribe --json` into yambar `script` tags (requires `jq`):

```sh
examples/yambar-workspaces.sh DP-1
```

It emits tags for all live dynamic workspaces (up to 9), including `active`,
`focused`, `id`, and `windows` counts. Use `evawm msg action focus-workspace-id <id>`
for bar click handlers so clicks target that workspace's own output even when
another monitor is focused.
