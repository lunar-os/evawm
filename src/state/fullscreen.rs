//! Fullscreen overlay state and transitions.
//!
//! Fullscreen is a screen-space overlay. It does not change the workspace
//! layout or camera: the window is configured to the full output size and
//! rendered at screen `(0, 0)`, while every other window stays in its normal
//! layout position underneath.

use smithay::{
    desktop::Window,
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State,
    utils::{Logical, Rectangle, SERIAL_COUNTER, Size},
};

use crate::{state::EvaWM, workspace::canvas_pixel_size};

impl EvaWM {
    pub(crate) fn fullscreen_size_on_output(&self, output_index: usize) -> Size<i32, Logical> {
        let output = &self.outputs[output_index];
        let output_size = output.workspaces[output.active_workspace]
            .scroll
            .output_size;
        Size::from((
            output_size.w.round().max(1.0) as i32,
            output_size.h.round().max(1.0) as i32,
        ))
    }

    fn fullscreen_screen_rect_on_output(&self, output_index: usize) -> Rectangle<f64, Logical> {
        let output = &self.outputs[output_index];
        output.fullscreen_screen_rect(&output.workspaces[output.active_workspace])
    }

    /// Final screen-space content rectangle for a window leaving fullscreen.
    /// Uses the layout target size, not the client's still-fullscreen
    /// committed size, so the exit animation ends at the tile shape.
    fn content_screen_rect_on_output(
        &self,
        output_index: usize,
        window: &Window,
    ) -> Option<Rectangle<f64, Logical>> {
        let output = &self.outputs[output_index];
        let workspace = &output.workspaces[output.active_workspace];
        let target = workspace.content_target_rect_for_mode(window)?;
        let location = output.space.element_location(window)?.to_f64();
        Some(Rectangle::new(
            location - output.camera_location(),
            target.size,
        ))
    }

    pub(crate) fn configure_fullscreen_overlay(&mut self, window: &Window) {
        self.configure_fullscreen_overlay_on_output(self.active_output, window);
    }

    fn configure_fullscreen_overlay_on_output(&mut self, output_index: usize, window: &Window) {
        // A canvas resize queued just before fullscreen must not resize the
        // overlay or clear its FULLSCREEN state on the next frame.
        self.window_states.clear_queued_canvas_size(window);
        let size = self.fullscreen_size_on_output(output_index);
        let workspace_index = self.outputs[output_index].active_workspace;
        if self.outputs[output_index].workspaces[workspace_index].mode
            == crate::workspace::Mode::Scroll
        {
            self.outputs[output_index].workspaces[workspace_index]
                .scroll
                .configure_fullscreen_window(window, size);
            return;
        }
        let Some(toplevel) = window.toplevel().cloned() else {
            return;
        };
        toplevel.with_pending_state(|state| {
            state.size = Some(size);
            state.states.set(State::Fullscreen);
            state.states.unset(State::TiledLeft);
            state.states.unset(State::TiledRight);
            state.states.unset(State::TiledTop);
            state.states.unset(State::TiledBottom);
        });
        toplevel.send_pending_configure();
    }

    pub(crate) fn configure_normal_after_fullscreen(&mut self, window: &Window) {
        self.configure_normal_after_fullscreen_on_output(self.active_output, window);
    }

    fn configure_normal_after_fullscreen_on_output(
        &mut self,
        output_index: usize,
        window: &Window,
    ) {
        let workspace_index = self.outputs[output_index].active_workspace;
        if self.outputs[output_index].workspaces[workspace_index].mode
            != crate::workspace::Mode::Canvas
        {
            // Scroll mode reconfigures tiles on the next layout pass.
            return;
        }
        let Some(rect) = self.outputs[output_index].workspaces[workspace_index]
            .canvas
            .rect(window)
        else {
            return;
        };
        self.configure_canvas_window(window, canvas_pixel_size(rect));
    }

    fn start_fullscreen_animation_on_output(
        &mut self,
        output_index: usize,
        window: &Window,
        from: Rectangle<f64, Logical>,
        to: Rectangle<f64, Logical>,
        entering: bool,
        now: f64,
    ) -> bool {
        let (border_from, border_to) = if entering { (1.0, 0.0) } else { (0.0, 1.0) };
        let mut config = self.config.animations.window_resize;
        if self.config.animations.off {
            config.off = true;
        }
        let started = self.window_states.start_fullscreen_transition(
            window,
            from,
            to,
            border_from,
            border_to,
            now,
            config,
        );
        if started {
            self.mark_full_redraw_output(output_index);
        }
        started
    }

