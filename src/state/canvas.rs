//! Canvas navigation, moving, resizing, panning, and maximize/fullscreen.

use smithay::{
    desktop::Window,
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State,
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Size},
};

use crate::{
    config::Action,
    state::EvaWM,
    workspace::{Direction, Mode, canvas_pixel_size, rects_approx_eq},
};

fn pan_amount(amount: Option<i32>, default: f64) -> f64 {
    amount.map_or(default, f64::from)
}

fn clamp_canvas_size(
    size: Size<f64, Logical>,
    min: Size<i32, Logical>,
    max: Size<i32, Logical>,
) -> Size<f64, Logical> {
    let mut width = size.w;
    let mut height = size.h;
    if max.w > 0 {
        width = width.min(f64::from(max.w));
    }
    if max.h > 0 {
        height = height.min(f64::from(max.h));
    }
    if min.w > 0 {
        width = width.max(f64::from(min.w));
    }
    if min.h > 0 {
        height = height.max(f64::from(min.h));
    }
    Size::from((width.round().max(1.0), height.round().max(1.0)))
}

/// Apply a relative resize, clamp to the client's constraints, and round to
/// whole logical pixels. Returns `None` when the result is the same rectangle,
/// so a no-op cannot send a configure or start a transition.
fn canvas_resize_target(
    from: Rectangle<f64, Logical>,
    width_fraction: f64,
    height_fraction: f64,
    min: Size<i32, Logical>,
    max: Size<i32, Logical>,
) -> Option<Rectangle<f64, Logical>> {
    let raw = Size::from((
        from.size.w * (1.0 + width_fraction),
        from.size.h * (1.0 + height_fraction),
    ));
    let to = Rectangle::new(from.loc, clamp_canvas_size(raw, min, max));
    (!rects_approx_eq(from, to)).then_some(to)
}

impl EvaWM {
    pub fn active_mode(&self) -> Mode {
        self.output_state().workspaces[self.active_workspace()].mode
    }

    fn canvas_focused(&self) -> Option<(Window, Rectangle<f64, Logical>)> {
        let output = self.output_state();
        let workspace = &output.workspaces[output.active_workspace];
        let window = workspace.focused_window()?.clone();
        let rect = workspace.canvas.rect(&window)?;
        Some((window, rect))
    }

    pub(crate) fn canvas_constraints(
        &self,
        window: &Window,
    ) -> (Size<i32, Logical>, Size<i32, Logical>) {
        self.canvas_constraints_at(self.active_output, self.active_workspace(), window)
    }

    pub(crate) fn canvas_constraints_at(
        &self,
        output_index: usize,
        workspace_index: usize,
        window: &Window,
    ) -> (Size<i32, Logical>, Size<i32, Logical>) {
        self.outputs
            .get(output_index)
            .and_then(|output| output.workspaces.get(workspace_index))
            .and_then(|workspace| {
                workspace
                    .scroll
                    .tile_index(window)
                    .map(|(column, tile)| (workspace, column, tile))
            })
            .map(|(workspace, column, tile)| {
                let tile = &workspace.scroll.columns[column].tiles[tile];
                (tile.min_size, tile.max_size)
            })
            .unwrap_or((Size::from((1, 1)), Size::from((0, 0))))
    }

    pub(crate) fn configure_canvas_window(&mut self, window: &Window, size: Size<i32, Logical>) {
        tracing::trace!(?size, "canvas configure");
        let wall_now = std::time::Instant::now();
        self.window_states.clear_queued_canvas_size(window);
        self.window_states
            .mark_canvas_configure(window, size, wall_now);
        let Some(toplevel) = window.toplevel().cloned() else {
            return;
        };
        toplevel.with_pending_state(|state| {
            state.size = Some(size);
            state.states.unset(State::Fullscreen);
            state.states.set(State::TiledLeft);
            state.states.set(State::TiledRight);
            state.states.set(State::TiledTop);
            state.states.set(State::TiledBottom);
        });
        toplevel.send_pending_configure();

        // This size came from canvas, not the scroll layout. Forget the
        // scroll tile's cached configure so switching or moving back to scroll
        // always sends the scroll size again.
        for output in &mut self.outputs {
            for workspace in &mut output.workspaces {
                workspace.scroll.forget_configured_size(window);
            }
        }
    }

