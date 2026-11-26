# EvaWM Config Reference

EvaWM uses KDL v1 parsed with `knuffel` 3.2. The embedded defaults live in
[`config/config.kdl`](../config/config.kdl); user configuration normally lives
at `~/.config/evawm/config.kdl`.

## Syntax Notes

- Booleans are `true` / `false` (not `#true` / `#false`).
- String arguments must be quoted: `cycle-window "next"`, `pan-camera "left"`.
- Nodes inside a block can end with a newline or `;`.
- Unknown sections and options warn and continue by default. Add `strict true`
  at the top level of a file to make them fatal for that file.

## Search Order

1. `--config <path>` / `-c <path>`
2. `$XDG_CONFIG_HOME/evawm/config.kdl`
3. `~/.config/evawm/config.kdl`
4. Embedded default (`config/config.kdl`)

## Top-Level Directives

```kdl
strict true
include "theme.kdl"
include "local.kdl"
```

- `strict` is **per file**: an include can opt into strict parsing without
  changing how the including file is parsed.
- `include` paths are relative to the file containing the directive. Includes
  are applied in order; later values win. Missing includes warn unless the
  including file is strict.
- The config watcher polls the main file and all included files every 500 ms
  and reloads automatically on save.

## Sections

### `startup`

```kdl
startup {
    spawn "swaybg" "-i" "/path/to/wallpaper.png" "-m" "fill"
    spawn-sh "wl-paste --type text --watch cliphist store"
}
```

`spawn` takes a program and arguments; `spawn-sh` runs a shell command via
`sh -c`. Multiple `startup` sections append. Startup commands run only
once when EvaWM starts, never on reload.

### `input`

```kdl
input {
    keyboard {
        xkb {
            rules ""
            model ""
            layout ""
            variant ""
            options ""
        }
        repeat-delay 250
        repeat-rate 25
    }
    cursor { theme ""; size 24; }
    mouse {
        accel-profile "flat"
        accel-speed 0.0
        natural-scroll false
        left-handed false
        middle-emulation false
        pan-button "none"
        mod-button "none"
    }
    touchpad {
        tap true
        natural-scroll true
        left-handed false
        accel-speed 0.0
        // accel-profile "flat"   // omitted by default (uses libinput device default)
    }
    focus-follows-mouse { scroll true; canvas true; }
}
```

- `xkb` sets the xkbcommon keymap (`rules`, `model`, `layout`, `variant`,
  `options`). Empty fields fall back to system defaults / `XKB_DEFAULT_*`
  environment variables, and changes reload live.
- `accel-profile` is `"flat"` or `"adaptive"`. `mouse` defaults to `"flat"`;
  `touchpad` defaults to the libinput device default (usually `"adaptive"`).
- `pan-button` and `mod-button` accept `"none"` (default), `"back"`
  (`BTN_SIDE` / mouse4), `"forward"` (`BTN_EXTRA` / mouse5), or `"middle"`.
  Leaving them `"none"` keeps browser back/forward buttons free for apps;
  `Mod+LMB` (move), `Mod+RMB` (resize), and `Mod+Shift+LMB` (canvas pan)
  always work.
- Presence flags can be written bare (`tap`) or with an explicit boolean
  (`tap false`).

### `xwayland`

```kdl
xwayland {
    enabled true
}
```

`enabled` defaults to `true`. EvaWM pre-binds the X11 sockets and exports
`DISPLAY` at startup, then launches `xwayland-satellite` on demand when the
first X11 client connects. If `xwayland-satellite` is missing or lacks
`-listenfd` support, EvaWM warns and continues without X11. Live reload can
enable or disable XWayland; disabling it stops the satellite and closes X11
clients.

### `layout`

```kdl
layout {
    gaps 8
    inner-border {
        width 3
        active-color "#ffffff"
        inactive-color "#505050"
    }
    outer-border {
        width 3
        active-color "#b4b4b4"
        inactive-color "#2a2a2a"
    }
    background-color "#000000"
    default-mode "scroll"
    default-column-width { proportion 0.5; }
    canvas {
        pan-step 200
        move-step 40
        new-window-position "center"
        new-window-width 0.55
        new-window-height 0.60
    }
    overview {
        zoom 0.5
    }
}
```

- Borders are drawn as four-strip rings around an open interior so
  semi-transparent windows show the wallpaper or underlying windows cleanly.
  Set `width 0` on either ring to disable it.
- `default-mode` is `"scroll"` (default) or `"canvas"` for newly created
  workspaces.
