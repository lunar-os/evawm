//! IPC snapshots, request handling, and subscriber events.

use serde_json::{Value, json};
use smithay::wayland::compositor::with_states;
use smithay::{
    desktop::Window, utils::SERIAL_COUNTER, wayland::shell::xdg::XdgToplevelSurfaceData,
};

use crate::{
    ipc::{
        IpcAction, IpcReply, IpcRequest, OutputInfo, StateSnapshot, WindowInfo, WindowRect,
        WorkspaceInfo, parse_action, version_value,
    },
    state::EvaWM,
    workspace::{Mode, Workspace},
};

impl EvaWM {
    pub(crate) fn ensure_window_id(&mut self, window: &Window) -> u64 {
        if let Some(id) = self.window_ids.get(window) {
            return *id;
        }
        let id = self.next_window_id;
        self.next_window_id += 1;
        self.window_ids.insert(window.clone(), id);
        id
    }

    pub(crate) fn forget_window_id(&mut self, window: &Window) {
        self.window_ids.remove(window);
        self.window_locations.remove(window);
        self.surface_windows.retain(|_, entry| entry != window);
        self.foreign_toplevel.remove_window(window);
    }

    fn workspace_windows_iter(workspace: &Workspace) -> impl Iterator<Item = &Window> {
        workspace.windows().chain(
            workspace
                .canvas
                .z_order
                .iter()
                .filter(|window| workspace.scroll.tile_index(window).is_none()),
        )
    }

    pub(crate) fn window_metadata(window: &Window) -> (Option<String>, Option<String>) {
        let Some(toplevel) = window.toplevel() else {
            return (None, None);
        };
        with_states(toplevel.wl_surface(), |states| {
            let Some(data) = states.data_map.get::<XdgToplevelSurfaceData>() else {
                return (None, None);
            };
            let Ok(data) = data.lock() else {
                return (None, None);
            };
            (data.title.clone(), data.app_id.clone())
        })
    }

    fn transform_name(transform: smithay::utils::Transform) -> String {
        match transform {
            smithay::utils::Transform::Normal => "normal".to_string(),
            smithay::utils::Transform::_90 => "90".to_string(),
            smithay::utils::Transform::_180 => "180".to_string(),
            smithay::utils::Transform::_270 => "270".to_string(),
            smithay::utils::Transform::Flipped => "flipped".to_string(),
            smithay::utils::Transform::Flipped90 => "flipped-90".to_string(),
            smithay::utils::Transform::Flipped180 => "flipped-180".to_string(),
            smithay::utils::Transform::Flipped270 => "flipped-270".to_string(),
        }
    }

