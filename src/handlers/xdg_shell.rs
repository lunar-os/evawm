use smithay::{
    delegate_xdg_decoration, delegate_xdg_shell,
    desktop::{
        PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab, PopupUngrabStrategy, Window,
        WindowSurfaceType, find_popup_root_surface, get_popup_toplevel_coords,
        layer_map_for_output,
    },
    input::pointer::Focus,
    reexports::{
        wayland_protocols::xdg::{
            decoration::zv1::server::zxdg_toplevel_decoration_v1, shell::server::xdg_toplevel,
        },
        wayland_server::{
            Resource, protocol::wl_output, protocol::wl_seat, protocol::wl_surface::WlSurface,
        },
    },
    utils::{Logical, Rectangle, SERIAL_COUNTER, Serial, Size},
    wayland::{
        compositor::{BufferAssignment, SurfaceAttributes, add_pre_commit_hook, with_states},
        seat::WaylandFocus,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            decoration::XdgDecorationHandler,
        },
    },
};

use crate::{
    config::NewWindowPosition,
    layout::constraints::clamp_client_size,
    state::{EvaWM, UnmappedToplevel},
};

impl XdgShellHandler for EvaWM {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        tracing::debug!("new unmapped toplevel");
        let wl_surface = surface.wl_surface().clone();
        let window = Window::new_wayland_window(surface);
        // Capture the last tile when the client unmaps by committing a null
        // buffer. `toplevel_destroyed` (or the unmap handler) runs afterwards,
        // once the surface contents are no longer renderable.
        add_pre_commit_hook::<EvaWM, _>(&wl_surface, |state, _display, surface| {
            let removed = with_states(surface, |states| {
                let mut guard = states.cached_state.get::<SurfaceAttributes>();
                matches!(
                    guard.pending().buffer.as_ref(),
                    Some(BufferAssignment::Removed)
                )
            });
            if !removed || state.config.animations.off || state.config.animations.window_close.off {
                return;
            }
            if let Some((output_index, window)) = state.window_and_output_for_surface(surface)
                && state
                    .workspace_for_window(&window)
                    .is_some_and(|(out, ws)| {
                        out == state.active_output
                            && out == output_index
                            && ws == state.outputs[out].active_workspace
                    })
                && let Some(element) = state.snapshot_window(output_index, &window)
            {
                let camera = state.outputs[output_index].camera_location();
                let rect = smithay::utils::Rectangle::new(
                    element.rect().loc + camera,
                    element.rect().size,
                );
                state
                    .window_states
                    .store_unmap_snapshot(&window, element, rect);
            }
        });
        self.unmapped_windows
            .insert(wl_surface, UnmappedToplevel::new(window));
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if self.remove_unmapped_toplevel(surface.wl_surface()) {
            return;
        }