- `canvas.new-window-position` controls where new canvas windows open:
  - `"center"` (default) — centered in the current view.
  - `"cursor-center"` (also accepts `"cursor"`) — centered under the pointer,
    clamped to the visible working area.
  - `"cursor-top-left"` — top-left corner at the pointer, clamped to the
    visible working area.
- `overview.zoom` sets the Overview scale factor (`0.1..=0.95`, default `0.5`).
- Workspaces are dynamic per output (up to 9): opening or moving a window into
  the last workspace appends a new trailing empty workspace, and empty
  workspaces are removed when left.

### `animations`

```kdl
animations {
    // Disable every animation globally:
    // off
    slowdown 1.0
    view-movement { spring damping-ratio=0.9 stiffness=900 epsilon=0.0007 }
    window-movement { spring damping-ratio=0.82 stiffness=900 epsilon=0.0007 }
    window-resize { spring damping-ratio=1.0 stiffness=800 epsilon=0.0005 }
    window-open {
        spring damping-ratio=0.72 stiffness=800 epsilon=0.001
        scale-from 0.9
        opacity-from 0.0
    }
    window-close {
        duration-ms 120
        curve "ease-out-quad"
        scale-to 0.92
    }
    overview { spring damping-ratio=0.9 stiffness=900 epsilon=0.0007 }
}
```

Every individual animation block also accepts `off` (or `off true` / `off false`)
to disable just that animation while keeping others enabled.

Each animation uses either a spring (`damping-ratio`, `stiffness`, `epsilon`)
or an easing (`duration-ms`, `curve`). Supported curves: `linear`,
`ease-in-out`, `ease-out-quad`, `ease-out-cubic`, `ease-out-expo`,
`ease-out-back`, `ease-in-out-back`, and `cubic-bezier x1 y1 x2 y2`.

### `output`

```kdl
output "DP-1" {
    mode "1920x1080@165"
    transform "normal"
    position x=0 y=0
    // off
}
```

Multiple blocks with the same output name merge by field. `mode` accepts
`WIDTHxHEIGHT` with an optional `@REFRESH`. Transforms are `normal`, `90`,
`180`, `270`, `flipped`, `flipped-90`, `flipped-180`, or `flipped-270`.

Output overrides apply at session startup; restart the session after editing
them.

### `binds`

```kdl
binds {
    Mod+Return { spawn "foot"; }
    Mod+Q { close-window; }
    Mod+B { toggle-canvas; }
    Mod+O { toggle-overview; }
    Mod+WheelScrollDown { open-overview; }
    Mod+WheelScrollUp { close-overview; }
}
```

Defining a `binds` block replaces any previously defined bind set wholesale
(whether from embedded defaults or an earlier include). Modifier names are
`Mod` (alias `Super`, `Logo`, `Win` — mapped to Alt under `--nested-mod alt`),
`Ctrl` (`Control`), `Alt`, and `Shift`. Wheel triggers (`WheelScrollUp`,
`WheelScrollDown`, `WheelScrollLeft`, `WheelScrollRight`) work with modifiers
just like keys.

See [`config/config.kdl`](../config/config.kdl) for the default bind set.
Additional actions/aliases also supported in `binds`:

- `spawn-sh "shell command"`, `move-column-to-workspace <1..=9>`,
  `power-off-monitors`, `reload-config`, `open-overview`, `close-overview`
- Optional pixel amount on `pan-camera "left"|"right"|"up"|"down" [amount]`
- Short/niri aliases: `focus-left`/`right`/`up`/`down`,
  `move-left`/`right`/`up`/`down`, `center-focused`, `set-column-width`,
  `set-window-height`, `consume-or-expel "left"|"right"`, `cycle-window "prev"`

## Live Reload

Saving the main config or any included file reloads automatically (within ~500 ms).
You can also trigger a reload manually with `reload-config` or
`evawm msg action reload-config`. Parse errors keep the previous config running.

| Change | Applies live? |
| --- | --- |
| `binds`, `animations`, `input` (including `xkb`), `cursor` | yes |
| `layout` gaps, borders, background, `default-column-width`, overview zoom | yes |
| `layout.canvas.pan-step`, `layout.canvas.move-step` | yes |
| `layout.default-mode` | new workspaces only |
| `layout.canvas.new-window-*` | new canvas windows only |
| `xwayland.enabled` | yes |
| `output` mode/position/transform/off | restart required |
| `startup` commands | first startup only |

## Overlay Rules

- `startup` sections append.
- `output` blocks merge by output name.
- `binds` replaces the entire previous bind set.
- All other sections merge field-by-field onto the embedded defaults.
- Includes apply in order; later values win.
