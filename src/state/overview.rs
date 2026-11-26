//! Niri-style Overview mode: fixed-scale zoom-out across all outputs.

use smithay::{input::pointer::CursorImageStatus, utils::Serial};

use crate::{state::EvaWM, workspace::Mode};

impl EvaWM {
    pub fn toggle_overview(&mut self) {
        if self.overview_open {
            self.close_overview();
        } else {
            self.open_overview();
        }
    }

    pub fn open_overview(&mut self) {
        if self.overview_open {
            return;
        }
        self.cancel_interaction_state();

        // Exit any active fullscreen overlays across all outputs so no
        // fullscreen window sits unscaled above the zoomed-out workspace.
        for output_index in 0..self.outputs.len() {
            let active = self.outputs[output_index].active_workspace;
            if self.outputs[output_index].workspaces[active].has_fullscreen() {
                self.set_fullscreen_window_on_output(output_index, None, false);
            }
        }

        self.overview_open = true;
        self.clear_pointer_focus(self.event_time_msec());
        self.set_cursor_image(CursorImageStatus::default_named());

        let now = self.animation_now();
        let zoom = self.config.layout.overview.zoom;
        let mut config = self.config.animations.overview;
        if self.config.animations.off {
            config.off = true;
        }
        for output in &mut self.outputs {
            output.overview_zoom.set_target(zoom, now, config);
        }
        self.mark_full_redraw();
        tracing::debug!(zoom, "overview opened");
    }

    pub fn close_overview(&mut self) {
        if !self.overview_open {
            return;
        }
        if self.drag.is_some() {
            self.end_drag();
        }
        self.overview_open = false;

        let now = self.animation_now();
        let mut config = self.config.animations.overview;
        if self.config.animations.off {
            config.off = true;
        }
        for output in &mut self.outputs {
            output.overview_zoom.set_target(1.0, now, config);
        }
        self.sync_pointer_focus(self.event_time_msec());
        self.mark_full_redraw();
        tracing::debug!("overview closed");
    }

    /// Handle a primary left-click on the workspace area while Overview is
    /// open: clicking a window focuses it and exits Overview (centering Canvas
    /// if the picked window would be outside the unzoomed view); clicking empty
    /// space exits Overview.
    pub(crate) fn overview_click_at(&mut self, serial: Serial) {
        self.activate_pointer_output();
        let world = self
            .output_state()
            .overview_screen_to_world(self.pointer_pos);
        if let Some(window) = self.window_under(world) {
            self.focus_window(Some(&window), serial);
            if self.active_mode() == Mode::Canvas && !self.canvas_window_visible(&window) {
                self.canvas_center();
            }
        }
        self.close_overview();
    }
}