        self.remove_mapped_toplevel(surface.wl_surface(), false);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if self
            .unmapped_windows
            .get(surface.wl_surface())
            .is_some_and(|unmapped| !unmapped.configured)
        {
            return;
        }
        surface.send_configure();
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        if self
            .unmapped_windows
            .get(surface.wl_surface())
            .is_some_and(|unmapped| !unmapped.configured)
        {
            return;
        }
        surface.send_configure();
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        output: Option<wl_output::WlOutput>,
    ) {
        let requested_output = output.and_then(|wl_output| {
            self.outputs
                .iter()
                .position(|output| output.output.owns(&wl_output))
        });

        if let Some(unmapped) = self.unmapped_windows.get_mut(surface.wl_surface()) {
            unmapped.wants_fullscreen = Some(requested_output);
            if unmapped.configured {
                let window = unmapped.window.clone();
                self.configure_unmapped_toplevel(&window, Some(requested_output));
            }
            return;
        }

        let Some((_, window)) = self.window_and_output_for_surface(surface.wl_surface()) else {
            surface.send_configure();
            return;
        };
        self.set_window_fullscreen_from_client(&window, true);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        if let Some(unmapped) = self.unmapped_windows.get_mut(surface.wl_surface()) {
            unmapped.wants_fullscreen = None;
            if unmapped.configured {
                let window = unmapped.window.clone();
                self.configure_unmapped_toplevel(&window, None);
            }
            return;
        }

        let Some((_, window)) = self.window_and_output_for_surface(surface.wl_surface()) else {
            surface.send_configure();
            return;
        };
        self.set_window_fullscreen_from_client(&window, false);
    }

    fn move_request(&mut self, surface: ToplevelSurface, _seat: wl_seat::WlSeat, serial: Serial) {
        if self.drag.is_some() || self.dnd_active || self.overview_open {
            return;
        }
        if !self.pointer.has_grab(serial) {
            return;
        }
        let Some(start_data) = self.pointer.grab_start_data() else {
            return;
        };
        if !start_data
            .focus
            .as_ref()
            .is_some_and(|(focus, _)| focus.same_client_as(&surface.wl_surface().id()))
        {
            return;
        }
        let Some(window) = self
            .space()
            .elements()
            .find(|w| w.toplevel().map(|t| t.wl_surface()) == Some(surface.wl_surface()))
            .cloned()
        else {
            return;
        };

        self.focus_window(Some(&window), serial);
        self.begin_window_move(&window, start_data.button);
        self.request_redraw();
    }

    fn resize_request(
        &mut self,
        _surface: ToplevelSurface,
        _seat: wl_seat::WlSeat,
        _serial: Serial,
        _edges: xdg_toplevel::ResizeEdge,
    ) {
        // Client-initiated CSD edge resize is intentionally unsupported;
        // Mod+RMB (or the configured side-button mod) is the resize path.
    }

    fn grab(&mut self, surface: PopupSurface, _seat: wl_seat::WlSeat, serial: Serial) {
        let kind = PopupKind::Xdg(surface);

        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };

        // Popup grabs must follow input on the focused window (including a
        // parent popup belonging to that same window for nested submenus), or
        // on a layer surface such as a bar menu. A grab from a hidden window
        // must not steal keyboard focus.
        let current_focus = self.keyboard.current_focus();
        let root_is_focus = current_focus.as_ref().is_some_and(|focused| {
            focused == &root
                || self
                    .popups
                    .find_popup(focused)
                    .and_then(|popup| find_popup_root_surface(&popup).ok())
                    .as_ref()
                    == Some(&root)
        });
        let layer = self.outputs.iter().find_map(|output| {
            layer_map_for_output(&output.output)
                .layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)
                .cloned()
        });
        if !root_is_focus && layer.is_none() {
            let _ = PopupManager::dismiss_popup(&root, &kind);
            return;
        }

        let seat = self.seat.clone();
        let Ok(mut grab) = self.popups.grab_popup(root, kind, &seat, serial) else {
            return;
        };

        let keyboard = self.keyboard.clone();
        let pointer = self.pointer.clone();
        let keyboard_grab = root_is_focus
            || layer
                .as_ref()
                .is_some_and(|layer| layer.can_receive_keyboard_focus());

        let keyboard_mismatch = keyboard_grab
            && keyboard.is_grabbed()
            && !(keyboard.has_grab(serial)
                || grab
                    .previous_serial()
                    .is_some_and(|previous| keyboard.has_grab(previous)));
        let pointer_mismatch = pointer.is_grabbed()
            && !(pointer.has_grab(serial)
                || grab
                    .previous_serial()
                    .is_some_and(|previous| pointer.has_grab(previous)));
        if keyboard_mismatch || pointer_mismatch {
            grab.ungrab(PopupUngrabStrategy::All);
            return;
        }

        if keyboard_grab {
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        self.popup_grab = Some(grab);
    }
}

