//! Focus, activation, layer hit-testing, and pointer focus queries.

use smithay::{
    desktop::{LayerSurface, Window, WindowSurfaceType, layer_map_for_output},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, SERIAL_COUNTER, Serial},
    wayland::shell::wlr_layer::Layer as WlrLayer,
};

use crate::state::EvaWM;

#[derive(Debug)]
pub(crate) struct MruCycle {
    /// MRU order captured when the cycle started.
    windows: Vec<Window>,
    /// Index of the currently selected window in `windows`.
    index: usize,
}

fn next_cycle_index(len: usize, base: usize, forward: bool) -> usize {
    debug_assert!(len > 0);
    if forward {
        (base + 1) % len
    } else {
        (base + len - 1) % len
    }
}

impl EvaWM {
    pub fn surface_under_on(
        &self,
        output_index: usize,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let output = self.outputs.get(output_index)?;
        let active = &output.workspaces[output.active_workspace];
        if let Some(fullscreen) = active.fullscreen_window() {
            let camera = output.camera_location();
            let output_point = pos - camera;
            let output_rect = smithay::utils::Rectangle::from_size(active.scroll.output_size);
            if !output_rect.contains(output_point) {
                return None;
            }
            let geometry = fullscreen.geometry().loc.to_f64();
            let surface_origin = Point::from((0.0, 0.0)) - geometry;
            return fullscreen
                .surface_under(output_point + geometry, WindowSurfaceType::ALL)
                .map(|(surface, origin)| (surface, camera + surface_origin + origin.to_f64()));
        }
        output.space.elements().rev().find_map(|window| {
            if self.window_states.is_closing(window) {
                return None;
            }
            let origin = output.window_surface_origin(window)?;
            window
                .surface_under(pos - origin.to_f64(), WindowSurfaceType::ALL)
                .map(|(surface, point)| (surface, (point + origin).to_f64()))
        })
    }

    pub fn fullscreen_above_top_layer_on(&self, output_index: usize) -> bool {
        let Some(output) = self.outputs.get(output_index) else {
            return false;
        };
        let active = &output.workspaces[output.active_workspace];
        active.fullscreen.is_some()
            || self
                .window_states
                .has_top_transition_in(output.space.elements())
    }

    pub fn layer_above_windows(&self, pos: Point<f64, Logical>) -> Option<WlSurface> {
        let output_index = self.pointer_output;
        let output = self.outputs.get(output_index)?;
        output
            .layer_above_windows_under(pos, self.fullscreen_above_top_layer_on(output_index))
            .map(|(surface, _)| surface)
    }

    pub fn keyboard_layer_focused(&self) -> bool {
        let Some(focus) = self.keyboard.current_focus() else {
            return false;
        };
        self.outputs.iter().any(|output| {
            layer_map_for_output(&output.output)
                .layer_for_surface(&focus, WindowSurfaceType::TOPLEVEL)
                .is_some()
        })
    }

    /// Give keyboard focus to a keyboard-interactive layer surface (launchers,
    /// prompt surfaces, lock screens).
    pub fn focus_layer_surface(&mut self, layer: &LayerSurface) {
        if !layer.can_receive_keyboard_focus() {
            return;
        }
        let serial = SERIAL_COUNTER.next_serial();
        let target = layer.wl_surface().clone();
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(self, Some(target), serial);
        tracing::debug!(namespace = layer.namespace(), "focused keyboard layer");
        for output_index in 0..self.outputs.len() {
            let contains = layer_map_for_output(&self.outputs[output_index].output)
                .layer_for_surface(layer.wl_surface(), WindowSurfaceType::TOPLEVEL)
                .is_some();
            if contains {
                self.request_redraw_output(output_index);
            }
        }
    }

    fn focus_layer_in(&mut self, pos: Point<f64, Logical>, layers: &[WlrLayer]) -> bool {
        let Some(output) = self.outputs.get(self.pointer_output) else {
            return false;
        };
        let layer = {
            let map = layer_map_for_output(&output.output);
            layers.iter().find_map(|&layer| {
                map.layers_on(layer).rev().find_map(|layer_surface| {
                    let geometry = map.layer_geometry(layer_surface)?;
                    layer_surface
                        .surface_under(pos - geometry.loc.to_f64(), WindowSurfaceType::ALL)?;
                    layer_surface
                        .can_receive_keyboard_focus()
                        .then(|| layer_surface.clone())
                })
            })
        };
        if let Some(layer) = layer {
            self.focus_layer_surface(&layer);
            true
        } else {
            false
        }
    }

    /// Focus a keyboard-interactive layer surface above windows (Overlay, and
    /// Top unless fullscreen covers Top) under a pointer-output local position.
    pub fn focus_layer_above_windows_at(&mut self, pos: Point<f64, Logical>) -> bool {
        let layers: &[WlrLayer] = if self.fullscreen_above_top_layer_on(self.pointer_output) {
            &[WlrLayer::Overlay]
        } else {
            &[WlrLayer::Overlay, WlrLayer::Top]
        };
        self.focus_layer_in(pos, layers)
    }