    pub(crate) fn ipc_snapshot(&mut self) -> StateSnapshot {
        let window_capacity: usize = self
            .outputs
            .iter()
            .flat_map(|output| &output.workspaces)
            .map(|workspace| {
                workspace
                    .scroll
                    .windows()
                    .count()
                    .max(workspace.canvas.z_order.len())
            })
            .sum();
        let workspace_capacity: usize = self
            .outputs
            .iter()
            .map(|output| output.workspaces.len())
            .sum();
        let mut outputs = Vec::with_capacity(self.outputs.len());
        let mut workspaces = Vec::with_capacity(workspace_capacity);
        let mut window_infos = Vec::with_capacity(window_capacity);
        let mut focused = None;

        // Disjoint borrows let us allocate missing window IDs inline in one pass.
        let active_output = self.active_output;
        #[allow(clippy::mutable_key_type)]
        let window_ids = &mut self.window_ids;
        let next_window_id = &mut self.next_window_id;
        let mut ensure_id = |window: &Window| -> u64 {
            if let Some(&id) = window_ids.get(window) {
                return id;
            }
            let id = *next_window_id;
            *next_window_id += 1;
            window_ids.insert(window.clone(), id);
            id
        };

        for (output_index, output) in self.outputs.iter().enumerate() {
            let name = output.output.name();
            let mode = output.output.current_mode();
            let rect = output
                .global_rect()
                .unwrap_or_else(|| smithay::utils::Rectangle::from_size((0.0, 0.0).into()));
            let output_active = output_index == active_output;
            let active_workspace_id = output.workspaces[output.active_workspace].id;
            outputs.push(OutputInfo {
                name: name.clone(),
                width: rect.size.w.round() as i32,
                height: rect.size.h.round() as i32,
                refresh_mhz: mode.map(|mode| mode.refresh),
                x: rect.loc.x.round() as i32,
                y: rect.loc.y.round() as i32,
                transform: Self::transform_name(output.output.current_transform()),
                active: output_active,
                active_workspace_id,
                active_workspace_index: output.active_workspace as u32 + 1,
            });

            for (workspace_index, workspace) in output.workspaces.iter().enumerate() {
                let (workspace_active, workspace_focused) =
                    workspace_ipc_flags(output_active, workspace_index, output.active_workspace);
                let focused_window = workspace.focused_window();
                let active_window_id = focused_window.map(&mut ensure_id);
                let mut window_count = 0usize;

                for window in Self::workspace_windows_iter(workspace) {
                    window_count += 1;
                    let id = ensure_id(window);
                    let (title, app_id) = Self::window_metadata(window);
                    let is_fullscreen = workspace.fullscreen_window() == Some(window);
                    let geometry = Self::window_ipc_rect(workspace, window, is_fullscreen);
                    let window_focused = workspace_focused && focused_window == Some(window);
                    let info = WindowInfo {
                        id,
                        title,
                        app_id,
                        output: name.clone(),
                        workspace_id: workspace.id,
                        workspace_index: workspace_index as u32 + 1,
                        focused: window_focused,
                        fullscreen: is_fullscreen,
                        mode: match workspace.mode {
                            Mode::Scroll => "scroll".to_string(),
                            Mode::Canvas => "canvas".to_string(),
                        },
                        geometry,
                    };
                    if window_focused {
                        focused = Some(info.clone());
                    }
                    window_infos.push(info);
                }

                workspaces.push(WorkspaceInfo {
                    id: workspace.id,
                    index: workspace_index as u32 + 1,
                    name: None,
                    output: name.clone(),
                    active: workspace_active,
                    focused: workspace_focused,
                    mode: match workspace.mode {
                        Mode::Scroll => "scroll".to_string(),
                        Mode::Canvas => "canvas".to_string(),
                    },
                    window_count,
                    active_window_id,
                });
            }
        }

        StateSnapshot {
            outputs,
            workspaces,
            windows: window_infos,
            focused,
        }
    }

    fn window_ipc_rect(workspace: &Workspace, window: &Window, is_fullscreen: bool) -> WindowRect {
        ipc_rect_from_bounds(
            workspace.output_size(),
            workspace.content_target_rect_for_mode(window),
            window.geometry(),
            is_fullscreen,
        )
    }

    pub(crate) fn focus_workspace_by_id(&mut self, id: u64) -> bool {
        let found = self
            .outputs
            .iter()
            .enumerate()
            .find_map(|(output_index, output)| {
                output
                    .workspaces
                    .iter()
                    .position(|workspace| workspace.id == id)
                    .map(|workspace| (output_index, workspace))
            });
        let Some((output, workspace)) = found else {
            return false;
        };
        self.cancel_interaction_state();
        let previous = self.active_output;
        self.active_output = output;
        if workspace != self.active_workspace() {
            self.switch_workspace(workspace);
            if previous != output {
                self.request_redraw_output(previous);
            }
        } else {
            // The target workspace is already active on the new output;
            // switch_workspace would return without moving keyboard focus.
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            let window = self.outputs[output].workspaces[workspace]
                .focused_window()
                .cloned();
            self.focus_window(window.as_ref(), serial);
            self.request_redraw_output(previous);
            self.request_redraw_output(output);
        }
        true
    }