impl XdgDecorationHandler for EvaWM {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        set_server_side_decoration(&toplevel);
    }

    fn request_mode(
        &mut self,
        toplevel: ToplevelSurface,
        _mode: zxdg_toplevel_decoration_v1::Mode,
    ) {
        // Tiling compositor: force server-side so CSD shadows, rounded
        // corners, and titlebars do not eat into the tile geometry.
        set_server_side_decoration(&toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        set_server_side_decoration(&toplevel);
    }
}

fn set_server_side_decoration(toplevel: &ToplevelSurface) {
    toplevel.with_pending_state(|state| {
        state.decoration_mode = Some(zxdg_toplevel_decoration_v1::Mode::ServerSide);
        if !state.states.contains(xdg_toplevel::State::Fullscreen) {
            state.states.set(xdg_toplevel::State::TiledLeft);
            state.states.set(xdg_toplevel::State::TiledRight);
            state.states.set(xdg_toplevel::State::TiledTop);
            state.states.set(xdg_toplevel::State::TiledBottom);
        }
    });
    if toplevel.is_initial_configure_sent() {
        toplevel.send_pending_configure();
    }
}

delegate_xdg_decoration!(EvaWM);
delegate_xdg_shell!(EvaWM);

/// Called from the compositor commit handler for popup surfaces.
pub fn handle_popup_commit(popups: &mut PopupManager, surface: &WlSurface) {
    popups.commit(surface);
    if let Some(popup) = popups.find_popup(surface) {
        match popup {
            PopupKind::Xdg(ref xdg) => {
                if !xdg.is_initial_configure_sent()
                    && let Err(err) = xdg.send_configure()
                {
                    tracing::warn!(?err, "failed to send initial popup configure");
                }
            }
            PopupKind::InputMethod(ref _input_method) => {}
        }
    }
}

impl EvaWM {
    pub(crate) fn remove_unmapped_toplevel(&mut self, surface: &WlSurface) -> bool {
        let Some(unmapped) = self.unmapped_windows.remove(surface) else {
            return false;
        };
        self.forget_dragged_window(&unmapped.window);
        self.forget_window_id(&unmapped.window);
        if !self.window_states.is_closing(&unmapped.window) {
            self.window_states.remove(&unmapped.window);
        }
        true
    }

    fn canvas_initial_size(
        &self,
        working_area: Rectangle<f64, Logical>,
        min_size: Size<i32, Logical>,
        max_size: Size<i32, Logical>,
    ) -> Size<i32, Logical> {
        let requested = Size::<i32, Logical>::from((
            (working_area.size.w * self.config.canvas.new_window_width)
                .round()
                .max(1.0) as i32,
            (working_area.size.h * self.config.canvas.new_window_height)
                .round()
                .max(1.0) as i32,
        ));
        clamp_client_size(requested, min_size, max_size, false)
    }

    pub(crate) fn send_initial_configure_if_needed(&mut self, surface: &WlSurface) {
        let Some(unmapped) = self.unmapped_windows.get_mut(surface) else {
            return;
        };
        if unmapped.configured {
            return;
        }
        unmapped.configured = true;
        let window = unmapped.window.clone();
        let wants_fullscreen = unmapped.wants_fullscreen;
        self.configure_unmapped_toplevel(&window, wants_fullscreen);
    }