    /// Focus a keyboard-interactive Bottom-layer surface under a pointer-output
    /// local position when no window sits above it.
    pub fn focus_layer_below_windows_at(&mut self, pos: Point<f64, Logical>) -> bool {
        if self.fullscreen_above_top_layer_on(self.pointer_output) {
            return false;
        }
        self.focus_layer_in(pos, &[WlrLayer::Bottom])
    }

    pub fn touch_mru(&mut self, window: &Window) {
        self.mru.retain(|entry| entry != window);
        self.mru.insert(0, window.clone());
    }

    pub fn remove_mru(&mut self, window: &Window) {
        self.mru.retain(|entry| entry != window);
        if let Some(cycle) = &mut self.mru_cycle {
            if let Some(index) = cycle.windows.iter().position(|entry| entry == window) {
                cycle.windows.remove(index);
                if index < cycle.index {
                    cycle.index -= 1;
                } else if cycle.index >= cycle.windows.len() {
                    cycle.index = cycle.windows.len().saturating_sub(1);
                }
            }
            if cycle.windows.is_empty() {
                self.mru_cycle = None;
                self.mru_cycle_step = false;
            }
        }
    }

    fn mod_held(&self) -> bool {
        let mods = self.keyboard.modifier_state();
        let mod_key = if self.nested_mod_alt {
            mods.alt
        } else {
            mods.logo
        };
        mod_key || self.extra_mod_held
    }

    pub(crate) fn activate_pointer_output(&mut self) {
        if self.drag.is_some() || self.dnd_active || self.keyboard_layer_focused() {
            return;
        }
        if self.active_output == self.pointer_output || self.pointer_output >= self.outputs.len() {
            return;
        }
        // Commit any active MRU cycle against the previous output before moving
        // `active_output`.
        self.commit_mru_cycle();
        let previous = self.active_output;
        self.active_output = self.pointer_output;
        let serial = SERIAL_COUNTER.next_serial();
        let window = self.output_state().workspaces[self.active_workspace()]
            .focused_window()
            .cloned();
        // Passive monitor crossing activates the output's focused window
        // without raising Canvas windows underneath the cursor.
        self.focus_window_internal(window.as_ref(), serial, false);
        self.request_redraw_output(previous);
        self.request_redraw_output(self.active_output);
    }

    pub fn focus_output_direction(&mut self, direction: crate::workspace::Direction) {
        // Drags are locked to the output where they started; changing
        // `active_output` mid-drag would route the pointer and layout updates
        // through the wrong output.
        if self.drag.is_some() || self.dnd_active {
            return;
        }
        let Some(target) = self.output_in_direction(self.active_output, direction) else {
            return;
        };
        if target == self.active_output {
            return;
        }
        self.commit_mru_cycle();
        let previous = self.active_output;
        self.active_output = target;
        let serial = SERIAL_COUNTER.next_serial();
        let window = self.output_state().workspaces[self.active_workspace()]
            .focused_window()
            .cloned();
        self.focus_window(window.as_ref(), serial);
        // Keep the cursor with keyboard focus. This is deliberately not
        // focus-follows-mouse: warping must not steal focus from the window
        // the user just selected with the keyboard.
        let time = self.start_time.elapsed().as_millis() as u32;
        self.warp_pointer_to_output_center(target, time);
        self.request_redraw_output(previous);
        self.request_redraw_output(target);
    }

    /// Commit the current MRU cycle selection, moving only the final choice
    /// to the front of the history.
    pub fn commit_mru_cycle(&mut self) {
        let Some(cycle) = self.mru_cycle.take() else {
            return;
        };
        self.mru_cycle_step = false;
        let focused = self.output_state().workspaces[self.active_workspace()]
            .focused_window()
            .cloned();
        if let Some(window) = focused {
            tracing::debug!(
                index = cycle.index,
                count = cycle.windows.len(),
                size = ?window.geometry().size,
                "mru cycle commit"
            );
            self.touch_mru(&window);
        } else if let Some(window) = cycle.windows.get(cycle.index)
            && self.workspace_for_window(window).is_some()
        {
            self.touch_mru(window);
        }
    }