    pub(crate) fn focus_window_by_id(&mut self, id: u64) -> bool {
        let found = self
            .window_ids
            .iter()
            .find_map(|(window, &wid)| (wid == id).then(|| window.clone()))
            .and_then(|window| {
                self.workspace_for_window(&window)
                    .map(|(output, workspace)| (output, workspace, window))
            });
        let Some((output, workspace, window)) = found else {
            return false;
        };
        self.activate_window_at(output, workspace, &window);
        true
    }

    pub(crate) fn activate_window_at(&mut self, output: usize, workspace: usize, window: &Window) {
        self.cancel_interaction_state();
        if self.overview_open {
            self.close_overview();
        }
        let was_deferred = self.defer_ipc;
        self.defer_ipc = true;
        let previous = self.active_output;
        self.active_output = output;
        if workspace != self.active_workspace() {
            self.switch_workspace(workspace);
        }
        // `switch_workspace` may delete an empty previous workspace, shifting indices.
        let workspace = self.outputs[output].active_workspace;
        if self.outputs[output].workspaces[workspace].fullscreen_window() != Some(window)
            && self.outputs[output].workspaces[workspace].has_fullscreen()
        {
            self.leave_fullscreen_for_action();
        }
        let serial = SERIAL_COUNTER.next_serial();
        self.focus_window(Some(window), serial);
        if self.outputs[output].workspaces[workspace].mode == Mode::Canvas
            && !self.outputs[output].workspaces[workspace].has_fullscreen()
            && !self.canvas_window_visible(window)
        {
            self.canvas_center();
        }
        if previous != output {
            self.request_redraw_output(previous);
        }
        self.request_redraw_output(output);
        self.defer_ipc = was_deferred;
        if !was_deferred && self.ipc_pending {
            self.ipc_pending = false;
            self.notify_ipc();
        }
    }

    pub(crate) fn notify_ipc(&mut self) {
        if self.defer_ipc {
            self.ipc_pending = true;
            return;
        }
        self.refresh_foreign_toplevels();
        self.refresh_workspace_protocol();
        if self.ipc_subscribers.is_empty() {
            return;
        }
        let snapshot = self.ipc_snapshot();
        let line = json!({"event": "state", "data": snapshot}).to_string();
        self.ipc_subscribers
            .retain(|sender| sender.try_send(line.clone()).is_ok());
    }

    pub(crate) fn handle_ipc_request(&mut self, request: IpcRequest) {
        let IpcRequest {
            request,
            respond,
            events,
        } = request;
        let id = request.id;
        let subscribe = request.cmd == "subscribe";
        let result: anyhow::Result<Value> = (|| match request.cmd.as_str() {
            "version" => Ok(version_value(
                self.ipc_server.as_ref().map(|server| server.path()),
            )),
            "outputs" => Ok(serde_json::to_value(self.ipc_snapshot().outputs)?),
            "workspaces" => Ok(serde_json::to_value(self.ipc_snapshot().workspaces)?),
            "windows" => Ok(serde_json::to_value(self.ipc_snapshot().windows)?),
            "focused" => Ok(serde_json::to_value(self.ipc_snapshot().focused)?),
            "subscribe" => Ok(serde_json::to_value(self.ipc_snapshot())?),
            "action" => {
                let name = request
                    .action
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("missing action name"))?;
                match parse_action(name, &request.args, request.yes)? {
                    IpcAction::Action(action) => {
                        self.execute_action(action);
                        Ok(json!({"action": name}))
                    }
                    IpcAction::FocusWindow(id) => {
                        if self.focus_window_by_id(id) {
                            Ok(json!({"action": name, "window_id": id}))
                        } else {
                            anyhow::bail!("no window with id {id}")
                        }
                    }
                    IpcAction::FocusWorkspaceId(id) => {
                        if self.focus_workspace_by_id(id) {
                            Ok(json!({"action": name, "workspace_id": id}))
                        } else {
                            anyhow::bail!("no workspace with id {id}")
                        }
                    }
                }
            }
            "focus-window" => {
                let id = request
                    .args
                    .first()
                    .and_then(Value::as_u64)
                    .ok_or_else(|| anyhow::anyhow!("focus-window needs a numeric window id"))?;
                if self.focus_window_by_id(id) {
                    Ok(json!({"action": "focus-window", "window_id": id}))
                } else {
                    anyhow::bail!("no window with id {id}")
                }
            }
            other => anyhow::bail!("unknown command {other:?}"),
        })();

