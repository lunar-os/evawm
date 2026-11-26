use crate::{handlers::xdg_shell, state::ClientState, state::EvaWM};
use smithay::{
    backend::renderer::utils::{on_commit_buffer_handler, with_renderer_surface_state},
    delegate_compositor, delegate_shm,
    reexports::{
        calloop::Interest,
        wayland_server::{
            Client, Resource,
            protocol::{wl_buffer, wl_surface::WlSurface},
        },
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SurfaceAttributes, add_blocker, add_pre_commit_hook, get_parent, is_sync_subsurface,
            with_states,
        },
        dmabuf::get_dmabuf,
        shm::{ShmHandler, ShmState},
    },
};

pub(crate) fn is_mapped(surface: &WlSurface) -> bool {
    with_renderer_surface_state(surface, |state| state.buffer().is_some()).unwrap_or(false)
}

impl CompositorHandler for EvaWM {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        // Wait for a client DMABUF's implicit write fence before applying its
        // commit so the compositor never samples an in-flight GPU render or
        // releases the previous buffer prematurely.
        add_pre_commit_hook::<Self, _>(surface, |state, _dh, surface| {
            let maybe_dmabuf = with_states(surface, |states| {
                states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .pending()
                    .buffer
                    .as_ref()
                    .and_then(|assignment| match assignment {
                        BufferAssignment::NewBuffer(buffer) => get_dmabuf(buffer).cloned().ok(),
                        _ => None,
                    })
            });
            if let Some(dmabuf) = maybe_dmabuf
                && let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ)
                && let Some(client) = surface.client()
            {
                let res = state.handle.insert_source(source, move |_, _, state| {
                    let dh = state.display_handle.clone();
                    state
                        .client_compositor_state(&client)
                        .blocker_cleared(state, &dh);
                    Ok(())
                });
                if res.is_ok() {
                    add_blocker(surface, blocker);
                }
            }
        });
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        if is_sync_subsurface(surface) {
            return;
        }

        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

        if self
            .dnd_icon
            .as_ref()
            .is_some_and(|icon| icon.surface == root)
        {
            self.request_redraw_output(self.pointer_output);
            return;
        }

        if surface == &root && self.unmapped_windows.contains_key(&root) {
            if is_mapped(&root) {
                if let Some(unmapped) = self.unmapped_windows.remove(&root) {
                    unmapped.window.on_commit();
                    self.map_toplevel_window(unmapped.window, unmapped.wants_fullscreen);
                }
            } else {
                self.send_initial_configure_if_needed(&root);
            }
            xdg_shell::handle_popup_commit(&mut self.popups, surface);
            return;
        }

        let window = self.surface_windows.get(&root).cloned().or_else(|| {
            self.outputs
                .iter()
                .flat_map(|output| &output.workspaces)
                .find_map(|workspace| workspace.window_for_surface(&root).cloned())
        });
        if let Some(window) = window {
            if surface == &root && !is_mapped(&root) && self.workspace_for_window(&window).is_some()
            {
                window.on_commit();
                self.unmap_mapped_toplevel(window);
                xdg_shell::handle_popup_commit(&mut self.popups, surface);
                return;
            }

            window.on_commit();
            self.on_window_commit(&window);
            xdg_shell::handle_popup_commit(&mut self.popups, surface);
            return;
        }

        xdg_shell::handle_popup_commit(&mut self.popups, surface);
        if let Some(output_index) = crate::handlers::layer::handle_commit(self, &root) {
            self.update_working_area_output(output_index);
        } else {
            // Cursor, popup, and other non-window surface commits belong to
            // the output under the pointer.
            self.request_redraw_output(self.pointer_output);
        }
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        if self.idle_inhibitors.remove(surface).is_some() {
            self.refresh_idle_inhibit();
        }
        self.remove_unmapped_toplevel(surface);
    }
}

impl BufferHandler for EvaWM {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for EvaWM {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

delegate_compositor!(EvaWM);
delegate_shm!(EvaWM);