    /// Queue an interactive resize size. It is sent at the next render frame,
    /// so a 1000 Hz mouse cannot flood Firefox/Chromium with configures while
    /// the frame and content still update every frame.
    pub(crate) fn queue_canvas_configure(&mut self, window: &Window, size: Size<i32, Logical>) {
        self.window_states.queue_canvas_size(window, size);
        self.schedule_canvas_configure_flush();
        self.request_redraw();
    }

    pub(crate) fn flush_canvas_configures(&mut self) {
        let wall_now = std::time::Instant::now();
        for (window, size) in self.window_states.take_ready_queued_canvas_sizes(wall_now) {
            self.configure_canvas_window(&window, size);
        }
    }

    pub(crate) fn canvas_update_rect_at(
        &mut self,
        output_index: usize,
        workspace_index: usize,
        window: &Window,
        rect: Rectangle<f64, Logical>,
    ) {
        let Some(output) = self.outputs.get_mut(output_index) else {
            return;
        };
        let Some(workspace) = output.workspaces.get_mut(workspace_index) else {
            return;
        };
        if workspace.mode != Mode::Canvas {
            return;
        }
        let changed = workspace
            .canvas
            .rect(window)
            .is_none_or(|old| !rects_approx_eq(old, rect));
        if changed {
            if workspace
                .canvas
                .maximized
                .as_ref()
                .is_some_and(|(maximized, _)| maximized == window)
            {
                workspace.canvas.maximized = None;
            }
            if workspace
                .canvas
                .last_restore
                .as_ref()
                .is_none_or(|(restored, _)| restored == window)
            {
                workspace.canvas.last_restore = Some((window.clone(), rect));
            }
        }
        workspace.canvas.set_rect(window, rect);
        workspace.apply_windows(&mut output.space);
        output.map_output();
        self.request_redraw_output(output_index);
    }

    fn canvas_apply(&mut self) {
        let output_index = self.active_output;
        let workspace_index = self.active_workspace();
        let output = &mut self.outputs[output_index];
        output.workspaces[workspace_index].apply(&mut output.space);
        output.map_output();
        self.mark_full_redraw_output(output_index);
    }

    fn canvas_transition(
        &mut self,
        window: &Window,
        from: Rectangle<f64, Logical>,
        to: Rectangle<f64, Logical>,
    ) {
        let now = self.animation_now();
        let mut config = self.config.animations.window_movement;
        if self.config.animations.off {
            config.off = true;
        }
        self.window_states
            .start_layout_transition(window, from, to, now, config);
        self.mark_full_redraw_output(self.active_output);
    }

    fn canvas_animate_camera(&mut self, target: Point<f64, Logical>) {
        let now = self.animation_now();
        let mut config = self.config.animations.view_movement;
        if self.config.animations.off {
            config.off = true;
        }
        let output_index = self.active_output;
        let output = &mut self.outputs[output_index];
        output.view_x.set_target(target.x, now, config);
        output.view_y.set_target(target.y, now, config);
        output.map_output();
        self.mark_full_redraw_output(output_index);
    }