        let line = match result {
            Ok(data) => {
                if subscribe {
                    if let Some(events) = events
                        && respond
                            .send(IpcReply {
                                line:
                                    json!({"id": id, "ok": true, "subscribed": true, "data": data})
                                        .to_string(),
                                subscribe: true,
                            })
                            .is_ok()
                    {
                        self.ipc_subscribers.push(events);
                    }
                    return;
                }
                json!({"id": id, "ok": true, "data": data}).to_string()
            }
            Err(err) => json!({"id": id, "ok": false, "error": err.to_string()}).to_string(),
        };
        let _ = respond.send(IpcReply {
            line,
            subscribe: false,
        });
    }
}

fn workspace_ipc_flags(
    output_active: bool,
    workspace_index: usize,
    active_workspace: usize,
) -> (bool, bool) {
    let active = workspace_index == active_workspace;
    let focused = output_active && active;
    (active, focused)
}

fn ipc_rect_from_bounds(
    output_size: smithay::utils::Size<f64, smithay::utils::Logical>,
    layout_rect: Option<smithay::utils::Rectangle<f64, smithay::utils::Logical>>,
    fallback: smithay::utils::Rectangle<i32, smithay::utils::Logical>,
    is_fullscreen: bool,
) -> WindowRect {
    if is_fullscreen {
        return WindowRect {
            x: 0,
            y: 0,
            width: output_size.w.round() as i32,
            height: output_size.h.round() as i32,
        };
    }
    if let Some(rect) = layout_rect {
        return WindowRect {
            x: rect.loc.x.round() as i32,
            y: rect.loc.y.round() as i32,
            width: rect.size.w.round() as i32,
            height: rect.size.h.round() as i32,
        };
    }
    WindowRect {
        x: fallback.loc.x,
        y: fallback.loc.y,
        width: fallback.size.w,
        height: fallback.size.h,
    }
}

#[cfg(test)]
mod tests {
    use super::{ipc_rect_from_bounds, workspace_ipc_flags};
    use smithay::utils::{Point, Rectangle, Size};

    #[test]
    fn workspace_flags_distinguish_active_on_unfocused_output() {
        assert_eq!(workspace_ipc_flags(true, 0, 0), (true, true));
        assert_eq!(workspace_ipc_flags(true, 1, 0), (false, false));
        assert_eq!(workspace_ipc_flags(false, 0, 0), (true, false));
        assert_eq!(workspace_ipc_flags(false, 1, 0), (false, false));
    }

    #[test]
    fn ipc_rect_uses_layout_or_fullscreen_bounds_before_csd_fallback() {
        let output = Size::from((1920.0, 1080.0));
        let fallback = Rectangle::new(Point::from((0, 0)), Size::from((600, 400)));

        let layout = ipc_rect_from_bounds(
            output,
            Some(Rectangle::new(
                Point::from((125.4, -40.6)),
                Size::from((800.2, 500.8)),
            )),
            fallback,
            false,
        );
        assert_eq!(
            (layout.x, layout.y, layout.width, layout.height),
            (125, -41, 800, 501)
        );

        let fullscreen = ipc_rect_from_bounds(
            output,
            Some(Rectangle::new(
                Point::from((125.0, 40.0)),
                Size::from((800.0, 500.0)),
            )),
            fallback,
            true,
        );
        assert_eq!(
            (
                fullscreen.x,
                fullscreen.y,
                fullscreen.width,
                fullscreen.height
            ),
            (0, 0, 1920, 1080)
        );

        let unplaced = ipc_rect_from_bounds(output, None, fallback, false);
        assert_eq!(
            (unplaced.x, unplaced.y, unplaced.width, unplaced.height),
            (0, 0, 600, 400)
        );
    }
}
