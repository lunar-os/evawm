//! `zwlr_foreign_toplevel_management_v1`: taskbar/dock window list and control.

use std::collections::HashMap;

use smithay::{
    desktop::Window,
    output::Output,
    reexports::{
        wayland_protocols_wlr::foreign_toplevel::v1::server::{
            zwlr_foreign_toplevel_handle_v1::{
                self, State as HandleState, ZwlrForeignToplevelHandleV1,
            },
            zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
            backend::GlobalId, delegate_dispatch, delegate_global_dispatch,
            protocol::wl_output::WlOutput,
        },
    },
    wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData},
};

use crate::{
    config::Action,
    state::{EvaWM, OutputState},
    workspace::{Mode, Workspace},
};

const MANAGER_VERSION: u32 = 3;
const STATE_BIT_MAXIMIZED: u8 = 1 << 0;
const STATE_BIT_ACTIVATED: u8 = 1 << 1;
const STATE_BIT_FULLSCREEN: u8 = 1 << 2;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToplevelSnapshot {
    window: Window,
    title: String,
    app_id: String,
    states: u8,
    output: Option<usize>,
}

fn state_bits(maximized: bool, activated: bool, fullscreen: bool) -> u8 {
    (u8::from(maximized) * STATE_BIT_MAXIMIZED)
        | (u8::from(activated) * STATE_BIT_ACTIVATED)
        | (u8::from(fullscreen) * STATE_BIT_FULLSCREEN)
}

fn encode_states(bits: u8) -> Vec<u8> {
    let mut buf = [0u8; 12];
    let mut len = 0;
    for (bit, state) in [
        (STATE_BIT_MAXIMIZED, HandleState::Maximized),
        (STATE_BIT_ACTIVATED, HandleState::Activated),
        (STATE_BIT_FULLSCREEN, HandleState::Fullscreen),
    ] {
        if bits & bit != 0 {
            buf[len..len + 4].copy_from_slice(&(state as u32).to_ne_bytes());
            len += 4;
        }
    }
    buf[..len].to_vec()
}

type ManagerHandle = (
    ZwlrForeignToplevelManagerV1,
    Option<ZwlrForeignToplevelHandleV1>,
);

#[derive(Debug, Default)]
pub struct ForeignToplevelState {
    _global: Option<GlobalId>,
    managers: Vec<ZwlrForeignToplevelManagerV1>,
    /// Per-window handles keyed by owning manager. `None` is a tombstone left
    /// when a client calls `Destroy` so commits never re-announce the window
    /// to that manager.
    #[allow(clippy::mutable_key_type)]
    handles: HashMap<Window, Vec<ManagerHandle>>,
    #[allow(clippy::mutable_key_type)]
    cache: HashMap<Window, ToplevelSnapshot>,
}

impl ForeignToplevelState {
    pub fn new(display: &DisplayHandle) -> Self {
        let global =
            display.create_global::<EvaWM, ZwlrForeignToplevelManagerV1, _>(MANAGER_VERSION, ());
        Self {
            _global: Some(global),
            managers: Vec::new(),
            handles: HashMap::new(),
            cache: HashMap::new(),
        }
    }

    fn prune_dead(&mut self) {
        self.managers.retain(Resource::is_alive);
        for entries in self.handles.values_mut() {
            entries.retain_mut(|(manager, handle)| {
                if !manager.is_alive() {
                    return false;
                }
                if handle.as_ref().is_some_and(|h| !h.is_alive()) {
                    *handle = None;
                }
                true
            });
        }
        self.handles.retain(|_, entries| !entries.is_empty());
    }

    fn sync_window(
        &mut self,
        dh: &DisplayHandle,
        outputs: &[OutputState],
        snapshot: &ToplevelSnapshot,
    ) {
        let entries = self.handles.entry(snapshot.window.clone()).or_default();
        let existing_len = entries.len();

        for manager in &self.managers {
            if entries[..existing_len]
                .iter()
                .any(|(existing_manager, _)| existing_manager.id() == manager.id())
            {
                continue;
            }
            let Some(client) = manager.client() else {
                continue;
            };
            let Ok(handle) = client.create_resource::<ZwlrForeignToplevelHandleV1, _, EvaWM>(
                dh,
                manager.version(),
                snapshot.window.clone(),
            ) else {
                continue;
            };
            manager.toplevel(&handle);
            send_full(&handle, snapshot, outputs, &client);
            entries.push((manager.clone(), Some(handle)));
        }

        match self.cache.get(&snapshot.window) {
            Some(old) if old == snapshot => {}
            Some(old) => {
                for (_, handle) in &entries[..existing_len] {
                    let Some(handle) = handle else {
                        continue;
                    };
                    let Some(client) = handle.client() else {
                        continue;
                    };
                    send_diff(handle, old, snapshot, outputs, &client);
                }
                self.cache.insert(snapshot.window.clone(), snapshot.clone());
            }
            None => {
                for (_, handle) in &entries[..existing_len] {
                    let Some(handle) = handle else {
                        continue;
                    };
                    let Some(client) = handle.client() else {
                        continue;
                    };
                    send_full(handle, snapshot, outputs, &client);
                }
                self.cache.insert(snapshot.window.clone(), snapshot.clone());
            }
        }
    }

