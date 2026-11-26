# EvaWM Architecture & Design

EvaWM is a Wayland compositor built on [Smithay](https://github.com/Smithay/smithay)
that combines niri-style scrolling columns with a **per-workspace infinite 2D
canvas**.

## 1. Core Model

- **Per-output dynamic workspaces:** Each monitor owns its own dynamic workspace
  list (up to 9). Opening or moving a window into the last workspace appends a
  new empty trailing workspace; empty workspaces are removed when left.
- **Per-workspace mode (`Scroll | Canvas`):** Any workspace can toggle between
  scrolling columns and a free 2D canvas without closing, recreating, or
  reparenting windows. Workspace 1 can be Canvas while Workspace 2 stays Scroll.
- **Per-mode layout, shared identity:** Every window exists once. Scroll keeps
  columns, widths, stacks, and column order; Canvas keeps independent world
  rectangles, z-order, and last camera position. Moving or resizing in one mode
  never corrupts the other.
- **Camera is the output's location in `Space`:** Panning moves the output
  viewport in Smithay's `Space<Window>` rather than transforming window
  coordinates. Layer-shell surfaces (bars, wallpapers) render in screen space
  and never pan.
- **Pan-only camera (plus Overview):** Canvas panning has no continuous zoom.
  Overview (`toggle-overview`) zooms all outputs out uniformly to
  `layout.overview.zoom` while keeping wallpaper and layer-shell at `1.0`.

## 2. State Structure

```text
EvaWM
├── config: Config
├── backend: Option<Backend>          // Session (DRM/libseat) or Nested (winit)
├── outputs: Vec<OutputState>         // per-monitor Space, workspaces, camera, overview zoom
│   └── OutputState
│       ├── output: Output
│       ├── space: Space<Window>      // output mapped at +camera; windows at world coords
│       ├── workspaces: Vec<Workspace>
│       │   └── Workspace
│       │       ├── id: u64           // stable per-session IPC id
│       │       ├── mode: Mode        // Scroll | Canvas
│       │       ├── scroll: Layout    // columns, tiles, constraints
│       │       ├── canvas: CanvasLayout // world rects, z-order, last_camera
│       │       ├── focus: Option<Window>
│       │       └── fullscreen: Option<Window>
│       ├── active_workspace: usize
│       ├── view_x / view_y: AnimatedValue
│       └── overview_zoom: AnimatedValue
├── window_states: WindowStates       // open/close/resize/transition animations + border cache
├── drag: Option<Drag>                // move, resize, canvas pan
├── xwayland: Option<XwaylandServer>  // lazy xwayland-satellite sockets
└── ipc_server: Option<IpcServer>     // native JSON Unix socket
```

## 3. Scroll, Canvas, and Mode Switching

### Scroll Mode

- Horizontal strip of columns in world space (camera `y = 0`; tiles start at
  `working_area.loc.y + gap + inset`), with vertically stacked tiles inside
  columns.
- Column widths use proportional fractions (default `0.5` of working area) and
  respect client `min_size` / `max_size` constraints.
- Horizontal focus/move scrolls the camera just enough to keep the focused
  column visible; `center-column` centers it explicitly.
- Interactive pointer drags always detach a window into its own column
  (stacking is explicit via `consume-or-expel`).

### Canvas Mode

- Windows live at arbitrary `(x, y, w, h)` world coordinates with explicit
  z-order.
- New windows open at `new-window-width` × `new-window-height` of the working
  area, placed at view center (`"center"`), centered under the cursor
  (`"cursor-center"`), or top-left at the cursor (`"cursor-top-left"`), clamped
  to the visible working area.
- Camera pans freely in X and Y via `Mod+Alt+arrows`, `Mod+Shift+LMB` drag, or
  an optional configured `pan-button`.
- `maximize-column` fills the current viewport (preserving gaps and exclusive
  zones) without moving the camera, and restores the previous rectangle on a
  second press.

### Mode Switching (`Scroll ↔ Canvas`)

1. **Focus is shared:** The focused window stays focused across switches.
2. **Scroll → Canvas:** Windows that already have Canvas rectangles keep them.
   Windows newly created in Scroll are seeded as one preserved group (keeping
   their relative Scroll stacks, columns, and gaps) centered in the workspace's
   last Canvas view. The camera moves to center the focused window.
3. **Canvas → Scroll:** Windows newly created in Canvas already hold a Scroll
   slot; Scroll reasserts tile sizes and fits the camera to the focused column.
4. **Closed windows:** Removed from both modes and never resurrected on switch.

### Overview Mode

- Zooms all outputs out to `layout.overview.zoom` (default `0.5`) around each
  output's working-area center using `RescaleRenderElement`, keeping wallpaper
  and layer-shell bars at native `1.0`.
- Shows only the active workspace on each output and stays open across workspace
  switches until toggled off or a window/empty space is clicked.
- Pointer hit-testing and `Mod+LMB` / `Mod+RMB` / pan drags invert the zoom
  transform so windows can be moved and resized directly inside Overview.

### Fullscreen Overlay

- Fullscreen is a per-workspace screen-space overlay rendered above the `Top`
  layer (below `Overlay` popups/lockers) with borders hidden.
- Underlying tiled/canvas windows remain mapped underneath; leaving fullscreen
  restores exact layout and camera state.

## 4. Rendering, Borders, and Animations

- **Damage-driven rendering:** Outputs redraw only when `needs_redraw` is set.
  Idle desktops perform zero redraws and stay near 0% CPU.
- **Per-output frame pacing:** Each output ticks only its own camera and owned
  windows (`tick_animations_for_output`), paced by that output's VBlank (or
  winit redraw in nested mode). Mixed-refresh setups (e.g. 165 Hz + 75 Hz) run
  independently with no global animation timer.
