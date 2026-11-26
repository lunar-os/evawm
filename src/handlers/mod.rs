pub mod compositor;
pub mod foreign_toplevel;
pub mod idle;
pub mod layer;
pub mod workspace;
pub mod xdg_shell;

use std::time::Duration;

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::ImportDma;
use smithay::delegate_dmabuf;
use smithay::input::pointer::PointerHandle;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::Output;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::{wl_output::WlOutput, wl_surface::WlSurface};
use smithay::utils::{IsAlive, Logical, Point};
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::keyboard_shortcuts_inhibit::{
    KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitState, KeyboardShortcutsInhibitor,
};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::pointer_constraints::{PointerConstraintsHandler, with_pointer_constraint};
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::data_device::{
    DataDeviceHandler, DataDeviceState, set_data_device_focus,
};
use smithay::wayland::selection::primary_selection::{
    PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
};
use smithay::wayland::selection::{ext_data_control, wlr_data_control};
use smithay::wayland::tablet_manager::TabletSeatHandler;
use smithay::wayland::xdg_activation::{
    XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
};
use smithay::wayland::xdg_system_bell::XdgSystemBellHandler;
use smithay::{
    delegate_cursor_shape, delegate_keyboard_shortcuts_inhibit, delegate_pointer_constraints,
    delegate_presentation, delegate_primary_selection, delegate_relative_pointer,
    delegate_text_input_manager, delegate_viewporter, delegate_xdg_activation,
    delegate_xdg_system_bell,
};
use smithay::{delegate_data_device, delegate_output, delegate_seat};

use crate::state::EvaWM;

impl SeatHandler for EvaWM {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<EvaWM> {
        &mut self.seat_state
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        image: smithay::input::pointer::CursorImageStatus,
    ) {
        self.set_cursor_image(image);
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|surface| self.display_handle.get_client(surface.id()).ok());
        set_data_device_focus(&self.display_handle, seat, client.clone());
        set_primary_focus(&self.display_handle, seat, client);
    }
}

delegate_seat!(EvaWM);

impl SelectionHandler for EvaWM {
    type SelectionUserData = ();
}

impl DataDeviceHandler for EvaWM {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

delegate_data_device!(EvaWM);

impl OutputHandler for EvaWM {
    fn output_bound(&mut self, output: Output, wl_output: WlOutput) {
        self.foreign_toplevel_output_bound(&output, &wl_output);
        self.workspace_protocol_output_bound(&output, &wl_output);
    }
}
delegate_output!(EvaWM);

impl DmabufHandler for EvaWM {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        self.dmabuf_state
            .as_mut()
            .expect("dmabuf state not initialized")
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        let imported = match &mut self.backend {
            #[cfg(feature = "nested")]
            Some(crate::state::Backend::Nested(nested)) => nested
                .graphics
                .renderer()
                .import_dmabuf(&dmabuf, None)
                .is_ok(),
            Some(crate::state::Backend::Session(session)) => {
                let ok = session.renderer.import_dmabuf(&dmabuf, None).is_ok();
                if ok {
                    dmabuf.set_node(session.render_node);
                }
                ok
            }
            None => false,
        };

        tracing::trace!(imported, "dmabuf import");
        if imported {
            let _ = notifier.successful::<EvaWM>();
        } else {
            notifier.failed();
        }
    }
}

delegate_dmabuf!(EvaWM);

impl PrimarySelectionHandler for EvaWM {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

delegate_primary_selection!(EvaWM);

impl ext_data_control::DataControlHandler for EvaWM {
    fn data_control_state(&mut self) -> &mut ext_data_control::DataControlState {
        &mut self.ext_data_control_state
    }
}

smithay::delegate_ext_data_control!(EvaWM);

impl wlr_data_control::DataControlHandler for EvaWM {
    fn data_control_state(&mut self) -> &mut wlr_data_control::DataControlState {
        &mut self.wlr_data_control_state
    }
}

smithay::delegate_data_control!(EvaWM);

delegate_presentation!(EvaWM);
delegate_pointer_constraints!(EvaWM);
delegate_relative_pointer!(EvaWM);
delegate_keyboard_shortcuts_inhibit!(EvaWM);

impl KeyboardShortcutsInhibitHandler for EvaWM {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        &mut self.keyboard_shortcuts_inhibit_state
    }

    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        inhibitor.activate();
    }
}