    pub(crate) fn execute_canvas_action(&mut self, action: &Action) -> bool {
        match action {
            Action::FocusLeft => self.canvas_focus(Direction::Left),
            Action::FocusRight => self.canvas_focus(Direction::Right),
            Action::FocusUp => self.canvas_focus(Direction::Up),
            Action::FocusDown => self.canvas_focus(Direction::Down),
            Action::FocusColumnFirst | Action::FocusColumnLast => {}
            Action::MoveWindowLeft => self.canvas_nudge(-self.canvas_move_step(), 0.0),
            Action::MoveWindowRight => self.canvas_nudge(self.canvas_move_step(), 0.0),
            Action::MoveUp => self.canvas_nudge(0.0, -self.canvas_move_step()),
            Action::MoveDown => self.canvas_nudge(0.0, self.canvas_move_step()),
            Action::MoveWindowToWorkspace(workspace) => {
                let target = usize::from(workspace.saturating_sub(1));
                self.move_window_to_workspace(target);
            }
            Action::MoveLeft
            | Action::MoveRight
            | Action::MoveColumnToFirst
            | Action::MoveColumnToLast
            | Action::ConsumeOrExpelLeft
            | Action::ConsumeOrExpelRight
            | Action::MoveColumnToWorkspace(_)
            | Action::ExpandColumnToAvailableWidth => {}
            Action::ResizeWidth(fraction) => self.canvas_resize(*fraction, 0.0),
            Action::ResizeHeight(fraction) => self.canvas_resize(0.0, *fraction),
            Action::MaximizeColumn => self.canvas_maximize(),
            Action::CenterFocused => self.canvas_center(),
            Action::FullscreenWindow => self.toggle_fullscreen(),
            Action::PanCameraLeft(amount) => {
                self.canvas_pan(-pan_amount(*amount, self.config.canvas.pan_step), 0.0);
            }
            Action::PanCameraRight(amount) => {
                self.canvas_pan(pan_amount(*amount, self.config.canvas.pan_step), 0.0);
            }
            Action::PanCameraUp(amount) => {
                self.canvas_pan(0.0, -pan_amount(*amount, self.config.canvas.pan_step));
            }
            Action::PanCameraDown(amount) => {
                self.canvas_pan(0.0, pan_amount(*amount, self.config.canvas.pan_step));
            }
            Action::HomeCanvas => self.canvas_home(),
            Action::CycleWindowNext => self.cycle_mru(true),
            Action::CycleWindowPrevious => self.cycle_mru(false),
            _ => return false,
        }
        true
    }

    fn canvas_move_step(&self) -> f64 {
        self.config.canvas.move_step
    }

    fn canvas_focus(&mut self, direction: Direction) {
        let output = self.output_state();
        let workspace = &output.workspaces[output.active_workspace];
        let current = workspace.focused_window().cloned();
        let Some(window) = workspace
            .canvas
            .focus_direction(current.as_ref(), direction)
        else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        self.focus_window(Some(&window), serial);
        self.request_redraw();
    }

    fn canvas_nudge(&mut self, dx: f64, dy: f64) {
        let Some((window, from)) = self.canvas_focused() else {
            return;
        };
        let to = Rectangle::new(from.loc + Point::from((dx, dy)), from.size);
        if rects_approx_eq(from, to) {
            return;
        }
        let now = self.animation_now();
        let visual_from = self
            .output_state()
            .current_content_world_rect(&window, now, &self.window_states)
            .unwrap_or(from);
        let output_index = self.active_output;
        let workspace_index = self.active_workspace();
        self.canvas_update_rect_at(output_index, workspace_index, &window, to);
        self.canvas_transition(&window, visual_from, to);
        self.notify_ipc();
    }

    fn canvas_resize(&mut self, width_fraction: f64, height_fraction: f64) {
        let Some((window, from)) = self.canvas_focused() else {
            return;
        };
        if self.window_is_fixed_size(&window) {
            tracing::trace!("canvas resize skipped: fixed-size window");
            return;
        }
        let (min, max) = self.canvas_constraints(&window);
        let Some(to) = canvas_resize_target(from, width_fraction, height_fraction, min, max) else {
            return;
        };
        let now = self.animation_now();
        let visual_from = self
            .output_state()
            .current_content_world_rect(&window, now, &self.window_states)
            .unwrap_or(from);
        let size = canvas_pixel_size(to);
        let output_index = self.active_output;
        let workspace_index = self.active_workspace();
        self.canvas_update_rect_at(output_index, workspace_index, &window, to);
        self.configure_canvas_window(&window, size);
        self.canvas_transition(&window, visual_from, to);
        self.notify_ipc();
    }