    fn configure_unmapped_toplevel(
        &self,
        window: &Window,
        wants_fullscreen: Option<Option<usize>>,
    ) {
        let Some(toplevel) = window.toplevel().cloned() else {
            return;
        };
        let target_output = wants_fullscreen
            .flatten()
            .filter(|&index| index < self.outputs.len())
            .unwrap_or(self.active_output);
        let fullscreen = wants_fullscreen.is_some();
        let size = if fullscreen {
            self.fullscreen_size_on_output(target_output)
        } else {
            let (min_size, max_size) = Self::window_constraints(window);
            let output = &self.outputs[target_output];
            let workspace = &output.workspaces[output.active_workspace];
            if workspace.mode == crate::workspace::Mode::Canvas {
                self.canvas_initial_size(workspace.scroll.working_area, min_size, max_size)
            } else {
                workspace.scroll.initial_content_size(min_size, max_size)
            }
        };

        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(zxdg_toplevel_decoration_v1::Mode::ServerSide);
            state.size = Some(size);
            if target_output == self.active_output {
                state.states.set(xdg_toplevel::State::Activated);
            }
            if fullscreen {
                state.states.set(xdg_toplevel::State::Fullscreen);
                state.states.unset(xdg_toplevel::State::TiledLeft);
                state.states.unset(xdg_toplevel::State::TiledRight);
                state.states.unset(xdg_toplevel::State::TiledTop);
                state.states.unset(xdg_toplevel::State::TiledBottom);
            } else {
                state.states.unset(xdg_toplevel::State::Fullscreen);
                state.states.set(xdg_toplevel::State::TiledLeft);
                state.states.set(xdg_toplevel::State::TiledRight);
                state.states.set(xdg_toplevel::State::TiledTop);
                state.states.set(xdg_toplevel::State::TiledBottom);
            }
        });
        toplevel.send_configure();
    }

    pub(crate) fn map_toplevel_window(
        &mut self,
        window: Window,
        wants_fullscreen: Option<Option<usize>>,
    ) {
        // Clear any leftover close animation if this toplevel was unmapped and
        // quickly remapped.
        self.window_states.remove(&window);

        let target_output = wants_fullscreen
            .flatten()
            .filter(|&index| index < self.outputs.len())
            .unwrap_or(self.active_output);
        let index = self.outputs[target_output].active_workspace;

        // A new window entering the workspace replaces the fullscreen overlay
        // instead of opening hidden underneath it.
        if self.outputs[target_output].workspaces[index].has_fullscreen() {
            self.leave_fullscreen_for_action_on_output(target_output);
        }

        self.ensure_window_id(&window);
        let now = self.animation_now();
        let mode = self.outputs[target_output].workspaces[index].mode;
        let before = (target_output == self.active_output
            && mode == crate::workspace::Mode::Scroll
            && wants_fullscreen.is_none())
        .then(|| self.capture_content_world_rects(now));
        let new_window_position = self.config.canvas.new_window_position;
        let cursor_local = (matches!(
            new_window_position,
            NewWindowPosition::CursorCenter | NewWindowPosition::CursorTopLeft
        ) && self.pointer_output == target_output)
            .then_some(self.pointer_pos);

        let move_drag_active = self.move_drag_active();
        let mut new_canvas_size = None;
        {
            let (min_size, max_size) = Self::window_constraints(&window);
            let area = self.outputs[target_output].workspaces[index]
                .scroll
                .working_area;
            let clamped = (mode == crate::workspace::Mode::Canvas)
                .then(|| self.canvas_initial_size(area, min_size, max_size));

            let output = &mut self.outputs[target_output];
            output.workspaces[index].scroll.add_window(window.clone());
            output.workspaces[index]
                .scroll
                .update_window_constraints(&window, min_size, max_size);
            if let Some(clamped) = clamped {
                // Size the new canvas window from the configured working-area
                // ratios and place it in the current view. Do not apply the
                // scroll layout here: that would reconfigure every existing
                // canvas window to its scroll size and break their frames.
                let size = Size::<f64, Logical>::from((f64::from(clamped.w), f64::from(clamped.h)));
                let camera =
                    smithay::utils::Point::from((output.view_x.value, output.view_y.value));
                let cursor = cursor_local.map(|local| camera + local);
                let rect = canvas_new_window_rect(area, camera, size, new_window_position, cursor);
                output.workspaces[index].canvas.insert(window.clone(), rect);
                if wants_fullscreen.is_none() {
                    new_canvas_size = Some(clamped);
                }
                if !move_drag_active {
                    output.workspaces[index].apply(&mut output.space);
                }
            }
        }
        if let Some(clamped) = new_canvas_size {
            self.configure_canvas_window(&window, clamped);
        }
        self.outputs[target_output].ensure_trailing_workspace();
        self.rebuild_window_locations();

        if wants_fullscreen.is_some() {
            self.set_fullscreen_window_on_output(target_output, Some(window.clone()), false);
        } else {
            if mode == crate::workspace::Mode::Scroll {
                self.apply_layout_output(target_output);
                if let Some(before) = before {
                    self.start_pending_layout_transitions(&before, now, true);
                }
            }
            if target_output == self.active_output {
                let serial = SERIAL_COUNTER.next_serial();
                self.focus_window(Some(&window), serial);
            } else {
                self.notify_ipc();
            }
        }
        tracing::debug!(
            output = target_output,
            geometry = ?window.geometry(),
            fullscreen = wants_fullscreen.is_some(),
            "toplevel mapped into layout"
        );
        self.on_window_commit(&window);
        self.request_redraw_output(target_output);
    }

    /// Remove a mapped toplevel when the client commits a null buffer, moving
    /// it back to `unmapped_windows` so it can cleanly perform a fresh initial
    /// configure handshake if it remaps later.
    pub(crate) fn unmap_mapped_toplevel(&mut self, window: Window) {
        let Some(surface) = window.toplevel().map(|t| t.wl_surface().clone()) else {
            return;
        };
        tracing::debug!("toplevel unmapped via null buffer");
        self.remove_mapped_toplevel(&surface, true);
        self.unmapped_windows
            .insert(surface, UnmappedToplevel::new(window));
    }

    fn remove_mapped_toplevel(&mut self, surface: &WlSurface, keep_unmapped: bool) {
        let found = self
            .outputs
            .iter()
            .enumerate()
            .find_map(|(output_index, output)| {
                output
                    .workspaces
                    .iter()
                    .enumerate()
                    .find_map(|(workspace, state)| {
                        state
                            .window_for_surface(surface)
                            .map(|window| (output_index, workspace, window.clone()))
                    })
            });

        let Some((output_index, workspace, window)) = found else {
            return;
        };

        // A client can disappear while it is being dragged. Drop the drag
        // first; otherwise later pointer motion or the button release can
        // remap/insert the dead window and keep its client buffers alive.
        let was_dragged = self.forget_dragged_window(&window);
        if !keep_unmapped {
            self.forget_window_id(&window);
        }

        // A close can remove/shift workspaces and columns. Do not leave a
        // drag holding an index into the old layout; cancel any drag on a
        // different window before cleanup.
        if self.drag.is_some() {
            self.cancel_interaction_state();
        }

        let active = output_index == self.active_output
            && workspace == self.outputs[output_index].active_workspace;
        let mode = self.outputs[output_index].workspaces[workspace].mode;
        self.remove_mru(&window);
        let now = self.animation_now();

        // Capture the tile before removing it from the layout. This works for
        // both Mod+Q and the client's X button, because the surface is still
        // available inside toplevel_destroyed. A window that vanished mid-drag
        // has no meaningful resting tile, so skip the close animation.
        self.window_states.settle_transitions(&window);
        let animate_close = active
            && !was_dragged
            && !self.config.animations.off
            && !self.config.animations.window_close.off;
        let (mut snapshot, mut from) = if !animate_close {
            // Drop any stored unmap snapshot without rendering a new GPU texture.
            let _ = self.window_states.take_unmap_snapshot(&window);
            (None, None)
        } else if let Some((element, rect)) = self.window_states.take_unmap_snapshot(&window) {
            (Some(element), Some(rect))
        } else {
            let snapshot = self.snapshot_window(output_index, &window);
            let from = snapshot.as_ref().map(|snapshot| {
                let camera = self.outputs[output_index].camera_location();
                smithay::utils::Rectangle::new(snapshot.rect().loc + camera, snapshot.rect().size)
            });
            (snapshot, from)
        };

        let workspace_id = self.outputs[output_index].workspaces[workspace].id;
        let before = (active && mode == crate::workspace::Mode::Scroll)
            .then(|| self.capture_content_world_rects(now));
        // Remove the window from Space immediately. The close animation renders
        // the offscreen snapshot; keeping an unmapped/dead Window mapped only
        // retains stale state and lets teardown races reach the renderer.
        self.outputs[output_index].space.unmap_elem(&window);
        self.outputs[output_index].workspaces[workspace].remove_window(&window);
        self.outputs[output_index].clean_up_workspaces();
        self.rebuild_window_locations();
        let workspace_removed = !self.outputs[output_index]
            .workspaces
            .iter()
            .any(|workspace| workspace.id == workspace_id);
        if workspace_removed {
            // The workspace was an empty inactive one; there is nothing on
            // screen to animate.
            snapshot = None;
            from = None;
            self.notify_ipc();
        }

        if active {
            self.active_output = output_index;
            self.apply_layout();
            // Reflow stacks and columns with the movement spring. Canvas
            // windows are free-floating, so there is no scroll reflow.
            if let Some(before) = before {
                self.start_pending_layout_transitions(&before, now, true);
            }
            let serial = SERIAL_COUNTER.next_serial();
            let active_workspace = self.outputs[output_index].active_workspace;
            let next = self.outputs[output_index].workspaces[active_workspace]
                .focused_window()
                .cloned();
            self.focus_window(next.as_ref(), serial);
        } else if !workspace_removed {
            if workspace == self.outputs[output_index].active_workspace {
                self.apply_layout_output(output_index);
            }
            self.notify_ipc();
        }

        if active && let (Some(snapshot), Some(from)) = (snapshot, from) {
            let config = self.config.animations.window_close;
            let to =
                crate::state::close::shrink_rect(from, self.config.animations.window_close_scale);
            if self.window_states.start_close_snapshot(
                &window,
                snapshot,
                from,
                to,
                output_index,
                workspace_id,
                now,
                config,
            ) {
                self.request_redraw_output(output_index);
                return;
            }
        }
        self.remove_window_animation(&window);
        self.request_redraw_output(output_index);
    }

    pub(crate) fn unconstrain_popup(&self, popup: &PopupSurface) {
        let kind = PopupKind::Xdg(popup.clone());
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };

        let target = self.outputs.iter().find_map(|output| {
            let output_geo = output.space.output_geometry(&output.output)?;
            let workspace = &output.workspaces[output.active_workspace];
            if let Some(fullscreen) = workspace.fullscreen_window()
                && fullscreen.wl_surface().as_deref() == Some(&root)
            {
                return Some(Rectangle::from_size(
                    workspace.scroll.output_size.to_i32_round(),
                ));
            }
            if let Some(window) = output
                .space
                .elements()
                .find(|w| w.toplevel().map(|t| t.wl_surface()) == Some(&root))
                && let Some(window_geo) = output.space.element_geometry(window)
            {
                let mut rect = Rectangle::new(output_geo.loc, output_geo.size);
                rect.loc -= window_geo.loc;
                return Some(rect);
            }
            let map = layer_map_for_output(&output.output);
            let layer = map.layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)?;
            let layer_geo = map.layer_geometry(layer)?;
            let mut rect = Rectangle::from_size(output_geo.size);
            rect.loc -= layer_geo.loc;
            Some(rect)
        });
        let Some(mut target) = target else {
            return;
        };
        target.loc -= get_popup_toplevel_coords(&kind);

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}

