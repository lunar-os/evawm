use smithay::{
    desktop::{LayerSurface, PopupKind, WindowSurfaceType, layer_map_for_output},
    output::Output,
    reexports::wayland_server::protocol::{wl_output, wl_surface::WlSurface},
    utils::SERIAL_COUNTER,
    wayland::{
        compositor::with_states,
        shell::{
            wlr_layer::{
                KeyboardInteractivity, Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData,
                WlrLayerShellHandler, WlrLayerShellState,
            },
            xdg::PopupSurface,
        },
    },
};

use crate::{handlers::compositor::is_mapped, state::EvaWM};

impl WlrLayerShellHandler for EvaWM {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        wl_output: Option<wl_output::WlOutput>,
        layer: Layer,
        namespace: String,
    ) {
        let output_index = wl_output
            .as_ref()
            .and_then(Output::from_resource)
            .and_then(|target| {
                self.outputs
                    .iter()
                    .position(|candidate| candidate.output == target)
            })
            .unwrap_or(self.active_output);
        let output = self.outputs[output_index].output.clone();

        let layer_surface = LayerSurface::new(surface, namespace.clone());
        {
            let mut map = layer_map_for_output(&output);
            if let Err(err) = map.map_layer(&layer_surface) {
                tracing::warn!("failed to map layer surface {namespace:?}: {err:?}");
                return;
            }
        }

        tracing::debug!(?layer, %namespace, "layer surface mapped");
        self.update_working_area_output(output_index);
    }

    fn new_popup(&mut self, _parent: WlrLayerSurface, popup: PopupSurface) {
        self.unconstrain_popup(&popup);
        let _ = self.popups.track_popup(PopupKind::Xdg(popup));
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let was_focused = self.keyboard.current_focus().as_ref() == Some(surface.wl_surface());
        let found = self
            .outputs
            .iter()
            .enumerate()
            .find_map(|(output_index, output)| {
                let map = layer_map_for_output(&output.output);
                let layer = map
                    .layers()
                    .find(|layer| layer.layer_surface() == &surface)
                    .cloned();
                layer.map(|layer| (output_index, map, layer))
            });
        let output_index = found.map(|(output_index, mut map, layer)| {
            map.unmap_layer(&layer);
            output_index
        });
        if was_focused {
            let serial = SERIAL_COUNTER.next_serial();
            let window = self.output_state().workspaces[self.active_workspace()]
                .focused_window()
                .cloned();
            self.focus_window(window.as_ref(), serial);
        }
        if let Some(output_index) = output_index {
            self.update_working_area_output(output_index);
        } else {
            self.request_redraw();
        }
    }
}

smithay::delegate_layer_shell!(EvaWM);

/// Called from the compositor commit handler for non-window surfaces.
///
/// Returns the owning output index if the surface belongs to a layer and an
/// arrange was done.
pub fn handle_commit(state: &mut EvaWM, surface: &WlSurface) -> Option<usize> {
    let output_index = state.outputs.iter().position(|output| {
        let map = layer_map_for_output(&output.output);
        map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
            .is_some()
    })?;

    let mut map = layer_map_for_output(&state.outputs[output_index].output);
    map.arrange();

    let initial_configure_sent = with_states(surface, |states| {
        states
            .data_map
            .get::<LayerSurfaceData>()
            .and_then(|data| data.lock().ok().map(|guard| guard.initial_configure_sent))
            .unwrap_or(false)
    });

    let mapped = is_mapped(surface);
    let mut focus = None;
    let mut release_focus = false;
    if let Some(layer) = map
        .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
        .cloned()
    {
        if !initial_configure_sent {
            let _ = layer.layer_surface().send_configure();
        }

        let is_focused = state.keyboard.current_focus().as_ref() == Some(layer.wl_surface());
        // Exclusive keyboard interactivity is only meaningful for mapped Top
        // and Overlay layers (such as launchers); OnDemand surfaces wait for an
        // explicit click and Background/Bottom layers never steal focus.
        let wants_exclusive_focus = mapped
            && matches!(layer.layer(), Layer::Top | Layer::Overlay)
            && layer.cached_state().keyboard_interactivity == KeyboardInteractivity::Exclusive;
        if wants_exclusive_focus && !is_focused {
            focus = Some(layer);
        } else if is_focused && (!mapped || !layer.can_receive_keyboard_focus()) {
            release_focus = true;
        }
    }
    drop(map);

    if let Some(layer) = focus {
        state.focus_layer_surface(&layer);
    } else if release_focus {
        let serial = SERIAL_COUNTER.next_serial();
        let window = state.output_state().workspaces[state.active_workspace()]
            .focused_window()
            .cloned();
        state.focus_window(window.as_ref(), serial);
    }

    Some(output_index)
}