    fn canvas_maximize(&mut self) {
        let Some((window, current)) = self.canvas_focused() else {
            return;
        };
        if self.window_is_fixed_size(&window) {
            tracing::trace!("canvas maximize skipped: fixed-size window");
            return;
        }
        tracing::trace!(?current, "canvas maximize");
        let now = self.animation_now();
        let visual_from = self
            .output_state()
            .current_content_world_rect(&window, now, &self.window_states)
            .unwrap_or(current);
        let (min, max) = self.canvas_constraints(&window);
        let output_index = self.active_output;
        let workspace_index = self.active_workspace();
        let camera = {
            let output = &self.outputs[output_index];
            Point::from((output.view_x.target, output.view_y.target))
        };

        let (restore, from, to, changed) = {
            let output = &mut self.outputs[output_index];
            let workspace = &mut output.workspaces[workspace_index];
            match workspace.canvas.maximized.take() {
                Some((maximized, saved)) if maximized == window => {
                    workspace.canvas.set_rect(&maximized, saved);
                    (None, visual_from, saved, true)
                }
                previous => {
                    let restore = previous.and_then(|(maximized, saved)| {
                        let previous_rect = workspace.canvas.rect(&maximized)?;
                        workspace.canvas.set_rect(&maximized, saved);
                        Some((maximized, previous_rect, saved))
                    });
                    let mut target = workspace.canvas_maximized_rect_at(camera);
                    target.size = clamp_canvas_size(target.size, min, max);
                    tracing::trace!(?target, "canvas maximize target");
                    if rects_approx_eq(current, target) {
                        // The rect is already at the maximized target. Never
                        // store that rect as its own restore target; if a
                        // meaningful previous size exists, F restores it.
                        let saved = workspace
                            .canvas
                            .last_restore
                            .as_ref()
                            .filter(|(restore_window, _)| restore_window == &window)
                            .map(|(_, rect)| *rect)
                            .filter(|rect| !rects_approx_eq(*rect, target));
                        if let Some(saved) = saved {
                            workspace.canvas.set_rect(&window, saved);
                            (restore, visual_from, saved, true)
                        } else {
                            (restore, visual_from, current, false)
                        }
                    } else {
                        workspace.canvas.last_restore = Some((window.clone(), current));
                        workspace.canvas.maximized = Some((window.clone(), current));
                        workspace.canvas.set_rect(&window, target);
                        (restore, visual_from, target, true)
                    }
                }
            }
        };

        let had_restore = restore.is_some();
        if let Some((previous, previous_rect, saved)) = restore {
            let prev_from = self
                .output_state()
                .current_content_world_rect(&previous, now, &self.window_states)
                .unwrap_or(previous_rect);
            self.configure_canvas_window(&previous, canvas_pixel_size(saved));
            self.canvas_transition(&previous, prev_from, saved);
        }
        if had_restore || changed {
            self.canvas_apply();
            self.notify_ipc();
        }
        if changed {
            tracing::trace!(?from, ?to, "canvas maximize apply");
            self.configure_canvas_window(&window, canvas_pixel_size(to));
            self.canvas_transition(&window, from, to);
        }
    }

    pub(crate) fn canvas_window_visible(&self, window: &Window) -> bool {
        let output = self.output_state();
        let workspace = &output.workspaces[output.active_workspace];
        let Some(rect) = workspace.canvas.rect(window) else {
            return true;
        };
        let area = workspace.scroll.working_area;
        let view = Rectangle::new(output.camera_location() + area.loc, area.size);
        canvas_rect_visible_in_view(rect, view)
    }

    pub(crate) fn canvas_center(&mut self) {
        let Some((_, rect)) = self.canvas_focused() else {
            return;
        };
        let output = self.output_state();
        let area = output.workspaces[output.active_workspace]
            .scroll
            .working_area;
        let target = Point::from((
            rect.loc.x + rect.size.w / 2.0 - area.loc.x - area.size.w / 2.0,
            rect.loc.y + rect.size.h / 2.0 - area.loc.y - area.size.h / 2.0,
        ));
        self.canvas_animate_camera(target);
    }

    fn canvas_pan(&mut self, dx: f64, dy: f64) {
        let output = self.output_state();
        let target = Point::from((output.view_x.target + dx, output.view_y.target + dy));
        self.canvas_animate_camera(target);
    }

