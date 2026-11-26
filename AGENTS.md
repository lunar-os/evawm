# AGENTS.md

Instructions for AI coding agents working on EvaWM.

## Read First

1. `docs/design.md` — architecture, state model, Scroll/Canvas/Overview semantics, module map.
2. `docs/config.md` — KDL config reference and live-reload matrix.
3. `docs/ipc.md` — `evawm msg` protocol, actions, and events.
4. `config/config.kdl` — shipped default config (embedded in the binary).

## Project Summary

EvaWM is a Smithay Wayland compositor (Rust edition 2024, MSRV 1.88) with a
DRM/libseat bare-metal session and an optional nested winit backend (`nested`
Cargo feature, enabled by default). Each output has dynamic workspaces (up to 9),
and each workspace can toggle between niri-style scrolling columns (`Scroll`)
and a free 2D pannable world (`Canvas`), plus multi-output Overview (`Mod+O`),
on-demand `xwayland-satellite`, native JSON IPC (`evawm msg`), four-strip
transparent-safe borders, analytical spring/easing animations, `wlr-screencopy`,
and `wlr-gamma-control`.

## Code Style

- Write self-documenting code: clear names, obvious structure, minimal comments.
- No section-separator banner comments (`// ---- Foo ----` or `// ==== Bar ====`).
- Comments explain *why*, not *what* (Smithay quirks, Wayland lifecycle invariants, coordinate math, GPU sync workarounds).
- Brief `///` doc comments on public types/functions when the signature isn't self-explanatory.
- Always run `cargo fmt`, `cargo clippy --locked --all-targets -- -D warnings`, and `cargo test --locked` before finishing a change. For changes touching backend/feature code, also check `cargo clippy --locked --no-default-features --all-targets -- -D warnings`.

## Commands

```sh
cargo check --locked
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --no-default-features --all-targets -- -D warnings
cargo fmt --check
make check-all                           # fmt-check, clippy, test, release build
cargo run -- --nested --nested-mod alt   # nested testing inside a host compositor
cargo build --release --no-default-features  # session-only build (no winit/X11)
evawm msg outputs --json                 # IPC queries
evawm msg action focus-workspace 2       # IPC action
evawm msg subscribe --json               # live event stream
```

Use `--nested-mod alt` when testing inside a host compositor that owns Super.
`--session` is the DRM/libseat path; test it on a free TTY.

## Architecture Invariants

1. **Per-mode layout, shared window identity.** A window exists once, while
   Scroll and Canvas keep independent geometry, z-order, and camera. Switching
   preserves both states; never convert or discard the other mode.
2. **Camera is the output's position in `Space`.** Do not transform windows to pan.
3. **Layer-shell is screen space.** Do not pan or zoom bars or wallpapers.
4. **Per-output frame pacing.** Never drive animations from a global timer.
   `tick_animations_for_output` advances only that output's camera and owned windows.
5. **Damage-driven redraws.** Redraw only when `needs_redraw` is set; zero idle redraws.
6. **Mode switch preserves layout, syncs focus, follows focus with camera.**
   Closed windows are removed from both modes and never resurrected. Newly
   arrived Scroll windows seed as a preserved group centered in the last Canvas view.
7. **Frame callbacks are a clock, not damage.** Deliver them on presented
   frames, with a refresh-paced fallback only while a callback is pending.
8. **Monotonic opacity.** Never drive open, close, or fullscreen-border alpha
   from an overshooting spring value; use `Animation::monotonic_progress`.
9. **No hidden state in config.** Everything that affects behavior is in
   `config/config.kdl` or a documented default. Keep shipped defaults
   system-agnostic (no monitor names, refresh rates, machine paths, or includes).

## Hard Requirements & Scope

- Wayland compositor on Smithay (pinned rev in `Cargo.toml`), Rust edition 2024.
- Minimal dependencies; justify any new crate and prefer std/existing deps.
- No blur, rounded corners, shadows, focus rings, floating
  rules, continuous zoom, `ext-session-lock-v1`, `xdg-toplevel-icon`, or native
  X11 backend (keep on-demand `xwayland-satellite`). Trackpad gestures are not
  implemented yet (possible future feature).
- Keep DRM composition on implicit-modifier `XRGB8888` (`FrameFlags::empty`) unless a
  verified EINVAL fallback is added; keep DMABUF implicit-sync blockers and
  `needs_sync()` waits intact.
- Keep IPC and bar scripts decoupled from core compositor logic.

## Config, Input & IPC Rules

- Parser is `knuffel` 3.2 (KDL v1): booleans are `true`/`false`; quote string
  arguments; unknown options warn by default (`strict true` is per-file).
- Live reload polls the main file and every `include` every 500 ms and keeps
  the previous config on parse errors. Output overrides require a restart;
  `startup` runs only once.
- Shipped default HJKL uses standard vim directions (`H` left, `J` down, `K` up,
  `L` right). Mouse `pan-button` and `mod-button` default to `"none"` (valid:
  `"none"`, `"back"`, `"forward"`, `"middle"`).
- `Mod+LMB` move, `Mod+RMB` resize, `Mod+Shift+LMB` canvas pan. All three also
  work inside Overview scaled by `1 / zoom`.
- IPC socket: `$XDG_RUNTIME_DIR/evawm-$WAYLAND_DISPLAY.sock` (`0600`), exported
  as `EVAWM_SOCKET`. Protocol v1 newline-delimited JSON. Window and workspace
  IDs are monotonic and never reused. Subscribers use bounded queues and are
  dropped if slow; never allocate or log in render/input hot paths.

## Performance Budgets

- Idle CPU < 1% of one core; zero idle redraws.
- No allocations or string formatting in render/input hot paths.
- No per-event `tracing` at `info` level.
- Animations force redraws only while active; `animations { off }` (and
  per-animation `off`) must snap cleanly with zero extra frames.

## Reference Sources (Optional Local Checkouts)

If `../reference/` exists locally, treat it as read-only reference material (do
not modify or build it):

- `../reference/smithay/` — pinned revision matching `Cargo.toml`; verify Smithay APIs against the pinned source rather than memory.
- `../reference/niri/` — scrolling layout, workspaces, and animation patterns.
- `../reference/driftwm/` — canvas camera, pointer drag/resize, gamma control.
- `../reference/vxwm/` — infinite-canvas concepts.

This repository is `GPL-3.0-or-later`. Preserve attribution when adapting code.

## Testing Expectations

- Unit-test geometry, camera math, mode seeding, animations, borders, IPC state,
  and KDL config parsing (keep `shipped_config_parses` green).
- Nested smoke test: `cargo run -- --nested --nested-mod alt`, test Scroll &
  Canvas switching (`Alt+B`), Overview (`Alt+O`), move/resize (`Alt+LMB`/`RMB`),
  fullscreen (`Alt+Shift+Return`), and animations settling to 0% idle CPU.
- Bare-metal session smoke test on a spare TTY:
  `RUST_LOG=evawm=debug ./target/release/evawm --session`
  (`EVAWM_DRM_CONNECTOR` and `EVAWM_DRM_DEVICE` are available for debugging).

## When Unsure

Ask the user before changing per-mode persistence semantics, the default input
contract, dependencies, license, or adding out-of-scope features. Update
`docs/config.md`, `docs/ipc.md`, or `docs/design.md` in the same commit when
behavior changes.
