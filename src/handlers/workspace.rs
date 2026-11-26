//! `ext-workspace-v1`: one dynamic workspace group per output.

use std::collections::{HashMap, HashSet};

use smithay::{
    output::Output,
    reexports::{
        wayland_protocols::ext::workspace::v1::server::{
            ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1, GroupCapabilities},
            ext_workspace_handle_v1::{
                self, ExtWorkspaceHandleV1, State as WorkspaceState, WorkspaceCapabilities,
            },
            ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
            backend::GlobalId, delegate_dispatch, delegate_global_dispatch,
            protocol::wl_output::WlOutput,
        },
    },
};

use crate::state::{EvaWM, OutputState};

const MANAGER_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceSnapshot {
    id: u64,
    output: usize,
    name: String,
    coordinates: Vec<u8>,
    state: WorkspaceState,
}

#[derive(Debug, Clone, Copy)]
pub struct GroupHandleData {
    manager_id: u64,
    output_index: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct WorkspaceHandleData {
    manager_id: u64,
    workspace_id: u64,
}

#[derive(Debug)]
struct GroupHandleEntry {
    handle: ExtWorkspaceGroupHandleV1,
    sent_outputs: Vec<WlOutput>,
    sent_workspaces: HashSet<u64>,
}

#[derive(Debug, Default)]
struct GroupEntry {
    /// Keyed by owning `ExtWorkspaceManagerV1` id so multiple managers from
    /// the same client stay completely isolated. `None` is a tombstone left
    /// when the client destroys its handle so sync never re-announces it.
    handles: HashMap<u64, Option<GroupHandleEntry>>,
}

#[derive(Debug)]
struct WorkspaceEntry {
    /// Keyed by owning `ExtWorkspaceManagerV1` id. `None` is a client-destroy
    /// tombstone.
    handles: HashMap<u64, Option<ExtWorkspaceHandleV1>>,
    cache: WorkspaceSnapshot,
}

#[derive(Debug, Default)]
pub struct WorkspaceProtocolState {
    _global: Option<GlobalId>,
    next_manager_id: u64,
    managers: HashMap<u64, ExtWorkspaceManagerV1>,
    groups: HashMap<usize, GroupEntry>,
    workspaces: HashMap<u64, WorkspaceEntry>,
    /// Double-buffered `activate` requests queued per manager until `commit`.
    pending_activations: HashMap<u64, Vec<u64>>,
}

impl WorkspaceProtocolState {
    pub fn new(display: &DisplayHandle) -> Self {
        let global = display.create_global::<EvaWM, ExtWorkspaceManagerV1, _>(MANAGER_VERSION, ());
        Self {
            _global: Some(global),
            next_manager_id: 1,
            managers: HashMap::new(),
            groups: HashMap::new(),
            workspaces: HashMap::new(),
            pending_activations: HashMap::new(),
        }
    }

    fn sync_workspaces(
        &mut self,
        dh: &DisplayHandle,
        snapshots: &[WorkspaceSnapshot],
        dirty_managers: &mut HashSet<u64>,
    ) {
        for snapshot in snapshots {
            let mut added = Vec::new();

            for (&manager_id, manager) in &self.managers {
                if self
                    .workspaces
                    .get(&snapshot.id)
                    .is_some_and(|entry| entry.handles.contains_key(&manager_id))
                {
                    continue;
                }
                let Some(client) = manager.client() else {
                    continue;
                };
                let Ok(handle) = client.create_resource::<ExtWorkspaceHandleV1, _, EvaWM>(
                    dh,
                    manager.version(),
                    WorkspaceHandleData {
                        manager_id,
                        workspace_id: snapshot.id,
                    },
                ) else {
                    continue;
                };
                manager.workspace(&handle);
                handle.id(snapshot.id.to_string());
                handle.name(snapshot.name.clone());
                handle.coordinates(snapshot.coordinates.clone());
                handle.state(snapshot.state);
                handle.capabilities(WorkspaceCapabilities::Activate);
                dirty_managers.insert(manager_id);
                added.push((manager_id, Some(handle)));
            }

            if let Some(entry) = self.workspaces.get_mut(&snapshot.id) {
                let name_changed = entry.cache.name != snapshot.name;
                let coords_changed = entry.cache.coordinates != snapshot.coordinates;
                let state_changed = entry.cache.state != snapshot.state;
                if name_changed || coords_changed || state_changed {
                    for (&manager_id, handle) in &entry.handles {
                        let Some(handle) = handle else {
                            continue;
                        };
                        if name_changed {
                            handle.name(snapshot.name.clone());
                        }
                        if coords_changed {
                            handle.coordinates(snapshot.coordinates.clone());
                        }
                        if state_changed {
                            handle.state(snapshot.state);
                        }
                        dirty_managers.insert(manager_id);
                    }
                }
                entry.cache = snapshot.clone();
                entry.handles.extend(added);
            } else {
                self.workspaces.insert(
                    snapshot.id,
                    WorkspaceEntry {
                        handles: added.into_iter().collect(),
                        cache: snapshot.clone(),
                    },
                );
            }
        }
    }