    fn canvas_home(&mut self) {
        let output = self.output_state();
        let workspace = &output.workspaces[output.active_workspace];
        let area = workspace.scroll.working_area;
        let center: Point<f64, Logical> = workspace.canvas_bounds().map_or_else(
            || Point::from((0.0, 0.0)),
            |rect| {
                Point::from((
                    rect.loc.x + rect.size.w / 2.0,
                    rect.loc.y + rect.size.h / 2.0,
                ))
            },
        );
        let target = Point::from((
            center.x - area.loc.x - area.size.w / 2.0,
            center.y - area.loc.y - area.size.h / 2.0,
        ));
        self.canvas_animate_camera(target);
    }
}

impl EvaWM {
    /// Toggle the active workspace between scroll and canvas.
    pub fn toggle_canvas(&mut self) {
        self.cancel_interaction_state();
        if self.active_fullscreen().is_some() {
            self.leave_fullscreen_for_action();
        }

        let now = self.animation_now();
        let mut config = self.config.animations.view_movement;
        if self.config.animations.off {
            config.off = true;
        }
        let output_index = self.active_output;
        let workspace_index = self.active_workspace();
        let before_rects = self.capture_content_world_rects(now);

        let entered_canvas = {
            let output = &mut self.outputs[output_index];
            let workspace = &mut output.workspaces[workspace_index];
            if workspace.mode == Mode::Scroll {
                output.seed_canvas(workspace_index, now, &self.window_states);
                let workspace = &mut output.workspaces[workspace_index];
                let focused = workspace.scroll.focused_window().cloned();
                for window in workspace.windows() {
                    self.window_states.clear_queued_canvas_size(window);
                }
                workspace.set_focus(focused.clone());
                workspace.mode = Mode::Canvas;
                workspace.refresh_configured_sizes();
                if let Some(window) = &focused {
                    workspace.canvas.raise(window);
                }
                workspace.apply(&mut output.space);
                let target = workspace.camera_target();
                output.view_x.set_target(target.x, now, config);
                output.view_y.set_target(target.y, now, config);
                true
            } else {
                workspace.canvas.last_camera =
                    Some(Point::from((output.view_x.target, output.view_y.target)));
                let focused = workspace.focused_window().cloned();
                for window in workspace.windows() {
                    self.window_states.clear_queued_canvas_size(window);
                }
                workspace.mode = Mode::Scroll;
                workspace.refresh_configured_sizes();
                if let Some(window) = &focused
                    && let Some((column, tile)) = workspace.scroll.tile_index(window)
                {
                    workspace.scroll.set_active_tile(column, tile);
                }
                workspace.sync_scroll_focus();
                workspace.apply(&mut output.space);
                let target = workspace.camera_target();
                output.view_x.set_target(target.x, now, config);
                output.view_y.set_target(0.0, now, config);
                false
            }
        };

        self.outputs[output_index].map_output();
        if !self.keyboard_layer_focused() {
            let focused = self.outputs[output_index].workspaces[workspace_index]
                .focused_window()
                .cloned();
            let serial = SERIAL_COUNTER.next_serial();
            self.focus_window_internal(focused.as_ref(), serial, false);
        }
        self.notify_ipc();
        self.start_pending_layout_transitions(&before_rects, now, true);
        self.mark_full_redraw_output(output_index);
        tracing::debug!(
            mode = if entered_canvas { "canvas" } else { "scroll" },
            "mode toggled"
        );
    }
}

fn canvas_axis_visible(rect_start: f64, rect_len: f64, view_start: f64, view_len: f64) -> bool {
    const EPS: f64 = 0.5;
    if rect_len <= view_len {
        rect_start >= view_start - EPS && rect_start + rect_len <= view_start + view_len + EPS
    } else {
        rect_start <= view_start + EPS && rect_start + rect_len >= view_start + view_len - EPS
    }
}

pub(crate) fn canvas_rect_visible_in_view(
    rect: Rectangle<f64, Logical>,
    view: Rectangle<f64, Logical>,
) -> bool {
    canvas_axis_visible(rect.loc.x, rect.size.w, view.loc.x, view.size.w)
        && canvas_axis_visible(rect.loc.y, rect.size.h, view.loc.y, view.size.h)
}

#[cfg(test)]
mod tests {
    use smithay::utils::{Logical, Point, Rectangle, Size};

    use super::canvas_resize_target;

