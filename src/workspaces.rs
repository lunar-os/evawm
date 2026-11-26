//! Workspace switching and window/column moves between workspaces and outputs.

use smithay::{
    desktop::Window,
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Size},
};

use crate::{
    layout::Column,
    state::EvaWM,
    workspace::{CanvasRemovedWindow, Direction, Mode, Workspace, canvas_pixel_size},
};

struct DetachedWindow {
    column: Column,
    canvas: Option<CanvasRemovedWindow>,
    was_fullscreen: bool,
}

fn place_transferred_canvas_group(
    target: &mut Workspace,
    items: impl IntoIterator<Item = (Window, Option<CanvasRemovedWindow>)>,
) {
    let mut unseeded = Vec::new();
    for (window, removed) in items {
        if let Some(removed) = removed {
            target
                .canvas
                .insert_at(removed.window, removed.rect, removed.z_index);
        } else if target.mode == Mode::Canvas {
            let scroll_rect = target
                .scroll
                .content_target_rect(&window)
                .unwrap_or_else(|| Rectangle::from_size(Size::from((800.0, 600.0))));
            unseeded.push((window, scroll_rect));
        }
    }
    if unseeded.is_empty() {
        return;
    }
    // Seed unseeded Scroll windows as one preserved group centered in the
    // target Canvas view so stacked tiles keep their relative positions.
    let area = target.scroll.working_area;
    let camera = target.canvas.seed_camera(area);
    let offset = crate::workspace::canvas_seed_group_offset(
        unseeded.iter().map(|(_, rect)| rect),
        area,
        camera,
    );
    for (window, scroll_rect) in unseeded {
        let rect = Rectangle::new(scroll_rect.loc + offset, scroll_rect.size);
        target.canvas.insert(window, rect);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MovePreservation {
    scroll_maximize: bool,
    canvas_maximize: bool,
}

/// Decide which maximize state follows a window to another output. Fullscreen
/// is transferred separately as one workspace-level overlay.
fn move_preservation(
    source_mode: Mode,
    scroll_maximized: bool,
    canvas_maximized: bool,
) -> MovePreservation {
    MovePreservation {
        scroll_maximize: scroll_maximized || (source_mode == Mode::Canvas && canvas_maximized),
        canvas_maximize: canvas_maximized || (source_mode == Mode::Scroll && scroll_maximized),
    }
}

impl EvaWM {
    pub fn switch_workspace(&mut self, index: usize) {
        self.cancel_interaction_state();
        let index = index.min(self.workspaces().len().saturating_sub(1));
        if index == self.active_workspace() {
            return;
        }

        // Workspaces are independent virtual desktops: unmap the old one,
        // apply the new one, snap the camera, then focus. No two workspaces are
        // mapped at once and there is no workspace animation.
        let old_windows: Vec<Window> = {
            let output = &mut self.outputs[self.active_output];
            let old = output.active_workspace;
            if output.workspaces[old].mode == Mode::Canvas {
                output.workspaces[old].canvas.last_camera =
                    Some(Point::from((output.view_x.target, output.view_y.target)));
            }
            let old_windows: Vec<Window> = output.workspaces[old].windows().cloned().collect();
            for window in &old_windows {
                window.set_activated(false);
                if let Some(toplevel) = window.toplevel() {
                    toplevel.send_pending_configure();
                }
                output.space.unmap_elem(window);
            }

            output.active_workspace = index;
            output.workspaces[index].apply(&mut output.space);
            let camera = output.workspaces[index].camera_target();
            output.view_x.snap(camera.x);
            output.view_y.snap(camera.y);
            output.map_output();

            // Leaving an empty workspace deletes it, matching niri's dynamic
            // workspaces. The target stays active even if it is empty.
            output.clean_up_workspaces();
            old_windows
        };
        self.rebuild_window_locations();

        // Do not leave hidden open/resize/layout animations consuming frames.
        for window in &old_windows {
            self.window_states.settle_transitions(window);
        }

        let active = self.active_workspace();
        if self.overview_open && self.workspaces()[active].has_fullscreen() {
            self.set_fullscreen_window_on_output(self.active_output, None, false);
        }
        tracing::debug!(workspace = active + 1, "workspace switched");

        let serial = SERIAL_COUNTER.next_serial();
        let window = self.workspaces()[active].focused_window().cloned();
        self.focus_window(window.as_ref(), serial);
        self.request_redraw();
    }

    /// Detach a single window from its source workspace & Space, clearing
    /// transient canvas/transition/fullscreen state and refreshing source focus.
    fn detach_window(
        &mut self,
        output_index: usize,
        workspace_index: usize,
        window: &Window,
    ) -> Option<DetachedWindow> {
        self.window_states.clear_queued_canvas_size(window);
        self.window_states.cancel_transition(window);

        let output = &mut self.outputs[output_index];
        let workspace = &mut output.workspaces[workspace_index];
        let column = workspace.scroll.take_window_to_column(window)?;
        let was_fullscreen = workspace.is_fullscreen(window);
        if was_fullscreen {
            workspace.exit_fullscreen();
        }
        let canvas = workspace.canvas.take(window);
        workspace.refresh_focus_for_mode();
        output.space.unmap_elem(window);
        Some(DetachedWindow {
            column,
            canvas,
            was_fullscreen,
        })
    }

    fn settle_source_workspace(
        &mut self,
        output_index: usize,
        workspace_index: usize,
        snap_canvas: bool,
    ) {
        let output = &mut self.outputs[output_index];
        let workspace = &mut output.workspaces[workspace_index];
        workspace.apply(&mut output.space);
        if snap_canvas || workspace.mode == Mode::Scroll {
            let camera = workspace.camera_target();
            output.view_x.snap(camera.x);
            output.view_y.snap(camera.y);
        }
        output.map_output();
    }

    fn configure_active_canvas_window(&mut self, window: &Window) {
        let active = self.active_workspace();
        if let Some(rect) = self.outputs[self.active_output].workspaces[active]
            .canvas
            .rect(window)
        {
            self.configure_canvas_window(window, canvas_pixel_size(rect));
        }
    }

    /// Move only the focused window to another workspace and follow it.
    /// The window keeps its canvas rectangle and its scroll slot (as its own
    /// column in the target layout).
    pub fn move_window_to_workspace(&mut self, index: usize) {
        self.cancel_interaction_state();
        let index = index.min(self.workspaces().len().saturating_sub(1));
        let active = self.active_workspace();
        if index == active {
            return;
        }
        let output_index = self.active_output;
        let Some(window) = self.outputs[output_index].workspaces[active]
            .focused_window()
            .cloned()
        else {
            return;
        };
        let target_old_fullscreen = self.outputs[output_index].workspaces[index]
            .fullscreen_window()
            .cloned();
        let target_mode = self.outputs[output_index].workspaces[index].mode;
        if let Some(old) = target_old_fullscreen.as_ref() {
            self.window_states.cancel_transition(old);
        }

        let Some(detached) = self.detach_window(output_index, active, &window) else {
            return;
        };
        {
            let target = &mut self.outputs[output_index].workspaces[index];
            target.scroll.insert_column(detached.column);
            if target.mode == Mode::Scroll {
                target.scroll.forget_configured_size(&window);
            }
            place_transferred_canvas_group(target, [(window.clone(), detached.canvas)]);
            target.fullscreen = detached.was_fullscreen.then(|| window.clone());
            target.set_focus(Some(window.clone()));
        }
        self.outputs[output_index].ensure_trailing_workspace();
        tracing::debug!(workspace = index + 1, "window moved to workspace");
        self.switch_workspace(index);
        if let Some(old) = target_old_fullscreen.as_ref() {
            self.configure_normal_after_fullscreen(old);
        }
        if detached.was_fullscreen {
            self.configure_fullscreen_overlay(&window);
        } else if target_mode == Mode::Canvas {
            self.configure_active_canvas_window(&window);
        }
    }

    /// Move only the focused window to the output in `direction`, then follow
    /// it with focus and the cursor.
    pub fn move_window_to_output(&mut self, direction: Direction) {
        if self.drag.is_some() || self.dnd_active {
            return;
        }
        let source_output = self.active_output;
        let Some(target_output) = self.output_in_direction(source_output, direction) else {
            tracing::debug!(?direction, "no output in move direction");
            return;
        };
        if target_output == source_output {
            return;
        }
        tracing::debug!(
            from = source_output,
            to = target_output,
            ?direction,
            "move window to output"
        );

        let source_workspace = self.outputs[source_output].active_workspace;
        let target_workspace = self.outputs[target_output].active_workspace;
        let Some(window) = self.outputs[source_output].workspaces[source_workspace]
            .focused_window()
            .cloned()
        else {
            return;
        };
        if self.outputs[source_output].workspaces[source_workspace]
            .scroll
            .tile_index(&window)
            .is_none()
        {
            return;
        }

        let source_mode = self.outputs[source_output].workspaces[source_workspace].mode;
        let target_mode = self.outputs[target_output].workspaces[target_workspace].mode;

        let (scroll_maximized, canvas_maximized, source_content_size) = {
            let workspace = &self.outputs[source_output].workspaces[source_workspace];
            let scroll_maximized = workspace
                .scroll
                .column_index(&window)
                .is_some_and(|index| workspace.scroll.columns[index].is_maximized());
            let source_content_size = workspace
                .scroll
                .content_target_rect(&window)
                .map(|rect| rect.size);
            (
                scroll_maximized,
                workspace
                    .canvas
                    .maximized
                    .as_ref()
                    .filter(|(w, _)| w == &window)
                    .map(|(_, saved)| *saved),
                source_content_size,
            )
        };

        let target_old_fullscreen = self.outputs[target_output].workspaces[target_workspace]
            .fullscreen_window()
            .cloned();
        if let Some(old) = target_old_fullscreen.as_ref() {
            self.window_states.cancel_transition(old);
        }

        let preservation =
            move_preservation(source_mode, scroll_maximized, canvas_maximized.is_some());
        let preserve_scroll_maximize = preservation.scroll_maximize;
        // Canvas has a single maximized slot; restoring the previous one is
        // handled while inserting the moved window.
        let preserve_canvas_maximize = preservation.canvas_maximize;

        let Some(detached) = self.detach_window(source_output, source_workspace, &window) else {
            return;
        };
        let preserve_target_fullscreen = detached.was_fullscreen;
        let column = detached.column;
        let canvas_removed = detached.canvas;
        self.settle_source_workspace(source_output, source_workspace, true);

        let target_area = self.outputs[target_output].workspaces[target_workspace]
            .scroll
            .working_area;
        let target_view_center = Point::from((
            self.outputs[target_output].view_x.value + target_area.loc.x + target_area.size.w / 2.0,
            self.outputs[target_output].view_y.value + target_area.loc.y + target_area.size.h / 2.0,
        ));
        let canvas_rect = canvas_rect_for_move(
            target_view_center,
            source_mode,
            canvas_maximized,
            canvas_removed.as_ref().map(|removed| removed.rect),
            source_content_size,
        );
        let canvas_maximize_rect = canvas_maximized.unwrap_or(canvas_rect);

        let target_focus;
        let mut canvas_client_size = None;
        let mut canvas_restore = None;
        {
            let output = &mut self.outputs[target_output];
            let target_camera = output.camera_location();
            let workspace = &mut output.workspaces[target_workspace];
            workspace.scroll.insert_column(column);
            if target_mode == Mode::Scroll {
                workspace.scroll.forget_configured_size(&window);
            }
            if preserve_scroll_maximize {
                workspace.scroll.ensure_active_column_maximized();
            }

            if let Some(removed) = canvas_removed {
                workspace
                    .canvas
                    .insert_at(window.clone(), canvas_rect, removed.z_index);
            } else if target_mode == Mode::Canvas {
                workspace.canvas.insert(window.clone(), canvas_rect);
            }

            if target_mode == Mode::Canvas {
                if preserve_canvas_maximize
                    && let Some((previous, saved)) = workspace.canvas.maximized.take()
                    && previous != window
                {
                    workspace.canvas.set_rect(&previous, saved);
                    canvas_restore = Some((previous, saved));
                }
                if preserve_canvas_maximize {
                    let max_rect = workspace.canvas_maximized_rect_at(target_camera);
                    workspace.canvas.maximized = Some((window.clone(), canvas_maximize_rect));
                    workspace.canvas.set_rect(&window, max_rect);
                }
                if !preserve_target_fullscreen {
                    let rect = workspace.canvas.rect(&window).unwrap_or(canvas_rect);
                    canvas_client_size = Some(canvas_pixel_size(rect));
                }
            }

            // One fullscreen per workspace. Clear any target fullscreen and
            // transfer the moved window's overlay state.
            workspace.fullscreen = None;
            if preserve_target_fullscreen {
                workspace.fullscreen = Some(window.clone());
            }
            target_focus = window.clone();
            workspace.set_focus(Some(target_focus.clone()));
        }
        self.outputs[target_output].ensure_trailing_workspace();

        if let Some((previous, saved)) = canvas_restore {
            self.configure_canvas_window(&previous, canvas_pixel_size(saved));
        }
        if let Some(size) = canvas_client_size {
            self.configure_canvas_window(&window, size);
        }
        self.active_output = target_output;
        // Apply layout and rebuild membership index before focusing so the
        // moved window is mapped in `target_output.space` and `window_locations`
        // is up to date before `focus_window` activates it and notifies IPC.
        self.rebuild_window_locations();
        self.apply_layout();
        let serial = SERIAL_COUNTER.next_serial();
        self.focus_window(Some(&target_focus), serial);
        if let Some(old) = target_old_fullscreen.as_ref() {
            self.configure_normal_after_fullscreen(old);
        }
        if preserve_target_fullscreen {
            self.configure_fullscreen_overlay(&window);
        }
        if target_mode == Mode::Canvas && !preserve_target_fullscreen {
            self.canvas_center();
        }
        let time = self.start_time.elapsed().as_millis() as u32;
        self.warp_pointer_to_output_center(target_output, time);
        self.mark_full_redraw_output(source_output);
        self.mark_full_redraw_output(target_output);

        tracing::debug!(
            from = source_output,
            to = target_output,
            ?source_mode,
            ?target_mode,
            preserve_fullscreen = preserve_target_fullscreen,
            preserve_scroll_maximize,
            preserve_canvas_maximize,
            "window moved to output"
        );
    }

    /// Finish a cross-output drag by moving the window into the target
    /// output's active workspace. `content_location` is the dropped content
    /// origin in target-output world coordinates.
    pub(crate) fn drop_window_from_drag(
        &mut self,
        window: Window,
        source_output: usize,
        source_workspace: usize,
        source_mode: Mode,
        target_output: usize,
        content_location: Point<f64, Logical>,
    ) -> bool {
        if source_output == target_output {
            return false;
        }

        // The target becomes active for insertion and focus. Exit its
        // fullscreen overlay first, matching keyboard cross-output moves.
        self.active_output = target_output;
        let target_workspace = self.outputs[target_output].active_workspace;
        if self.outputs[target_output].workspaces[target_workspace].has_fullscreen() {
            self.leave_fullscreen_for_action();
        }
        let target_mode = self.outputs[target_output].workspaces[target_workspace].mode;
        let content_size = window.geometry().size.to_f64();
        let now = self.animation_now();
        let target_before = if target_mode == Mode::Scroll {
            self.capture_content_world_rects(now)
        } else {
            Vec::new()
        };

        let Some(detached) = self.detach_window(source_output, source_workspace, &window) else {
            return false;
        };
        let column = detached.column;
        self.settle_source_workspace(source_output, source_workspace, false);

        {
            let output = &mut self.outputs[target_output];
            let workspace = &mut output.workspaces[target_workspace];
            if target_mode == Mode::Scroll {
                let inset = workspace.frame_inset();
                if !workspace
                    .scroll
                    .insert_column_at_position(column, content_location.x - inset)
                {
                    return false;
                }
                workspace.scroll.forget_configured_size(&window);
                if let Some(removed) = detached.canvas {
                    workspace
                        .canvas
                        .insert_at(removed.window, removed.rect, removed.z_index);
                }
            } else {
                workspace.scroll.insert_column(column);
                let size = if content_size.w > 0.0 && content_size.h > 0.0 {
                    content_size
                } else {
                    Size::from((800.0, 600.0))
                };
                workspace
                    .canvas
                    .insert(window.clone(), Rectangle::new(content_location, size));
            }
            workspace.set_focus(Some(window.clone()));
        }
        self.outputs[target_output].ensure_trailing_workspace();
        self.rebuild_window_locations();

        self.apply_layout();
        let serial = SERIAL_COUNTER.next_serial();
        self.focus_window(Some(&window), serial);

        if target_mode == Mode::Scroll {
            let to = self.outputs[target_output].workspaces[target_workspace]
                .content_target_rect_for_mode(&window);
            if let Some(to) = to {
                self.start_pending_layout_transitions_except(
                    &target_before,
                    now,
                    true,
                    Some(&window),
                );
                let from = Rectangle::new(
                    content_location,
                    if content_size.w > 0.0 && content_size.h > 0.0 {
                        content_size
                    } else {
                        to.size
                    },
                );
                let mut config = self.config.animations.window_movement;
                if self.config.animations.off {
                    config.off = true;
                }
                if self
                    .window_states
                    .start_layout_transition(&window, from, to, now, config)
                {
                    self.mark_full_redraw_output(target_output);
                }
            }
        }

        self.mark_full_redraw_output(source_output);
        self.mark_full_redraw_output(target_output);
        tracing::debug!(
            from = source_output,
            to = target_output,
            ?source_mode,
            ?target_mode,
            "window dropped on output"
        );
        true
    }

    pub fn move_column_to_workspace(&mut self, index: usize) {
        self.cancel_interaction_state();
        let index = index.min(self.workspaces().len().saturating_sub(1));
        if index == self.active_workspace() {
            return;
        }
        let output_index = self.active_output;
        let target_old_fullscreen = self.outputs[output_index].workspaces[index]
            .fullscreen_window()
            .cloned();
        if let Some(old) = target_old_fullscreen.as_ref() {
            self.window_states.cancel_transition(old);
        }

        let (moved_windows, target_mode, source_exited_fullscreen) = {
            let output = &mut self.outputs[output_index];
            let active = output.active_workspace;
            // A fullscreen window cannot stay fullscreen while its column moves.
            let fullscreen = output.workspaces[active].fullscreen_window().cloned();
            output.workspaces[active].exit_fullscreen();
            if let Some(window) = fullscreen.as_ref() {
                self.window_states.cancel_transition(window);
            }

            let Some(column) = output.workspaces[active].scroll.take_active_column() else {
                return;
            };
            let focused = column
                .tiles
                .get(column.active)
                .map(|tile| tile.window.clone());
            let target_mode = output.workspaces[index].mode;
            let mut canvas_removed = Vec::with_capacity(column.tiles.len());
            for tile in &column.tiles {
                canvas_removed.push(output.workspaces[active].canvas.take(&tile.window));
                output.space.unmap_elem(&tile.window);
                self.window_states.cancel_transition(&tile.window);
            }

            output.workspaces[active].refresh_focus_for_mode();

            let moved_windows: Vec<Window> = column
                .tiles
                .iter()
                .map(|tile| tile.window.clone())
                .collect();
            let source_exited_fullscreen =
                fullscreen.filter(|window| !moved_windows.contains(window));
            output.workspaces[index].fullscreen = None;
            output.workspaces[index].scroll.insert_column(column);
            if target_mode == Mode::Scroll {
                for window in &moved_windows {
                    output.workspaces[index]
                        .scroll
                        .forget_configured_size(window);
                }
            }
            place_transferred_canvas_group(
                &mut output.workspaces[index],
                moved_windows.iter().cloned().zip(canvas_removed),
            );
            output.workspaces[index].set_focus(focused);
            (moved_windows, target_mode, source_exited_fullscreen)
        };
        if let Some(exited) = source_exited_fullscreen.as_ref() {
            self.configure_normal_after_fullscreen(exited);
        }
        self.outputs[output_index].ensure_trailing_workspace();
        tracing::debug!(workspace = index + 1, "column moved to workspace");
        self.switch_workspace(index);
        if target_mode == Mode::Canvas {
            for window in moved_windows {
                self.configure_active_canvas_window(&window);
            }
        }
        if let Some(old) = target_old_fullscreen.as_ref() {
            self.configure_normal_after_fullscreen(old);
        }
    }
}

fn canvas_rect_for_move(
    target_view_center: Point<f64, Logical>,
    source_mode: Mode,
    canvas_maximized: Option<Rectangle<f64, Logical>>,
    canvas_rect: Option<Rectangle<f64, Logical>>,
    source_content_size: Option<Size<f64, Logical>>,
) -> Rectangle<f64, Logical> {
    if let Some(saved) = canvas_maximized {
        return saved;
    }
    if let Some(rect) = canvas_rect
        && source_mode == Mode::Canvas
    {
        return rect;
    }
    let size = if source_mode == Mode::Scroll {
        source_content_size.or(canvas_rect.map(|rect| rect.size))
    } else {
        canvas_rect.map(|rect| rect.size).or(source_content_size)
    }
    .unwrap_or_else(|| Size::from((800.0, 600.0)));
    Rectangle::new(
        target_view_center - Point::from((size.w / 2.0, size.h / 2.0)),
        size,
    )
}

#[cfg(test)]
mod tests {
    use super::{MovePreservation, canvas_rect_for_move, move_preservation};
    use crate::workspace::Mode;
    use smithay::utils::{Point, Rectangle, Size};

    fn state(
        source_mode: Mode,
        scroll_maximized: bool,
        canvas_maximized: bool,
    ) -> MovePreservation {
        move_preservation(source_mode, scroll_maximized, canvas_maximized)
    }

    #[test]
    fn move_preserves_maximize_state() {
        assert_eq!(
            state(Mode::Scroll, true, false),
            MovePreservation {
                scroll_maximize: true,
                canvas_maximize: true,
            }
        );
        assert_eq!(
            state(Mode::Canvas, false, true),
            MovePreservation {
                scroll_maximize: true,
                canvas_maximize: true,
            }
        );
        assert_eq!(
            state(Mode::Canvas, false, false),
            MovePreservation {
                scroll_maximize: false,
                canvas_maximize: false,
            }
        );
    }

    #[test]
    fn canvas_rect_keeps_canvas_and_centres_scroll_content() {
        let view_center = Point::from((600.0, 450.0));
        let canvas_rect = Rectangle::new(Point::from((50.0, 60.0)), Size::from((400.0, 300.0)));

        assert_eq!(
            canvas_rect_for_move(view_center, Mode::Canvas, None, Some(canvas_rect), None),
            canvas_rect
        );
        assert_eq!(
            canvas_rect_for_move(
                view_center,
                Mode::Scroll,
                None,
                Some(canvas_rect),
                Some(Size::from((600.0, 400.0))),
            ),
            Rectangle::new(Point::from((300.0, 250.0)), Size::from((600.0, 400.0)))
        );
        assert_eq!(
            canvas_rect_for_move(view_center, Mode::Canvas, Some(canvas_rect), None, None),
            canvas_rect
        );
        assert_eq!(
            canvas_rect_for_move(view_center, Mode::Scroll, None, None, None),
            Rectangle::new(Point::from((200.0, 150.0)), Size::from((800.0, 600.0)))
        );
    }
}