    fn cleanup_workspaces(
        &mut self,
        snapshots: &[WorkspaceSnapshot],
        dirty_managers: &mut HashSet<u64>,
    ) {
        let live: HashSet<u64> = snapshots.iter().map(|snapshot| snapshot.id).collect();
        self.workspaces.retain(|id, entry| {
            if live.contains(id) {
                true
            } else {
                for (&manager_id, handle) in &entry.handles {
                    if let Some(handle) = handle {
                        handle.removed();
                        dirty_managers.insert(manager_id);
                    }
                }
                false
            }
        });
    }

    fn sync_groups(
        &mut self,
        dh: &DisplayHandle,
        outputs: &[OutputState],
        snapshots: &[WorkspaceSnapshot],
        dirty_managers: &mut HashSet<u64>,
    ) {
        let mut groups: HashMap<usize, Vec<&WorkspaceSnapshot>> = HashMap::new();
        for snapshot in snapshots {
            groups.entry(snapshot.output).or_default().push(snapshot);
        }

        for (output_index, output) in outputs.iter().enumerate() {
            let Some(workspace_snapshots) = groups.get(&output_index) else {
                continue;
            };
            let entry = self.groups.entry(output_index).or_default();

            for (&manager_id, manager) in &self.managers {
                if entry.handles.contains_key(&manager_id) {
                    continue;
                }
                let Some(client) = manager.client() else {
                    continue;
                };
                let Ok(handle) = client.create_resource::<ExtWorkspaceGroupHandleV1, _, EvaWM>(
                    dh,
                    manager.version(),
                    GroupHandleData {
                        manager_id,
                        output_index,
                    },
                ) else {
                    continue;
                };
                manager.workspace_group(&handle);
                handle.capabilities(GroupCapabilities::empty());
                let mut sent_outputs = Vec::new();
                for wl_output in output.output.client_outputs(&client) {
                    handle.output_enter(&wl_output);
                    sent_outputs.push(wl_output);
                }
                let mut sent_workspaces = HashSet::new();
                for snapshot in workspace_snapshots {
                    if let Some(workspace) = self.workspaces.get(&snapshot.id)
                        && let Some(Some(workspace_handle)) = workspace.handles.get(&manager_id)
                    {
                        handle.workspace_enter(workspace_handle);
                        sent_workspaces.insert(snapshot.id);
                    }
                }
                dirty_managers.insert(manager_id);
                entry.handles.insert(
                    manager_id,
                    Some(GroupHandleEntry {
                        handle,
                        sent_outputs,
                        sent_workspaces,
                    }),
                );
            }

            let output_live: HashSet<u64> = workspace_snapshots
                .iter()
                .map(|snapshot| snapshot.id)
                .collect();

            for (&manager_id, group_handle) in &mut entry.handles {
                let Some(group_handle) = group_handle else {
                    continue;
                };
                group_handle.sent_outputs.retain(Resource::is_alive);
                if let Some(client) = group_handle.handle.client() {
                    for wl_output in output.output.client_outputs(&client) {
                        if !group_handle.sent_outputs.contains(&wl_output) {
                            group_handle.handle.output_enter(&wl_output);
                            group_handle.sent_outputs.push(wl_output);
                            dirty_managers.insert(manager_id);
                        }
                    }
                }

                group_handle.sent_workspaces.retain(|id| {
                    if output_live.contains(id) {
                        return true;
                    }
                    if let Some(workspace) = self.workspaces.get(id)
                        && let Some(Some(workspace_handle)) = workspace.handles.get(&manager_id)
                    {
                        group_handle.handle.workspace_leave(workspace_handle);
                        dirty_managers.insert(manager_id);
                    }
                    false
                });

                for snapshot in workspace_snapshots {
                    if group_handle.sent_workspaces.contains(&snapshot.id) {
                        continue;
                    }
                    if let Some(workspace) = self.workspaces.get(&snapshot.id)
                        && let Some(Some(workspace_handle)) = workspace.handles.get(&manager_id)
                    {
                        group_handle.handle.workspace_enter(workspace_handle);
                        group_handle.sent_workspaces.insert(snapshot.id);
                        dirty_managers.insert(manager_id);
                    }
                }
            }
        }

        let live: HashSet<usize> = outputs
            .iter()
            .enumerate()
            .filter(|(index, _)| groups.contains_key(index))
            .map(|(index, _)| index)
            .collect();
        self.groups.retain(|index, entry| {
            if live.contains(index) {
                true
            } else {
                for (&manager_id, group_handle) in &entry.handles {
                    if let Some(group_handle) = group_handle {
                        group_handle.handle.removed();
                        dirty_managers.insert(manager_id);
                    }
                }
                false
            }
        });
    }