    /// Change the fullscreen overlay on `output_index`'s active workspace.
    /// `None` exits.
    pub(crate) fn set_fullscreen_window_on_output(
        &mut self,
        output_index: usize,
        window: Option<Window>,
        animate: bool,
    ) {
        let index = self.outputs[output_index].active_workspace;
        let now = self.animation_now();
        let current = self.outputs[output_index].workspaces[index]
            .fullscreen_window()
            .cloned();
        if current == window {
            return;
        }
        if window.is_some() && self.overview_open {
            self.close_overview();
        }

        tracing::debug!(
            output_index,
            ?window,
            ?current,
            animate,
            "set fullscreen window"
        );
        let entering = window.is_some();
        let from = window
            .as_ref()
            .or(current.as_ref())
            .and_then(|subject| {
                self.outputs[output_index].current_content_screen_rect(
                    subject,
                    now,
                    &self.window_states,
                )
            })
            .unwrap_or_else(|| self.fullscreen_screen_rect_on_output(output_index));

        let old = current;
        {
            let workspace = &mut self.outputs[output_index].workspaces[index];
            if let Some(old) = old.as_ref() {
                workspace.exit_fullscreen();
                self.window_states.cancel_transition(old);
            }
            if let Some(window) = window.as_ref() {
                workspace.enter_fullscreen(window.clone());
                if workspace.mode == crate::workspace::Mode::Scroll
                    && let Some((column, tile)) = workspace.scroll.tile_index(window)
                {
                    workspace.scroll.set_active_tile(column, tile);
                }
                workspace.set_focus(Some(window.clone()));
            }
        }

        if let Some(window) = window.as_ref() {
            self.configure_fullscreen_overlay_on_output(output_index, window);
        }
        self.apply_layout_output(output_index);
        if let Some(old) = old.as_ref() {
            self.configure_normal_after_fullscreen_on_output(output_index, old);
        }

        if animate {
            let target = window.as_ref().or(old.as_ref());
            if let Some(target) = target {
                let to = if entering {
                    self.fullscreen_screen_rect_on_output(output_index)
                } else {
                    self.content_screen_rect_on_output(output_index, target)
                        .unwrap_or_else(|| self.fullscreen_screen_rect_on_output(output_index))
                };
                self.start_fullscreen_animation_on_output(
                    output_index,
                    target,
                    from,
                    to,
                    entering,
                    now,
                );
            }
        }

        if output_index == self.active_output {
            let serial = SERIAL_COUNTER.next_serial();
            let focused = self.outputs[output_index].workspaces[index]
                .focused_window()
                .cloned();
            self.focus_window(focused.as_ref(), serial);
        } else {
            self.notify_ipc();
        }
        self.request_redraw_output(output_index);
    }

    /// Leave fullscreen for a same-workspace action on `output_index`.
    pub(crate) fn leave_fullscreen_for_action_on_output(&mut self, output_index: usize) {
        let index = self.outputs[output_index].active_workspace;
        let window = self.outputs[output_index].workspaces[index]
            .fullscreen_window()
            .cloned();
        let Some(window) = window else {
            return;
        };
        self.outputs[output_index].workspaces[index].exit_fullscreen();
        self.window_states.cancel_transition(&window);
        self.apply_layout_output(output_index);
        self.configure_normal_after_fullscreen_on_output(output_index, &window);
        self.request_redraw_output(output_index);
    }

    /// Leave fullscreen for a same-workspace action.
    pub(crate) fn leave_fullscreen_for_action(&mut self) {
        self.leave_fullscreen_for_action_on_output(self.active_output);
    }

    /// Toggle fullscreen for the focused window on the active workspace.
    pub fn toggle_fullscreen(&mut self) {
        if self.drag.is_some() {
            return;
        }

        let output_index = self.active_output;
        let index = self.active_workspace();
        let current = self.outputs[output_index].workspaces[index]
            .fullscreen_window()
            .cloned();
        let focused = self.outputs[output_index].workspaces[index]
            .focused_window()
            .cloned();
        let target = match (&current, &focused) {
            (Some(current), Some(focused)) if current == focused => None,
            (_, Some(_)) => focused,
            _ => None,
        };
        let animate = current.is_none() || target.is_none();
        self.set_fullscreen_window_on_output(output_index, target, animate);
    }

    /// Apply an external fullscreen request on any output (for example from
    /// `wlr-foreign-toplevel`).
    pub fn set_window_fullscreen(&mut self, window: &Window, fullscreen: bool) {
        self.set_window_fullscreen_inner(window, fullscreen, false);
    }

    /// Apply a client-initiated `xdg_toplevel.set_fullscreen` /
    /// `unset_fullscreen` request. Unfocused windows cannot hijack the
    /// fullscreen overlay or steal focus from another window on the workspace.
    pub(crate) fn set_window_fullscreen_from_client(&mut self, window: &Window, fullscreen: bool) {
        self.set_window_fullscreen_inner(window, fullscreen, true);
    }

    fn set_window_fullscreen_inner(
        &mut self,
        window: &Window,
        fullscreen: bool,
        from_client: bool,
    ) {
        let reject = || {
            if let Some(toplevel) = window.toplevel() {
                toplevel.send_configure();
            }
        };
        if self.drag.is_some() {
            reject();
            return;
        }
        let Some((output_index, index)) = self.workspace_for_window(window) else {
            reject();
            return;
        };
        if index != self.outputs[output_index].active_workspace {
            reject();
            return;
        }

        let workspace = &self.outputs[output_index].workspaces[index];
        let current = workspace.fullscreen_window().cloned();
        let target = if fullscreen {
            if current.as_ref() == Some(window) {
                reject();
                return;
            }
            // Reject client-initiated fullscreen from unfocused windows so a
            // background client (such as a fullscreen game reacting to focus
            // loss) cannot steal focus and re-cover the workspace.
            if from_client && workspace.focused_window() != Some(window) {
                tracing::debug!(
                    output_index,
                    workspace = index,
                    "ignoring client fullscreen request from unfocused window"
                );
                reject();
                return;
            }
            Some(window.clone())
        } else {
            if current.as_ref() != Some(window) {
                reject();
                return;
            }
            None
        };
        let animate = current.is_none() || target.is_none();
        self.set_fullscreen_window_on_output(output_index, target, animate);
    }
}
