//! Client-initiated Wayland drag-and-drop.
//!
//! The data-device protocol delivers the source and target negotiation, while
//! Smithay's `DnDGrab` owns the pointer grab and target enter/motion/leave/drop
//! routing. EvaWM stores the optional drag icon so the renderer can draw it at
//! the pointer.

use smithay::{
    input::{
        Seat,
        dnd::{DnDGrab, DndGrabHandler, DndTarget, GrabType, Source},
        pointer::Focus,
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{IsAlive, Logical, Point, Serial},
    wayland::selection::data_device::WaylandDndGrabHandler,
};

use crate::state::EvaWM;

#[derive(Debug, Clone)]
pub struct DndIcon {
    pub surface: WlSurface,
    pub offset: Point<i32, Logical>,
}

impl WaylandDndGrabHandler for EvaWM {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        type_: GrabType,
    ) {
        self.dnd_icon = icon
            .filter(|surface| surface.alive())
            .map(|surface| DndIcon {
                surface,
                offset: Point::from((0, 0)),
            });

        match type_ {
            GrabType::Pointer => {
                let Some(pointer) = seat.get_pointer() else {
                    source.cancel();
                    self.dnd_icon = None;
                    return;
                };
                let Some(start_data) = pointer.grab_start_data() else {
                    source.cancel();
                    self.dnd_icon = None;
                    return;
                };

                self.dnd_active = true;
                pointer.set_grab(
                    self,
                    DnDGrab::new_pointer(&self.display_handle, start_data, source, seat),
                    serial,
                    Focus::Keep,
                );
                self.mark_full_redraw_output(self.pointer_output);
            }
            GrabType::Touch => {
                // EvaWM does not route touch input yet; cancel rather than
                // leaving the client waiting for a grab that never moves.
                source.cancel();
                self.dnd_icon = None;
            }
        }
    }
}

impl DndGrabHandler for EvaWM {
    fn dropped(
        &mut self,
        _target: Option<DndTarget<'_, Self>>,
        _validated: bool,
        _seat: Seat<Self>,
        _location: Point<f64, Logical>,
    ) {
        self.dnd_active = false;
        self.dnd_icon = None;
        self.mark_full_redraw_output(self.pointer_output);
    }

    fn cancelled(&mut self, _seat: Seat<Self>, _location: Point<f64, Logical>) {
        self.dnd_active = false;
        self.dnd_icon = None;
        self.mark_full_redraw_output(self.pointer_output);
    }
}