    fn sync(
        &mut self,
        dh: &DisplayHandle,
        outputs: &[OutputState],
        snapshots: &[WorkspaceSnapshot],
    ) {
        self.managers.retain(|_, manager| manager.is_alive());
        self.pending_activations
            .retain(|id, _| self.managers.contains_key(id));
        for entry in self.groups.values_mut() {
            entry.handles.retain(|id, _| self.managers.contains_key(id));
            for slot in entry.handles.values_mut() {
                if slot.as_ref().is_some_and(|group| !group.handle.is_alive()) {
                    *slot = None;
                }
            }
        }
        for entry in self.workspaces.values_mut() {
            entry.handles.retain(|id, _| self.managers.contains_key(id));
            for slot in entry.handles.values_mut() {
                if slot.as_ref().is_some_and(|handle| !handle.is_alive()) {
                    *slot = None;
                }
            }
        }

        let mut dirty_managers = HashSet::new();
        self.sync_workspaces(dh, snapshots, &mut dirty_managers);
        self.sync_groups(dh, outputs, snapshots, &mut dirty_managers);
        self.cleanup_workspaces(snapshots, &mut dirty_managers);

        for (&manager_id, manager) in &self.managers {
            if dirty_managers.contains(&manager_id) {
                manager.done();
            }
        }
    }

    fn destroy_workspace_handle(
        &mut self,
        handle: &ExtWorkspaceHandleV1,
        data: &WorkspaceHandleData,
    ) {
        if let Some(entry) = self.workspaces.get_mut(&data.workspace_id)
            && let Some(slot) = entry.handles.get_mut(&data.manager_id)
            && slot.as_ref() == Some(handle)
        {
            *slot = None;
        }
    }

    fn destroy_group_handle(&mut self, handle: &ExtWorkspaceGroupHandleV1, data: &GroupHandleData) {
        if let Some(entry) = self.groups.get_mut(&data.output_index)
            && let Some(slot) = entry.handles.get_mut(&data.manager_id)
            && slot.as_ref().is_some_and(|group| &group.handle == handle)
        {
            *slot = None;
        }
    }
}

fn workspace_name(index: usize) -> String {
    format!("{}", index + 1)
}

fn workspace_coordinates(index: usize) -> Vec<u8> {
    (index as u32).to_ne_bytes().to_vec()
}

fn workspace_state(active: bool) -> WorkspaceState {
    if active {
        WorkspaceState::Active
    } else {
        WorkspaceState::empty()
    }
}

impl EvaWM {
    fn workspace_snapshots(&self) -> Vec<WorkspaceSnapshot> {
        let mut snapshots = Vec::new();
        for (output_index, output) in self.outputs.iter().enumerate() {
            for (workspace_index, workspace) in output.workspaces.iter().enumerate() {
                snapshots.push(WorkspaceSnapshot {
                    id: workspace.id,
                    output: output_index,
                    name: workspace_name(workspace_index),
                    coordinates: workspace_coordinates(workspace_index),
                    state: workspace_state(workspace_index == output.active_workspace),
                });
            }
        }
        snapshots
    }

    pub(crate) fn refresh_workspace_protocol(&mut self) {
        if self.workspace_protocol.managers.is_empty() {
            return;
        }
        let snapshots = self.workspace_snapshots();
        let display = self.display_handle.clone();
        self.workspace_protocol
            .sync(&display, &self.outputs, &snapshots);
    }