    fn rect(width: f64, height: f64) -> Rectangle<f64, Logical> {
        Rectangle::new(Point::from((10.0, 20.0)), Size::from((width, height)))
    }

    fn unconstrained() -> Size<i32, Logical> {
        Size::from((0, 0))
    }

    #[test]
    fn zero_percent_is_a_noop() {
        let from = rect(100.0, 50.0);
        assert!(canvas_resize_target(from, 0.0, 0.0, unconstrained(), unconstrained()).is_none());
    }

    #[test]
    fn positive_and_negative_percentages_change_size() {
        let from = rect(100.0, 50.0);

        let grown = canvas_resize_target(from, 0.10, 0.0, unconstrained(), unconstrained())
            .expect("10% should grow the width");
        assert_eq!(grown.size, Size::from((110.0, 50.0)));

        let shrunk = canvas_resize_target(from, -0.10, 0.0, unconstrained(), unconstrained())
            .expect("-10% should shrink the width");
        assert_eq!(shrunk.size, Size::from((90.0, 50.0)));
    }

    #[test]
    fn width_and_height_are_independent() {
        let from = rect(100.0, 50.0);

        let width_only = canvas_resize_target(from, 0.20, 0.0, unconstrained(), unconstrained())
            .expect("width change should apply");
        assert_eq!(width_only.size, Size::from((120.0, 50.0)));

        let height_only = canvas_resize_target(from, 0.0, -0.20, unconstrained(), unconstrained())
            .expect("height change should apply");
        assert_eq!(height_only.size, Size::from((100.0, 40.0)));
    }

    #[test]
    fn sizes_clamp_to_client_minimum_and_maximum() {
        let from = rect(100.0, 50.0);
        let min = Size::from((80, 40));
        let max = Size::from((105, 60));

        let grown = canvas_resize_target(from, 0.10, 0.10, min, max)
            .expect("growth should be clamped, not ignored");
        assert_eq!(grown.size, Size::from((105.0, 55.0)));

        let shrunk = canvas_resize_target(from, -0.50, -0.50, min, max)
            .expect("shrink should be clamped, not ignored");
        assert_eq!(shrunk.size, Size::from((80.0, 40.0)));
    }

    #[test]
    fn noop_at_a_constraint_is_none() {
        let from = rect(105.0, 60.0);
        let max = Size::from((105, 60));
        assert!(canvas_resize_target(from, 0.10, 0.10, unconstrained(), max).is_none());
    }

    #[test]
    fn sizes_round_to_whole_pixels() {
        let from = rect(101.0, 99.0);
        let resized = canvas_resize_target(from, 0.10, 0.10, unconstrained(), unconstrained())
            .expect("10% should change the rounded size");
        assert_eq!(resized.size, Size::from((111.0, 109.0)));
    }

    #[test]
    fn canvas_visibility_accepts_contained_and_spanning_rects() {
        let view = Rectangle::new(Point::from((100.0, 30.0)), Size::from((1920.0, 1050.0)));

        let inside = Rectangle::new(Point::from((200.0, 100.0)), Size::from((1200.0, 800.0)));
        assert!(super::canvas_rect_visible_in_view(inside, view));

        let taller_clamped =
            Rectangle::new(Point::from((100.0, 30.0)), Size::from((1920.0, 1080.0)));
        assert!(super::canvas_rect_visible_in_view(taller_clamped, view));
    }

    #[test]
    fn canvas_visibility_rejects_clipped_or_offscreen_rects() {
        let view = Rectangle::new(Point::from((100.0, 30.0)), Size::from((1920.0, 1050.0)));

        let clipped_right =
            Rectangle::new(Point::from((1500.0, 100.0)), Size::from((800.0, 600.0)));
        assert!(!super::canvas_rect_visible_in_view(clipped_right, view));

        let offscreen = Rectangle::new(Point::from((2500.0, 100.0)), Size::from((800.0, 600.0)));
        assert!(!super::canvas_rect_visible_in_view(offscreen, view));

        let taller_shifted_down =
            Rectangle::new(Point::from((100.0, 400.0)), Size::from((1920.0, 1080.0)));
        assert!(!super::canvas_rect_visible_in_view(
            taller_shifted_down,
            view
        ));
    }
}
