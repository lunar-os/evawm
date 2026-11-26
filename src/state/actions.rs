//! Keyboard action dispatch and layout movement transitions.

use smithay::{
    desktop::Window,
    utils::{Logical, Rectangle, SERIAL_COUNTER},
};

use crate::{
    config::Action,
    state::EvaWM,
    workspace::{Direction, Mode},
};

impl EvaWM {
    pub(crate) fn start_pending_layout_transitions(
        &mut self,
        before: &[(Window, Rectangle<f64, Logical>)],
        now: f64,
        movement: bool,
    ) {
        self.start_pending_layout_transitions_except(before, now, movement, None);
    }

    pub(crate) fn start_pending_layout_transitions_except(
        &mut self,
        before: &[(Window, Rectangle<f64, Logical>)],
        now: f64,
        movement: bool,
        exclude: Option<&Window>,
    ) {
        if self.drag.is_some() {
            return;
        }
        let mut config = if movement {
            self.config.animations.window_movement
        } else {
            self.config.animations.window_resize
        };
        if self.config.animations.off {
            config.off = true;
        }
        if config.off {
            return;
        }
        let output_index = self.active_output;
        let index = self.outputs[output_index].active_workspace;
        let workspace = &self.outputs[output_index].workspaces[index];
        let mut started = false;
        for (window, before_rect) in before {
            if exclude == Some(window) || workspace.is_fullscreen(window) {
                // The fullscreen overlay owns its own enter/exit animation;
                // layout transitions must never pull it back to its tile.
                continue;
            }
            let Some(target) = workspace.content_target_rect_for_mode(window) else {
                continue;
            };
            if (target.loc.x - before_rect.loc.x).abs() <= 1.0
                && (target.loc.y - before_rect.loc.y).abs() <= 1.0
                && (target.size.w - before_rect.size.w).abs() <= 1.0
                && (target.size.h - before_rect.size.h).abs() <= 1.0
            {
                continue;
            }
            started |= self.window_states.start_layout_transition(
                window,
                *before_rect,
                target,
                now,
                config,
            );
        }
        if started {
            self.mark_full_redraw_output(output_index);
        }
    }

    pub fn execute_action(&mut self, action: Action) {
        let was_deferred = self.defer_ipc;
        self.defer_ipc = true;
        if !was_deferred {
            self.ipc_pending = false;
        }
        self.execute_action_inner(action);
        self.defer_ipc = was_deferred;
        if !was_deferred && self.ipc_pending {
            self.ipc_pending = false;
            self.notify_ipc();
        }
    }