    fn sync(
        &mut self,
        dh: &DisplayHandle,
        outputs: &[OutputState],
        snapshots: &[ToplevelSnapshot],
    ) {
        self.prune_dead();
        for snapshot in snapshots {
            self.sync_window(dh, outputs, snapshot);
        }

        #[allow(clippy::mutable_key_type)]
        let live: std::collections::HashSet<Window> = snapshots
            .iter()
            .map(|snapshot| snapshot.window.clone())
            .collect();
        self.handles.retain(|window, entries| {
            if live.contains(window) {
                true
            } else {
                for (_, handle) in entries {
                    if let Some(handle) = handle {
                        handle.closed();
                    }
                }
                false
            }
        });
        self.cache.retain(|window, _| live.contains(window));
    }

    pub(crate) fn remove_window(&mut self, window: &Window) {
        if let Some(entries) = self.handles.remove(window) {
            for (_, handle) in entries {
                if let Some(handle) = handle {
                    handle.closed();
                }
            }
        }
        self.cache.remove(window);
    }

    fn destroy_handle(&mut self, handle: &ZwlrForeignToplevelHandleV1) {
        for entries in self.handles.values_mut() {
            for (_, entry) in entries {
                if entry.as_ref() == Some(handle) {
                    *entry = None;
                }
            }
        }
    }
}

fn send_full(
    handle: &ZwlrForeignToplevelHandleV1,
    snapshot: &ToplevelSnapshot,
    outputs: &[OutputState],
    client: &Client,
) {
    handle.title(snapshot.title.clone());
    handle.app_id(snapshot.app_id.clone());
    if let Some(output) = snapshot.output.and_then(|index| outputs.get(index)) {
        for wl_output in output.output.client_outputs(client) {
            handle.output_enter(&wl_output);
        }
    }
    handle.state(encode_states(snapshot.states));
    handle.done();
}

fn send_diff(
    handle: &ZwlrForeignToplevelHandleV1,
    old: &ToplevelSnapshot,
    snapshot: &ToplevelSnapshot,
    outputs: &[OutputState],
    client: &Client,
) {
    if old.title != snapshot.title {
        handle.title(snapshot.title.clone());
    }
    if old.app_id != snapshot.app_id {
        handle.app_id(snapshot.app_id.clone());
    }
    if old.output != snapshot.output {
        if let Some(output) = old.output.and_then(|index| outputs.get(index)) {
            for wl_output in output.output.client_outputs(client) {
                handle.output_leave(&wl_output);
            }
        }
        if let Some(output) = snapshot.output.and_then(|index| outputs.get(index)) {
            for wl_output in output.output.client_outputs(client) {
                handle.output_enter(&wl_output);
            }
        }
    }
    if old.states != snapshot.states {
        handle.state(encode_states(snapshot.states));
    }
    handle.done();
}

fn window_is_maximized(workspace: &Workspace, window: &Window) -> bool {
    match workspace.mode {
        Mode::Scroll => workspace
            .scroll
            .column_index(window)
            .is_some_and(|index| workspace.scroll.columns[index].is_maximized()),
        Mode::Canvas => workspace
            .canvas
            .maximized
            .as_ref()
            .is_some_and(|(maximized, _)| maximized == window),
    }
}

impl EvaWM {
    fn window_state_bits(
        &self,
        output_index: usize,
        workspace_index: usize,
        workspace: &Workspace,
        window: &Window,
    ) -> u8 {
        let activated = output_index == self.active_output
            && workspace_index == self.outputs[output_index].active_workspace
            && workspace.focused_window() == Some(window);
        let maximized = window_is_maximized(workspace, window);
        let fullscreen = workspace.fullscreen_window() == Some(window);
        state_bits(maximized, activated, fullscreen)
    }