- **Frame callbacks:** Delivered on presented frames, with a refresh-paced
  fallback only while a client has a pending callback and presentation stops.
- **Four-strip borders:** Inner and outer rings are each drawn as four
  non-overlapping `SolidColorBuffer` edge strips (`top`, `bottom`, `left`,
  `right`) around an open interior so semi-transparent windows display the
  wallpaper or windows behind them cleanly.
- **Analytical animations:** Springs (underdamped, critically damped, and
  overdamped real-exponential form) and easings evaluate against a monotonic
  virtual `Clock`. Opacity fades (open, close, fullscreen border) use
  `Animation::monotonic_progress` so bouncy springs never cause alpha flicker.
  Open and close animations composite the whole tile (content + borders) into a
  reusable offscreen `SnapshotBuffer`.
- **DRM presentation:** Uses implicit-modifier (`DRM_FORMAT_MOD_INVALID`)
  `XRGB8888` buffers (`FrameFlags::empty`) with explicit DMABUF implicit-sync
  pre-commit blockers and DRM `needs_sync()` waits for broad GPU/XWayland
  compatibility.

## 5. Protocols & Subsystems

Implemented Wayland protocols:
- `wl_compositor`, `wl_subcompositor`, `xdg-shell`, `wlr-layer-shell`, `xdg-decoration`
- `wl_shm`, `linux-dmabuf`
- `wl_seat`, `wl_output`, `xdg-output`, `wp-viewporter`
- `wp-presentation-time`, `wp-cursor-shape`, `zwp-text-input-v3`
- `wl_data_device` (clipboard + client-initiated Wayland DnD), `primary-selection`, `wlr-data-control`, `ext-data-control`
- `pointer-constraints`, `relative-pointer`, `keyboard-shortcuts-inhibit`
- `ext-idle-notify`, `idle-inhibit`, `xdg-activation`, `xdg-system-bell`
- `wlr-foreign-toplevel-management`, `ext-workspace`
- `wlr-screencopy` (deferred damage capture for screenshots and `xdg-desktop-portal-wlr`)
- `wlr-gamma-control` (for `gammastep` / night light)

Intentionally excluded:
- `ext-session-lock-v1`, `xdg-toplevel-icon`, blur/shadows/rounded corners,
  continuous zoom, floating rules, and output hotplug (trackpad gestures are a
  possible future feature).

XWayland is handled via on-demand `xwayland-satellite`: EvaWM pre-binds X11
sockets, exports `DISPLAY`, and spawns the satellite only when the first X11
client connects.

## 6. Module Layout

```text
src/
  main.rs              CLI args, nested backend entry, event loop
  session.rs           udev/DRM/libseat bare-metal backend
  session_env.rs       systemd/D-Bus session & portal environment management
  state/               EvaWM core, outputs, focus, fullscreen, canvas, overview, actions, IPC
  workspace.rs         Workspace, Mode, CanvasLayout, group seeding
  workspaces.rs        dynamic workspace transitions and cross-output moves
  layout/              Scroll column geometry, camera fit, movement, constraints
  render/              output assembly, window/border/layer render elements, damage
  borders.rs           cached four-strip inner/outer border rings
  animation.rs         analytical springs, easings, virtual clock
  window_state.rs      per-window animation, resize suppression, close snapshots
  snapshot.rs          reusable offscreen GLES tile snapshot buffer
  input.rs             keyboard, pointer, libinput device settings, mouse binds
  drag.rs              interactive move, resize, and canvas pan state
  dnd.rs               client-initiated Wayland drag-and-drop
  cursor.rs            xcursor theme loader + embedded fallback cursor
  gamma.rs             wlr-gamma-control DRM CRTC gamma ramps
  screencopy.rs        wlr-screencopy SHM capture
  xwayland.rs          lazy xwayland-satellite socket listener & launcher
  config/              KDL types, parser, bind/action decoding, tests
  config_watcher.rs    500 ms off-thread config & include polling watcher
  reload.rs            live config application
  ipc/                 Unix socket server, protocol types, `evawm msg` CLI
  handlers/            Smithay Wayland protocol handler implementations
  signals.rs           SIGTERM/SIGINT self-pipe handling for clean shutdown
  spawn.rs             shared pidfd child process reaper
```