    fn execute_action_inner(&mut self, action: Action) {
        if !matches!(
            action,
            Action::CycleWindowNext | Action::CycleWindowPrevious
        ) {
            self.commit_mru_cycle();
        }
        let index = self.active_workspace();
        let now = self.animation_now();

        // Cross-output moves and overview toggles have their own fullscreen
        // handling, so dispatch them before the generic guards.
        match action {
            Action::ToggleOverview => {
                self.toggle_overview();
                return;
            }
            Action::OpenOverview => {
                self.open_overview();
                return;
            }
            Action::CloseOverview => {
                self.close_overview();
                return;
            }
            Action::MoveToMonitorLeft => {
                self.move_window_to_output(Direction::Left);
                return;
            }
            Action::MoveToMonitorRight => {
                self.move_window_to_output(Direction::Right);
                return;
            }
            Action::MoveToMonitorUp => {
                self.move_window_to_output(Direction::Up);
                return;
            }
            Action::MoveToMonitorDown => {
                self.move_window_to_output(Direction::Down);
                return;
            }
            _ => {}
        }

        let mode = self.active_mode();
        let was_fullscreen = self.workspaces()[index].has_fullscreen();
        if was_fullscreen {
            match action {
                Action::FullscreenWindow => {
                    self.toggle_fullscreen();
                    return;
                }
                Action::FocusMonitorLeft
                | Action::FocusMonitorRight
                | Action::FocusMonitorUp
                | Action::FocusMonitorDown
                | Action::CloseWindow
                | Action::Quit
                | Action::PowerOffMonitors
                | Action::Spawn(_)
                | Action::SpawnSh(_)
                | Action::ReloadConfig
                | Action::FocusWorkspace(_)
                | Action::MoveWindowToWorkspace(_)
                | Action::MoveColumnToWorkspace(_) => {}
                Action::ResizeWidth(_)
                | Action::ResizeHeight(_)
                | Action::MaximizeColumn
                | Action::ExpandColumnToAvailableWidth
                | Action::CenterFocused
                | Action::PanCameraLeft(_)
                | Action::PanCameraRight(_)
                | Action::PanCameraUp(_)
                | Action::PanCameraDown(_)
                | Action::HomeCanvas => return,
                Action::FocusColumnFirst
                | Action::FocusColumnLast
                | Action::MoveLeft
                | Action::MoveRight
                | Action::MoveColumnToFirst
                | Action::MoveColumnToLast
                | Action::ConsumeOrExpelLeft
                | Action::ConsumeOrExpelRight
                    if mode == Mode::Canvas =>
                {
                    return;
                }
                _ => self.leave_fullscreen_for_action(),
            }
        }

        if mode == Mode::Canvas && self.execute_canvas_action(&action) {
            return;
        }

        // Dispatch non-layout actions before capturing transition rects so
        // commands like spawn, close, or reload never disturb focus or camera.
        match action {
            Action::Quit => self.loop_signal.stop(),
            Action::PowerOffMonitors => self.power_off_monitors(),
            Action::ToggleCanvas => self.toggle_canvas(),
            Action::FocusWorkspace(workspace) => {
                let target = usize::from(workspace.saturating_sub(1));
                self.switch_workspace(target);
            }
            Action::MoveWindowToWorkspace(workspace) => {
                let target = usize::from(workspace.saturating_sub(1));
                self.move_window_to_workspace(target);
            }
            Action::MoveColumnToWorkspace(workspace) => {
                let target = usize::from(workspace.saturating_sub(1));
                self.move_column_to_workspace(target);
            }
            Action::FocusMonitorLeft => self.focus_output_direction(Direction::Left),
            Action::FocusMonitorRight => self.focus_output_direction(Direction::Right),
            Action::FocusMonitorUp => self.focus_output_direction(Direction::Up),
            Action::FocusMonitorDown => self.focus_output_direction(Direction::Down),
            Action::CenterFocused => {
                self.active_layout_mut().center();
                let camera = self.active_layout().camera_x;
                let now = self.animation_now();
                let mut config = self.config.animations.view_movement;
                if self.config.animations.off {
                    config.off = true;
                }
                self.output_state_mut()
                    .view_x
                    .set_target(camera, now, config);
                self.map_output();
                self.request_redraw();
            }
            Action::CycleWindowNext => self.cycle_mru(true),
            Action::CycleWindowPrevious => self.cycle_mru(false),
            Action::Spawn(args) => {
                if let Some((program, args)) = args.split_first() {
                    let mut command = std::process::Command::new(program);
                    command.args(args);
                    self.apply_child_env(&mut command);
                    if let Err(err) = crate::spawn::reap(command, program) {
                        tracing::warn!("failed to spawn {program}: {err}");
                    }
                }
            }
            Action::SpawnSh(command) => {
                if let Err(err) = self.spawn_shell_command(&command) {
                    tracing::warn!("failed to spawn shell command: {err}");
                }
            }
            Action::CloseWindow => {
                let window = self.workspaces()[index].focused_window().cloned();
                if let Some(window) = window
                    && let Some(toplevel) = window.toplevel()
                {
                    toplevel.send_close();
                }
            }
            Action::FullscreenWindow => self.toggle_fullscreen(),
            Action::ReloadConfig => self.reload_config(),
            // Canvas-only actions are intentional no-ops in Scroll mode;
            // overview and cross-output moves were handled above.
            Action::PanCameraLeft(_)
            | Action::PanCameraRight(_)
            | Action::PanCameraUp(_)
            | Action::PanCameraDown(_)
            | Action::HomeCanvas
            | Action::ToggleOverview
            | Action::OpenOverview
            | Action::CloseOverview
            | Action::MoveToMonitorLeft
            | Action::MoveToMonitorRight
            | Action::MoveToMonitorUp
            | Action::MoveToMonitorDown => {}
            action => {
                if matches!(
                    action,
                    Action::ResizeWidth(_)
                        | Action::ResizeHeight(_)
                        | Action::MaximizeColumn
                        | Action::ExpandColumnToAvailableWidth
                ) && let Some(window) = self.workspaces()[index].focused_window()
                    && self.window_is_fixed_size(window)
                {
                    return;
                }

                let layout_movement = matches!(
                    action,
                    Action::MoveLeft
                        | Action::MoveRight
                        | Action::MoveWindowLeft
                        | Action::MoveWindowRight
                        | Action::MoveUp
                        | Action::MoveDown
                        | Action::MoveColumnToFirst
                        | Action::MoveColumnToLast
                        | Action::ConsumeOrExpelLeft
                        | Action::ConsumeOrExpelRight
                );
                let before_rects = if !was_fullscreen && !self.config.animations.off {
                    self.capture_content_world_rects(now)
                } else {
                    Vec::new()
                };

                if matches!(action, Action::ResizeWidth(_) | Action::ResizeHeight(_))
                    && let Some(window) = self.workspaces()[index].focused_window().cloned()
                {
                    self.window_states.clear_suppression(&window);
                }

                match action {
                    Action::FocusLeft => self.active_layout_mut().focus_left(),
                    Action::FocusRight => self.active_layout_mut().focus_right(),
                    Action::FocusUp => self.active_layout_mut().focus_up(),
                    Action::FocusDown => self.active_layout_mut().focus_down(),
                    Action::FocusColumnFirst => self.active_layout_mut().focus_first(),
                    Action::FocusColumnLast => self.active_layout_mut().focus_last(),
                    Action::MoveLeft => self.active_layout_mut().move_left(),
                    Action::MoveRight => self.active_layout_mut().move_right(),
                    Action::MoveWindowLeft => self.active_layout_mut().move_window_left(),
                    Action::MoveWindowRight => self.active_layout_mut().move_window_right(),
                    Action::MoveUp => self.active_layout_mut().move_up(),
                    Action::MoveDown => self.active_layout_mut().move_down(),
                    Action::MoveColumnToFirst => self.active_layout_mut().move_first(),
                    Action::MoveColumnToLast => self.active_layout_mut().move_last(),
                    Action::ConsumeOrExpelLeft => self.active_layout_mut().consume_or_expel_left(),
                    Action::ConsumeOrExpelRight => {
                        self.active_layout_mut().consume_or_expel_right()
                    }
                    Action::ResizeWidth(fraction) => {
                        self.active_layout_mut().resize_width(fraction)
                    }
                    Action::ResizeHeight(fraction) => {
                        self.active_layout_mut().resize_height(fraction)
                    }
                    Action::MaximizeColumn => self.active_layout_mut().maximize_column(),
                    Action::ExpandColumnToAvailableWidth => {
                        self.active_layout_mut().expand_column_to_available_width()
                    }
                    _ => unreachable!("all non-layout actions returned above"),
                }

                self.apply_layout();
                if !before_rects.is_empty() && self.active_mode() != Mode::Canvas {
                    self.start_pending_layout_transitions(&before_rects, now, layout_movement);
                }
                let window = self.output_state().workspaces[index]
                    .focused_window()
                    .cloned();
                let serial = SERIAL_COUNTER.next_serial();
                self.focus_window(window.as_ref(), serial);
                self.request_redraw();
            }
        }
    }
}