impl PointerConstraintsHandler for EvaWM {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        // Activate immediately only when `surface` already holds pointer focus
        // and the cursor is inside the constraint region. Otherwise
        // `move_pointer` activates it once the pointer enters the surface
        // (including gamescope's pre-enter lock).
        if self.overview_open
            || self.drag.is_some()
            || pointer.current_focus().as_ref() != Some(surface)
        {
            return;
        }
        let surface_local = self
            .pointer_focus_under(self.pointer_pos)
            .filter(|(hit, _)| hit == surface)
            .map(|(_, origin)| {
                (self.pointer_pos + self.pointer_camera_location() - origin).to_i32_round()
            });
        let Some(surface_local) = surface_local else {
            return;
        };
        with_pointer_constraint(surface, pointer, |constraint| {
            if let Some(constraint) = constraint
                && constraint
                    .region()
                    .is_none_or(|region| region.contains(surface_local))
            {
                constraint.activate();
            }
        });
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        if self.overview_open
            || self.drag.is_some()
            || pointer.current_focus().as_ref() != Some(surface)
        {
            return;
        }
        let active = with_pointer_constraint(surface, pointer, |constraint| {
            constraint.is_some_and(|constraint| constraint.is_active())
        });
        if !active {
            return;
        }

        let target = self
            .outputs
            .iter()
            .enumerate()
            .find_map(|(output_index, output)| {
                let active_ws = &output.workspaces[output.active_workspace];
                if let Some(fullscreen) = active_ws.fullscreen_window()
                    && fullscreen.wl_surface().as_deref() == Some(surface)
                {
                    let origin = output.camera_location() - fullscreen.geometry().loc.to_f64();
                    return Some((output_index, origin));
                }
                output.space.elements().find_map(|window| {
                    (window.wl_surface().as_deref() == Some(surface))
                        .then(|| output.window_surface_origin(window).map(Point::to_f64))
                        .flatten()
                        .map(|origin| (output_index, origin))
                })
            })
            .or_else(|| {
                // Fall back to the live pointer focus snapshot for subsurfaces
                // or layer surfaces that hold a constraint directly.
                self.pointer_motion.as_ref().and_then(|snapshot| {
                    (snapshot.surface.as_ref() == Some(surface))
                        .then_some((self.pointer_output, snapshot.origin))
                })
            });

        let Some((output_index, origin)) = target else {
            return;
        };
        let Some(rect) = self.outputs[output_index].global_rect() else {
            return;
        };
        let camera = self.outputs[output_index].camera_location();
        let raw_local = (origin + location) - camera;
        let clamped_local = Point::from((
            raw_local.x.clamp(0.0, (rect.size.w - 1.0).max(0.0)),
            raw_local.y.clamp(0.0, (rect.size.h - 1.0).max(0.0)),
        ));
        let global = rect.loc + clamped_local;
        let world = clamped_local + camera;

        self.pointer_output = output_index;
        self.pointer_pos = clamped_local;
        self.pointer_global = global;
        self.last_absolute_pointer = None;
        pointer.set_location(world);
        self.request_redraw_output(output_index);
    }
}

impl XdgActivationHandler for EvaWM {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // Tokens are single-use and short-lived so expired or replayed tokens
        // cannot accumulate or steal focus long after the triggering action.
        const TOKEN_TTL: Duration = Duration::from_secs(5);
        self.xdg_activation_state
            .retain_tokens(|_, data| data.timestamp.elapsed() <= TOKEN_TTL);
        self.xdg_activation_state.remove_token(&token);

        if token_data.timestamp.elapsed() > TOKEN_TTL || !surface.alive() {
            tracing::debug!("ignoring stale or dead xdg activation token");
            return;
        }

        let found = self
            .outputs
            .iter()
            .enumerate()
            .find_map(|(output_index, output)| {
                output
                    .workspaces
                    .iter()
                    .enumerate()
                    .find_map(|(workspace, layout)| {
                        layout
                            .window_for_surface(&surface)
                            .map(|window| (output_index, workspace, window.clone()))
                    })
            });

        if let Some((output_index, workspace, window)) = found {
            self.activate_window_at(output_index, workspace, &window);
        }
    }
}

delegate_xdg_activation!(EvaWM);

impl XdgSystemBellHandler for EvaWM {
    fn ring(&mut self, _surface: Option<WlSurface>) {}
}

delegate_xdg_system_bell!(EvaWM);

delegate_text_input_manager!(EvaWM);
delegate_cursor_shape!(EvaWM);

// Required by `CursorShapeManagerState`; tablet input is not advertised.
impl TabletSeatHandler for EvaWM {}

delegate_viewporter!(EvaWM);
