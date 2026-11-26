//! Live config reload.

use crate::{config::Config, cursor::Cursor, state::EvaWM};

impl EvaWM {
    /// Ask the watcher thread to re-read the config immediately. File changes
    /// are picked up automatically; this is for the `reload-config` action.
    pub fn reload_config(&mut self) {
        if let Some(watcher) = &self.config_watcher {
            watcher.reload();
            tracing::info!("config reload requested");
            return;
        }

        let Some(path) = self.config_path.clone() else {
            tracing::info!("no user config file to reload");
            return;
        };

        match Config::load(Some(&path)) {
            Ok((config, _)) => self.apply_config(config),
            Err(err) => tracing::warn!("config reload failed; keeping old config: {err:#}"),
        }
    }

    /// Apply a successfully parsed config without disturbing windows,
    /// workspaces, cameras, or the startup commands that already ran.
    pub fn apply_config(&mut self, config: Config) {
        let xwayland_changed = config.xwayland != self.config.xwayland;
        let outputs_changed = config.outputs != self.config.outputs;
        let cursor_changed = config.cursor.theme != self.config.cursor.theme
            || config.cursor.size != self.config.cursor.size;
        if cursor_changed {
            self.cursor = Cursor::load(&config.cursor.theme, config.cursor.size);
            // Force the next frame to rebuild cached cursor buffers.
            self.cursor_cache.clear();
            self.cursor_icon = None;
        }

        self.animation_clock
            .set_slowdown(config.animations.slowdown);

        for output in &mut self.outputs {
            output.default_mode = config.layout.default_mode;
            for workspace in &mut output.workspaces {
                workspace.scroll.set_options(
                    config.layout.gaps,
                    config.layout.default_column_proportion,
                    config.layout.inner_border,
                    config.layout.outer_border,
                );
            }
        }

        tracing::info!(
            binds = config.binds.len(),
            gaps = config.layout.gaps,
            inner_border = config.layout.inner_border.width,
            outer_border = config.layout.outer_border.width,
            "config reloaded"
        );

        if outputs_changed {
            tracing::warn!("output changes require a session restart");
        }

        // Replacing the config keeps startup commands as data only; they are
        // not run again.
        let overview_zoom_changed =
            (self.config.layout.overview.zoom - config.layout.overview.zoom).abs() > f64::EPSILON;
        let xkb_changed = self.config.input.xkb != config.input.xkb;
        let repeat_changed = self.config.input.repeat_delay != config.input.repeat_delay
            || self.config.input.repeat_rate != config.input.repeat_rate;
        self.config = config;
        if xkb_changed {
            let xkb = self.config.input.xkb.clone();
            let keyboard = self.keyboard.clone();
            if let Err(err) = keyboard.set_xkb_config(self, xkb.to_smithay()) {
                tracing::warn!(
                    ?err,
                    layout = %xkb.layout,
                    variant = %xkb.variant,
                    "reloaded XKB keymap failed; keeping previous keymap"
                );
            }
        }
        if repeat_changed {
            let rate = i32::try_from(self.config.input.repeat_rate).unwrap_or(25);
            let delay = i32::try_from(self.config.input.repeat_delay).unwrap_or(250);
            self.keyboard.change_repeat_info(rate, delay);
        }
        if self.overview_open && overview_zoom_changed {
            let now = self.animation_now();
            let zoom = self.config.layout.overview.zoom;
            let mut anim = self.config.animations.overview;
            if self.config.animations.off {
                anim.off = true;
            }
            for output in &mut self.outputs {
                output.overview_zoom.set_target(zoom, now, anim);
            }
        }
        if self.config.animations.off {
            self.window_states.settle_all_transitions();
            for output in &mut self.outputs {
                let target_x = output.view_x.target;
                let target_y = output.view_y.target;
                let target_zoom = output.overview_zoom.target;
                output.view_x.snap(target_x);
                output.view_y.snap(target_y);
                output.overview_zoom.snap(target_zoom);
                output.map_output();
            }
        }
        self.reapply_libinput_settings();

        if xwayland_changed {
            if self.config.xwayland.enabled {
                let display = self.start_xwayland();
                if self.is_session() {
                    crate::session_env::update_display_environment(display.as_deref());
                }
            } else {
                self.stop_xwayland();
                if self.is_session() {
                    crate::session_env::update_display_environment(None);
                }
            }
        }

        // Border widths affect client configure sizes, so re-run the active
        // layout on every output immediately.
        for output_index in 0..self.outputs.len() {
            self.apply_layout_output(output_index);
        }
        self.mark_full_redraw();
        self.request_redraw_all();
        self.notify_ipc();
    }
}