    fn toplevel_snapshot_at(
        &self,
        output_index: usize,
        workspace_index: usize,
        workspace: &Workspace,
        window: &Window,
    ) -> ToplevelSnapshot {
        let (title, app_id) = Self::window_metadata(window);
        ToplevelSnapshot {
            window: window.clone(),
            title: title.unwrap_or_default(),
            app_id: app_id.unwrap_or_default(),
            states: self.window_state_bits(output_index, workspace_index, workspace, window),
            output: Some(output_index),
        }
    }

    fn toplevel_snapshots(&self) -> Vec<ToplevelSnapshot> {
        #[allow(clippy::mutable_key_type)]
        let mut seen = std::collections::HashSet::new();
        let mut snapshots = Vec::new();
        for (output_index, output) in self.outputs.iter().enumerate() {
            for (workspace_index, workspace) in output.workspaces.iter().enumerate() {
                for window in workspace
                    .scroll
                    .windows()
                    .chain(workspace.canvas.z_order.iter())
                {
                    if !seen.insert(window.clone()) {
                        continue;
                    }
                    snapshots.push(self.toplevel_snapshot_at(
                        output_index,
                        workspace_index,
                        workspace,
                        window,
                    ));
                }
            }
        }
        snapshots
    }

    /// Publish every window's title, app-id, output, and state. Called from
    /// central focus/action state changes, so duplicates are filtered by cache.
    pub(crate) fn refresh_foreign_toplevels(&mut self) {
        if self.foreign_toplevel.managers.is_empty() {
            return;
        }
        let snapshots = self.toplevel_snapshots();
        let display = self.display_handle.clone();
        self.foreign_toplevel
            .sync(&display, &self.outputs, &snapshots);
    }

    /// Publish one window after a client commit (title/app-id/size state).
    pub(crate) fn refresh_foreign_toplevel_window(&mut self, window: &Window) {
        if self.foreign_toplevel.managers.is_empty() {
            return;
        }
        let Some((output_index, workspace_index)) = self.workspace_for_window(window) else {
            return;
        };
        let Some(workspace) = self
            .outputs
            .get(output_index)
            .and_then(|output| output.workspaces.get(workspace_index))
        else {
            return;
        };
        let states = self.window_state_bits(output_index, workspace_index, workspace, window);
        // Fast path for the commit hot loop: borrow title/app_id directly and
        // bail with zero allocations when nothing changed and every live
        // manager already tracks this window.
        let all_managers_tracked =
            self.foreign_toplevel
                .handles
                .get(window)
                .is_some_and(|entries| {
                    self.foreign_toplevel.managers.iter().all(|manager| {
                        !manager.is_alive()
                            || entries
                                .iter()
                                .any(|(existing, _)| existing.id() == manager.id())
                    })
                });
        if all_managers_tracked
            && let Some(cached) = self.foreign_toplevel.cache.get(window)
            && cached.output == Some(output_index)
            && cached.states == states
            && window.toplevel().is_some_and(|toplevel| {
                with_states(toplevel.wl_surface(), |surface_states| {
                    surface_states
                        .data_map
                        .get::<XdgToplevelSurfaceData>()
                        .and_then(|data| data.lock().ok())
                        .is_some_and(|data| {
                            data.title.as_deref().unwrap_or("") == cached.title
                                && data.app_id.as_deref().unwrap_or("") == cached.app_id
                        })
                })
            })
        {
            return;
        }

        let snapshot = self.toplevel_snapshot_at(output_index, workspace_index, workspace, window);
        let display = self.display_handle.clone();
        self.foreign_toplevel.prune_dead();
        self.foreign_toplevel
            .sync_window(&display, &self.outputs, &snapshot);
    }

    pub(crate) fn foreign_toplevel_output_bound(&mut self, output: &Output, wl_output: &WlOutput) {
        if self.foreign_toplevel.managers.is_empty() {
            return;
        }
        let Some(output_index) = self
            .outputs
            .iter()
            .position(|candidate| candidate.output == *output)
        else {
            return;
        };
        let Some(client) = wl_output.client() else {
            return;
        };
        for (window, entries) in &self.foreign_toplevel.handles {
            if self
                .foreign_toplevel
                .cache
                .get(window)
                .and_then(|snapshot| snapshot.output)
                != Some(output_index)
            {
                continue;
            }
            for (_, handle) in entries {
                if let Some(handle) = handle
                    && handle.is_alive()
                    && handle.client().as_ref() == Some(&client)
                {
                    handle.output_enter(wl_output);
                    handle.done();
                }
            }
        }
    }