/// Place a new canvas window in the visible working area, optionally centered
/// under the cursor. Clamps to the view so it cannot open off-screen.
fn canvas_new_window_rect(
    area: smithay::utils::Rectangle<f64, Logical>,
    camera: smithay::utils::Point<f64, Logical>,
    size: smithay::utils::Size<f64, Logical>,
    position: NewWindowPosition,
    cursor: Option<smithay::utils::Point<f64, Logical>>,
) -> smithay::utils::Rectangle<f64, Logical> {
    let visible = smithay::utils::Rectangle::new(camera + area.loc, area.size);
    let mut loc = match (position, cursor) {
        (NewWindowPosition::CursorCenter, Some(cursor)) => {
            cursor - smithay::utils::Point::from((size.w / 2.0, size.h / 2.0))
        }
        (NewWindowPosition::CursorTopLeft, Some(cursor)) => cursor,
        _ => {
            visible.loc
                + smithay::utils::Point::from((
                    (visible.size.w - size.w) / 2.0,
                    (visible.size.h - size.h) / 2.0,
                ))
        }
    };
    let max_x = (visible.loc.x + visible.size.w - size.w).max(visible.loc.x);
    let max_y = (visible.loc.y + visible.size.h - size.h).max(visible.loc.y);
    loc.x = loc.x.clamp(visible.loc.x, max_x);
    loc.y = loc.y.clamp(visible.loc.y, max_y);
    smithay::utils::Rectangle::new(loc, size)
}