    /// Cycle the MRU list scoped to the active workspace. While `Mod` is held,
    /// this walks a stable snapshot; the snapshot is committed on modifier
    /// release or on the next non-cycle action.
    pub fn cycle_mru(&mut self, forward: bool) {
        let active_output = self.active_output;
        let active_workspace = self.active_workspace();
        if self.mru_cycle.is_none() {
            let current = self.output_state().workspaces[active_workspace]
                .focused_window()
                .cloned();
            let mut windows: Vec<Window> = self
                .mru
                .iter()
                .filter(|window| {
                    self.workspace_for_window(window) == Some((active_output, active_workspace))
                })
                .cloned()
                .collect();
            for window in self.outputs[active_output].workspaces[active_workspace].windows() {
                if !windows.contains(window) {
                    windows.push(window.clone());
                }
            }
            if windows.is_empty() {
                return;
            }
            let index = match current
                .as_ref()
                .and_then(|window| windows.iter().position(|entry| entry == window))
            {
                Some(index) => index,
                None if forward => windows.len() - 1,
                None => 0,
            };
            self.mru_cycle = Some(MruCycle { windows, index });
        }

        let Some(mut cycle) = self.mru_cycle.take() else {
            return;
        };
        let selected = cycle.windows.get(cycle.index).cloned();
        cycle.windows.retain(|window| {
            self.workspace_for_window(window) == Some((active_output, active_workspace))
        });
        if cycle.windows.is_empty() {
            self.mru_cycle_step = false;
            return;
        }
        let base = selected
            .and_then(|window| cycle.windows.iter().position(|entry| entry == &window))
            .unwrap_or(if forward { cycle.windows.len() - 1 } else { 0 });
        let index = next_cycle_index(cycle.windows.len(), base, forward);
        cycle.index = index;
        let target = cycle.windows[index].clone();
        tracing::debug!(
            index,
            count = cycle.windows.len(),
            size = ?target.geometry().size,
            "mru cycle step"
        );

        self.mru_cycle = Some(cycle);
        self.mru_cycle_step = true;
        let serial = SERIAL_COUNTER.next_serial();
        self.focus_window(Some(&target), serial);
        if self.active_mode() == crate::workspace::Mode::Canvas {
            self.canvas_center();
        }
        self.mru_cycle_step = false;
        // If triggered via IPC or a non-Mod bind, no Mod release will follow;
        // commit immediately so hover focus and MRU history are not stuck.
        if !self.mod_held() {
            self.commit_mru_cycle();
        }
        self.request_redraw();
    }

    pub fn focus_window(&mut self, window: Option<&Window>, serial: Serial) {
        self.focus_window_internal(window, serial, true);
        self.request_redraw();
    }

    pub(crate) fn focus_window_internal(
        &mut self,
        window: Option<&Window>,
        serial: Serial,
        raise_canvas: bool,
    ) {
        if !self.mru_cycle_step {
            self.commit_mru_cycle();
        }
        let (changed, target) = {
            let output = &mut self.outputs[self.active_output];
            let workspace = &mut output.workspaces[output.active_workspace];
            let target = window.and_then(|w| w.toplevel().map(|t| t.wl_surface().clone()));

            let changed = match workspace.mode {
                crate::workspace::Mode::Scroll => {
                    // Keep the layout active column/tile in sync with keyboard
                    // focus and bring the column into view (niri's
                    // `center-focused-column "never"` fit).
                    let changed = if let Some(window) = window {
                        let Some((column_index, tile_index)) = workspace.scroll.tile_index(window)
                        else {
                            return;
                        };
                        workspace.scroll.set_active_tile(column_index, tile_index)
                    } else {
                        false
                    };
                    workspace.set_focus(window.cloned());
                    changed
                }
                crate::workspace::Mode::Canvas => {
                    if let Some(window) = window
                        && !workspace.canvas.contains(window)
                    {
                        return;
                    }
                    workspace.set_focus(window.cloned());
                    if let Some(window) = window
                        && raise_canvas
                    {
                        workspace.canvas.raise(window);
                    }
                    false
                }
            };
            (changed, target)
        };

        if changed {
            self.apply_layout();
        }

        if raise_canvas
            && self.active_mode() == crate::workspace::Mode::Canvas
            && let Some(window) = window
        {
            self.output_state_mut().space.raise_element(window, false);
        }

        for (output_index, output) in self.outputs.iter().enumerate() {
            for w in output.space.elements() {
                let active = output_index == self.active_output && window == Some(w);
                w.set_activated(active);
                if let Some(toplevel) = w.toplevel() {
                    toplevel.send_pending_configure();
                }
            }
        }

        if let Some(window) = window
            && !self.mru_cycle_step
        {
            self.touch_mru(window);
        }

        let keyboard = self.keyboard.clone();
        keyboard.set_focus(self, target, serial);
        self.notify_ipc();
    }
}

#[cfg(test)]
mod tests {
    use super::next_cycle_index;

    #[test]
    fn mru_cycle_wraps_in_both_directions() {
        assert_eq!(next_cycle_index(3, 0, true), 1);
        assert_eq!(next_cycle_index(3, 1, true), 2);
        assert_eq!(next_cycle_index(3, 2, true), 0);
        assert_eq!(next_cycle_index(3, 0, false), 2);
        assert_eq!(next_cycle_index(3, 2, false), 1);
        assert_eq!(next_cycle_index(3, 1, false), 0);
        assert_eq!(next_cycle_index(1, 0, true), 0);
        assert_eq!(next_cycle_index(1, 0, false), 0);
    }
}
