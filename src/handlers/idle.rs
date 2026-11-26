//! Idle notification and inhibition.

use smithay::{
    delegate_idle_inhibit, delegate_idle_notify,
    desktop::{Window, WindowSurfaceType, layer_map_for_output},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{IsAlive, Rectangle},
    wayland::{
        compositor::get_parent,
        idle_inhibit::IdleInhibitHandler,
        idle_notify::{IdleNotifierHandler, IdleNotifierState},
    },
};

use crate::state::EvaWM;

impl IdleNotifierHandler for EvaWM {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.idle_notifier
    }
}

impl IdleInhibitHandler for EvaWM {
    fn inhibit(&mut self, surface: WlSurface) {
        *self.idle_inhibitors.entry(surface).or_insert(0) += 1;
        self.refresh_idle_inhibit();
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        if let Some(count) = self.idle_inhibitors.get_mut(&surface) {
            *count -= 1;
            if *count == 0 {
                self.idle_inhibitors.remove(&surface);
            }
        }
        self.refresh_idle_inhibit();
    }
}

impl EvaWM {
    /// Drop inhibitors whose surface has died, and publish only inhibitors
    /// that belong to something currently visible.
    ///
    /// A video on another workspace, or a window scrolled off-screen in
    /// canvas, must not keep the session awake.
    pub(crate) fn refresh_idle_inhibit(&mut self) {
        self.idle_inhibitors
            .retain(|surface, count| *count > 0 && surface.alive());
        let inhibited = self
            .idle_inhibitors
            .keys()
            .any(|surface| self.inhibitor_is_visible(surface));

        let changed = self.idle_notifier.is_inhibited() != inhibited;
        self.idle_notifier.set_is_inhibited(inhibited);
        if changed {
            tracing::debug!(
                inhibited,
                inhibitors = self.idle_inhibitors.len(),
                "idle inhibitor state changed"
            );
        }
    }

    fn inhibitor_is_visible(&self, surface: &WlSurface) -> bool {
        // The inhibitor is normally created on the toplevel, but resolve the
        // root so a subsurface inhibitor is treated like its window.
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

        if let Some((_, window)) = self.window_and_output_for_surface(&root) {
            return self.window_is_visible(&window);
        }

        self.outputs.iter().any(|output| {
            layer_map_for_output(&output.output)
                .layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)
                .is_some()
        })
    }

    fn window_is_visible(&self, window: &Window) -> bool {
        let Some((output_index, workspace_index)) = self.workspace_for_window(window) else {
            return false;
        };
        let Some(output) = self.outputs.get(output_index) else {
            return false;
        };
        if output.active_workspace != workspace_index {
            return false;
        }

        let workspace = &output.workspaces[workspace_index];
        if workspace.fullscreen_window() == Some(window) {
            return true;
        }

        let Some(location) = output.space.element_location(window) else {
            return false;
        };
        let size = window.geometry().size;
        if size.w <= 0 || size.h <= 0 {
            return false;
        }
        let viewport = Rectangle::new(output.camera_location(), workspace.scroll.output_size);
        Rectangle::new(location.to_f64(), size.to_f64()).overlaps(viewport)
    }

    /// Reset idle timers for real input activity.
    pub(crate) fn note_idle_activity(&mut self) {
        self.idle_notifier.notify_activity(&self.seat);
    }
}

delegate_idle_notify!(EvaWM);
delegate_idle_inhibit!(EvaWM);