#[cfg(test)]
mod tests {
    use super::canvas_new_window_rect;
    use crate::config::NewWindowPosition;
    use smithay::utils::{Point, Rectangle, Size};

    #[test]
    fn canvas_center_places_window_in_view() {
        let area = Rectangle::new(Point::from((10.0, 20.0)), Size::from((1000.0, 800.0)));
        let camera = Point::from((100.0, 200.0));
        let rect = canvas_new_window_rect(
            area,
            camera,
            Size::from((400.0, 300.0)),
            NewWindowPosition::Center,
            None,
        );
        assert_eq!(rect.loc, Point::from((410.0, 470.0)));
        assert_eq!(rect.size, Size::from((400.0, 300.0)));
    }

    #[test]
    fn canvas_cursor_center_places_and_clamps_to_visible_area() {
        let area = Rectangle::new(Point::from((10.0, 20.0)), Size::from((1000.0, 800.0)));
        let camera = Point::from((100.0, 200.0));
        let centered = canvas_new_window_rect(
            area,
            camera,
            Size::from((400.0, 300.0)),
            NewWindowPosition::CursorCenter,
            Some(Point::from((500.0, 500.0))),
        );
        assert_eq!(centered.loc, Point::from((300.0, 350.0)));

        let clamped = canvas_new_window_rect(
            area,
            camera,
            Size::from((400.0, 300.0)),
            NewWindowPosition::CursorCenter,
            Some(Point::from((1200.0, 1200.0))),
        );
        assert_eq!(clamped.loc, Point::from((710.0, 720.0)));
    }