    pub(crate) fn workspace_protocol_output_bound(
        &mut self,
        output: &Output,
        wl_output: &WlOutput,
    ) {
        if self.workspace_protocol.managers.is_empty() {
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
        let Some(group) = self.workspace_protocol.groups.get_mut(&output_index) else {
            return;
        };
        for (&manager_id, slot) in &mut group.handles {
            let Some(group_handle) = slot else {
                continue;
            };
            if !group_handle.handle.is_alive()
                || group_handle.handle.client().as_ref() != Some(&client)
            {
                continue;
            }
            group_handle.sent_outputs.retain(Resource::is_alive);
            if !group_handle.sent_outputs.contains(wl_output) {
                group_handle.handle.output_enter(wl_output);
                group_handle.sent_outputs.push(wl_output.clone());
                if let Some(manager) = self.workspace_protocol.managers.get(&manager_id)
                    && manager.is_alive()
                {
                    manager.done();
                }
            }
        }
    }
}

impl GlobalDispatch<ExtWorkspaceManagerV1, (), EvaWM> for WorkspaceProtocolState {
    fn bind(
        state: &mut EvaWM,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, EvaWM>,
    ) {
        let manager_id = state.workspace_protocol.next_manager_id;
        state.workspace_protocol.next_manager_id = manager_id.wrapping_add(1).max(1);
        let manager = data_init.init(resource, manager_id);
        state
            .workspace_protocol
            .managers
            .insert(manager_id, manager);
        state.refresh_workspace_protocol();
    }
}

impl Dispatch<ExtWorkspaceManagerV1, u64, EvaWM> for WorkspaceProtocolState {
    fn request(
        state: &mut EvaWM,
        _client: &Client,
        manager: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        &manager_id: &u64,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, EvaWM>,
    ) {
        match request {
            ext_workspace_manager_v1::Request::Commit => {
                if let Some(activations) = state
                    .workspace_protocol
                    .pending_activations
                    .remove(&manager_id)
                {
                    for workspace_id in activations {
                        state.focus_workspace_by_id(workspace_id);
                    }
                }
            }
            ext_workspace_manager_v1::Request::Stop => {
                manager.finished();
                state
                    .workspace_protocol
                    .pending_activations
                    .remove(&manager_id);
                state.workspace_protocol.managers.remove(&manager_id);
                for entry in state.workspace_protocol.groups.values_mut() {
                    entry.handles.remove(&manager_id);
                }
                for entry in state.workspace_protocol.workspaces.values_mut() {
                    entry.handles.remove(&manager_id);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, GroupHandleData, EvaWM> for WorkspaceProtocolState {
    fn request(
        state: &mut EvaWM,
        _client: &Client,
        handle: &ExtWorkspaceGroupHandleV1,
        request: ext_workspace_group_handle_v1::Request,
        data: &GroupHandleData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, EvaWM>,
    ) {
        match request {
            ext_workspace_group_handle_v1::Request::CreateWorkspace { .. } => {}
            ext_workspace_group_handle_v1::Request::Destroy => {
                state.workspace_protocol.destroy_group_handle(handle, data);
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceHandleV1, WorkspaceHandleData, EvaWM> for WorkspaceProtocolState {
    fn request(
        state: &mut EvaWM,
        _client: &Client,
        handle: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        data: &WorkspaceHandleData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, EvaWM>,
    ) {
        match request {
            ext_workspace_handle_v1::Request::Activate => {
                if !state
                    .workspace_protocol
                    .managers
                    .contains_key(&data.manager_id)
                {
                    return;
                }
                let queue = state
                    .workspace_protocol
                    .pending_activations
                    .entry(data.manager_id)
                    .or_default();
                if !queue.contains(&data.workspace_id) {
                    queue.push(data.workspace_id);
                }
            }
            ext_workspace_handle_v1::Request::Deactivate
            | ext_workspace_handle_v1::Request::Assign { .. }
            | ext_workspace_handle_v1::Request::Remove => {}
            ext_workspace_handle_v1::Request::Destroy => {
                state
                    .workspace_protocol
                    .destroy_workspace_handle(handle, data);
            }
            _ => {}
        }
    }
}

delegate_global_dispatch!(EvaWM: [ExtWorkspaceManagerV1: ()] => WorkspaceProtocolState);
delegate_dispatch!(EvaWM: [ExtWorkspaceManagerV1: u64] => WorkspaceProtocolState);
delegate_dispatch!(EvaWM: [ExtWorkspaceGroupHandleV1: GroupHandleData] => WorkspaceProtocolState);
delegate_dispatch!(EvaWM: [ExtWorkspaceHandleV1: WorkspaceHandleData] => WorkspaceProtocolState);

#[cfg(test)]
mod tests {
    use super::{WorkspaceState, workspace_coordinates, workspace_name, workspace_state};

    #[test]
    fn workspace_snapshots_use_one_based_names_and_native_coordinates() {
        assert_eq!(workspace_name(0), "1");
        assert_eq!(workspace_name(4), "5");
        assert_eq!(workspace_coordinates(2), 2u32.to_ne_bytes().to_vec());
        assert_eq!(workspace_state(true), WorkspaceState::Active);
        assert_eq!(workspace_state(false), WorkspaceState::empty());
    }
}