    fn focus_foreign_window(&mut self, window: &Window) -> bool {
        let Some((output, workspace)) = self.workspace_for_window(window) else {
            return false;
        };
        self.activate_window_at(output, workspace, window);
        true
    }

    fn set_foreign_fullscreen(&mut self, window: &Window, fullscreen: bool) {
        if !self.focus_foreign_window(window) {
            return;
        }
        self.set_window_fullscreen(window, fullscreen);
    }

    fn set_foreign_maximized(&mut self, window: &Window, maximized: bool) {
        if !self.focus_foreign_window(window) {
            return;
        }
        let Some((output, workspace)) = self.workspace_for_window(window) else {
            return;
        };
        let current = window_is_maximized(&self.outputs[output].workspaces[workspace], window);
        if current != maximized {
            self.execute_action(Action::MaximizeColumn);
        }
    }
}

impl GlobalDispatch<ZwlrForeignToplevelManagerV1, (), EvaWM> for ForeignToplevelState {
    fn bind(
        state: &mut EvaWM,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, EvaWM>,
    ) {
        let manager = data_init.init(resource, ());
        state.foreign_toplevel.managers.push(manager);
        state.refresh_foreign_toplevels();
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, (), EvaWM> for ForeignToplevelState {
    fn request(
        state: &mut EvaWM,
        _client: &Client,
        manager: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, EvaWM>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Request::Stop = request {
            manager.finished();
            state
                .foreign_toplevel
                .managers
                .retain(|entry| entry.id() != manager.id());
            for handles in state.foreign_toplevel.handles.values_mut() {
                handles.retain(|(entry_manager, _)| entry_manager.id() != manager.id());
            }
            state
                .foreign_toplevel
                .handles
                .retain(|_, handles| !handles.is_empty());
        }
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, Window, EvaWM> for ForeignToplevelState {
    fn request(
        state: &mut EvaWM,
        _client: &Client,
        handle: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        window: &Window,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, EvaWM>,
    ) {
        match request {
            zwlr_foreign_toplevel_handle_v1::Request::Activate { seat: _ } => {
                state.focus_foreign_window(window);
            }
            zwlr_foreign_toplevel_handle_v1::Request::Close => {
                if let Some(toplevel) = window.toplevel() {
                    toplevel.send_close();
                }
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetMaximized => {
                state.set_foreign_maximized(window, true);
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMaximized => {
                state.set_foreign_maximized(window, false);
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetFullscreen { output: _ } => {
                state.set_foreign_fullscreen(window, true);
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetFullscreen => {
                state.set_foreign_fullscreen(window, false);
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetMinimized
            | zwlr_foreign_toplevel_handle_v1::Request::UnsetMinimized
            | zwlr_foreign_toplevel_handle_v1::Request::SetRectangle { .. } => {}
            zwlr_foreign_toplevel_handle_v1::Request::Destroy => {
                state.foreign_toplevel.destroy_handle(handle);
            }
            _ => {}
        }
    }
}

delegate_global_dispatch!(EvaWM: [ZwlrForeignToplevelManagerV1: ()] => ForeignToplevelState);
delegate_dispatch!(EvaWM: [ZwlrForeignToplevelManagerV1: ()] => ForeignToplevelState);
delegate_dispatch!(EvaWM: [ZwlrForeignToplevelHandleV1: Window] => ForeignToplevelState);

#[cfg(test)]
mod tests {
    use super::{HandleState, encode_states, state_bits};

    fn decode(states: &[u8]) -> Vec<u32> {
        states
            .as_chunks::<4>()
            .0
            .iter()
            .map(|state| u32::from_ne_bytes(*state))
            .collect()
    }

    #[test]
    fn toplevel_states_follow_protocol_order_and_omit_absent_bits() {
        assert_eq!(
            decode(&encode_states(state_bits(false, false, false))),
            Vec::<u32>::new()
        );
        assert_eq!(
            decode(&encode_states(state_bits(true, false, false))),
            vec![HandleState::Maximized as u32]
        );
        assert_eq!(
            decode(&encode_states(state_bits(true, true, true))),
            vec![
                HandleState::Maximized as u32,
                HandleState::Activated as u32,
                HandleState::Fullscreen as u32,
            ]
        );
    }
}