    #[test]
    fn canvas_cursor_top_left_places_and_clamps_to_visible_area() {
        let area = Rectangle::new(Point::from((10.0, 20.0)), Size::from((1000.0, 800.0)));
        let camera = Point::from((100.0, 200.0));
        let placed = canvas_new_window_rect(
            area,
            camera,
            Size::from((400.0, 300.0)),
            NewWindowPosition::CursorTopLeft,
            Some(Point::from((300.0, 350.0))),
        );
        assert_eq!(placed.loc, Point::from((300.0, 350.0)));

        let clamped = canvas_new_window_rect(
            area,
            camera,
            Size::from((400.0, 300.0)),
            NewWindowPosition::CursorTopLeft,
            Some(Point::from((900.0, 900.0))),
        );
        assert_eq!(clamped.loc, Point::from((710.0, 720.0)));
    }

    #[test]
    fn oversized_window_pins_to_visible_origin() {
        let area = Rectangle::new(Point::from((0.0, 0.0)), Size::from((500.0, 400.0)));
        let rect = canvas_new_window_rect(
            area,
            Point::from((0.0, 0.0)),
            Size::from((800.0, 600.0)),
            NewWindowPosition::Center,
            None,
        );
        assert_eq!(rect.loc, Point::from((0.0, 0.0)));
    }
}
