# EvaWM

A keyboard focused Wayland compositor built on [Smithay](https://github.com/Smithay/smithay)
that combines [niri](https://github.com/YaLTeR/niri) style scrolling with an infinite floating canvas.

> [!WARNING]
> EvaWM is experimental software built heavily with AI. It's just a personal project but it's here if others want to try it out.

## Features

- **Scroll & Canvas modes (`Mod+B`):** Toggle any workspace between
  scrolling and an infinite floating canvas.
- **Overview (`Mod+O`):** Zoom out across all monitors to view/move windows around.
- **Dynamic workspaces**
- **Multi-monitor support**
- **Borders & animations:** Configurable spring/easing animations and double borders.
- **KDL config & live reload:** Simple config file with `include` support and automatic live reloading.
- **On-demand XWayland:** Launches `xwayland-satellite` automatically only when
  an X11 app is opened.
- **JSON IPC (`evawm msg`):** Query state, run actions, and stream live events
  for status bars and scripts.
- **Desktop essentials:** Compatible with bars, wallpapers (`wlr-layer-shell`), screenshots & screen sharing (`wlr-screencopy`), night light (`wlr-gamma-control`), clipboard, and Wayland drag-and-drop.

**Not included:**
- Blur, shadows, rounded corners
- Continuous canvas zoom
- Trackpad gestures (possible future feature)
- Lockscreen protocol (`ext-session-lock-v1`, e.g. `hyprlock`/`swaylock`)
- Monitor hotplug or monitor scaling (scale is currently fixed at `1`)

## Quick Start

### Build dependencies

*(On Debian/Ubuntu/Fedora, install the corresponding `-dev` / `-devel` packages.)*

- Rust (1.88+), C compiler, `pkg-config`, and `make`
- `libseat`
- `libdrm`
- `libinput`
- `libudev`
- `libgbm` / Mesa (EGL & GLES)
- `libxkbcommon`
- `wayland` (`libwayland-server`)

Clone, build, and install:

```sh
git clone https://github.com/lunar-os/evawm.git
cd evawm
cargo build --release
sudo make install-session
```

Then log out and select **EvaWM** in your display manager (Ly, SDDM, GDM, etc.),
or launch it directly from a free TTY with `evawm --session`. (If your display
manager does not include `/usr/local/bin` in `PATH`, install with
`sudo make PREFIX=/usr install-session`.)

### Some Starter Keybinds

- `Mod+Return` — open terminal (`foot`)
- `Mod+A` — open launcher (`fuzzel`)
- `Mod+Q` — close focused window
- `Mod+B` — toggle Scroll / Canvas mode on current workspace
- `Mod+O` — toggle Overview
- `Mod+Shift+E` — quit EvaWM

<details>
<summary><b>More options (nested testing, session-only build, screen sharing)</b></summary>

### Testing nested inside another compositor (`--nested`)

```sh
cargo run --release -- --nested --nested-mod alt
```

(`--nested-mod alt` uses `Alt` as `Mod` so your host compositor doesn't steal
`Super`.)

### Session-only build without `winit`

If you don't need `--nested` mode, you can build without `winit` for a smaller
session-only binary (XWayland, screen sharing, animations, and all session
features remain enabled):

```sh
cargo build --release --no-default-features
```

### Screen sharing (OBS, Discord, browsers)

Install `xdg-desktop-portal-wlr` (**0.8.5+** — 0.8.4 freezes after the first
frame). `sudo make install-session` already installs the EvaWM portal config
system-wide (without affecting other desktops). If you didn't run
`install-session`, install it per-user with:

```sh
make install-portal-config
```

</details>

## Suggested Software

EvaWM stays somewhat minimal and relies on external software for the rest of the desktop such as:

- **Terminal:** `foot`
- **Launcher:** `fuzzel`
- **Wallpaper:** [wawa](https://codeberg.org/shrub900/wawa) or [swaybg](https://github.com/swaywm/swaybg)
- **Status bar:** any `wlr-layer-shell` bar — I personally use [yambar](https://codeberg.org/dnkl/yambar) with
  [`examples/yambar-workspaces.sh`](examples/yambar-workspaces.sh)
- **X11 apps:** `xwayland-satellite`
- **Night light:** `gammastep -m wayland`

## Config & IPC

EvaWM works out of the box with embedded defaults. To customize it, create
`~/.config/evawm/config.kdl` with only the options you want to change, or copy
[`config/config.kdl`](config/config.kdl) as a starting point (it is fully
commented and includes all default keybinds).

- [`config/config.kdl`](config/config.kdl) — embedded defaults and default keybinds.
- [`docs/config.md`](docs/config.md) — full KDL configuration reference and live-reload guide.
- [`docs/ipc.md`](docs/ipc.md) — `evawm msg` JSON IPC protocol and bar integration.
- [`docs/design.md`](docs/design.md) — architecture notes.

## Credits

- [Smithay](https://github.com/Smithay/smithay) — Wayland compositor framework and embedded fallback cursor (`anvil`, MIT).
- [niri](https://github.com/YaLTeR/niri) — scrolling columns, dynamic
  workspaces, Overview, spring math, and KDL config inspiration.
- [hevel](https://git.sr.ht/~dlm/hevel) & [vxwm](https://codeberg.org/wh1tepearl/vxwm) —
  infinite-canvas inspiration.
- [driftwm](https://github.com/malbiruk/driftwm) — `wlr-gamma-control` implementation adapted from `driftwm` (GPL-3.0-or-later, © Klim Kostiuk).

## License

GPL-3.0-or-later. See [`LICENSE`](LICENSE).
