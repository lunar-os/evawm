//! Per-output compositor state: layout, camera, `Space`, and workspaces.

use std::time::Instant;

#[cfg(any(feature = "nested", test))]
use smithay::utils::Physical;
use smithay::{
    desktop::{Space, Window, WindowSurfaceType, layer_map_for_output},
    output::Output,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle, Size},
    wayland::shell::wlr_layer::Layer as WlrLayer,
};

use crate::{
    animation::{AnimatedValue, AnimationConfig},
    config::Config,
    layout::Layout,
    window_state::WindowStates,
    workspace::{Direction, MAX_WORKSPACES, Mode, Workspace},
};

pub struct OutputState {
    pub output: Output,
    pub space: Space<Window>,
    /// Dynamic workspaces on this output. Each keeps its own scroll and canvas
    /// layout; only the active workspace's windows are mapped into `space`.
    pub workspaces: Vec<Workspace>,
    pub active_workspace: usize,
    pub view_x: AnimatedValue,
    pub view_y: AnimatedValue,
    /// Animated Overview scale (`1.0` when closed, configured zoom when open).
    pub overview_zoom: AnimatedValue,
    /// This output needs a frame for the current dirty state.
    pub needs_redraw: bool,
    /// Pending full-redraw frames (animations, reloads, first frame).
    pub full_redraw: u8,
    /// Mode applied to newly created workspaces.
    pub default_mode: Mode,
    /// An idle render callback is already queued for this output.
    pub render_scheduled: bool,
    /// Consecutive transient render failures while the DRM session recovers.
    pub render_retries: u8,
    /// A backoff retry timer is already armed for this output.
    pub render_retry_armed: bool,
    /// Last time this output presented a frame. Used to decide whether the
    /// present path or the idle fallback owns frame callbacks.
    pub last_present: Option<Instant>,
    /// Last time frame callbacks were delivered for this output.
    pub last_frame_callback: Option<Instant>,
    /// A frame-callback pacing timer is already queued for this output.
    pub frame_callback_tick: bool,
    /// Monotonic counter incremented each time this output presents a frame
    /// with damage. Used by `wlr-screencopy` `copy_with_damage` so the first
    /// capture and any damaged capture complete immediately while idle
    /// captures wait for real damage.
    pub present_seq: u64,
}

impl OutputState {
    pub fn new(output: Output, config: &Config) -> Self {
        let output_size = output
            .current_mode()
            .map(|mode| {
                output
                    .current_transform()
                    .transform_size(mode.size.to_logical(1))
            })
            .unwrap_or_else(|| Size::from((1280, 800)));
        let mut space = Space::default();
        space.map_output(&output, (0, 0));

        let layout = Layout::new(
            output_size,
            config.layout.gaps,
            config.layout.default_column_proportion,
            config.layout.inner_border,
            config.layout.outer_border,
        );
        let mut workspaces = vec![Workspace::new(layout)];
        workspaces[0].mode = config.layout.default_mode;

        Self {
            output,
            space,
            workspaces,
            active_workspace: 0,
            view_x: AnimatedValue::new(0.0),
            view_y: AnimatedValue::new(0.0),
            overview_zoom: AnimatedValue::new(1.0),
            needs_redraw: true,
            full_redraw: 1,
            default_mode: config.layout.default_mode,
            render_scheduled: false,
            render_retries: 0,
            render_retry_armed: false,
            last_present: None,
            last_frame_callback: None,
            frame_callback_tick: false,
            present_seq: 0,
        }
    }

    fn new_workspace(&self) -> Workspace {
        let mut workspace = Workspace::new(self.new_workspace_layout());
        workspace.mode = self.default_mode;
        workspace
    }

    fn new_workspace_layout(&self) -> Layout {
        let source = &self.workspaces[self.active_workspace].scroll;
        let mut layout = Layout::new(
            Size::from((
                source.working_area.size.w.round() as i32,
                source.working_area.size.h.round() as i32,
            )),
            source.gap,
            source.default_proportion,
            source.inner_border,
            source.outer_border,
        );
        layout.working_area = source.working_area;
        layout.output_size = source.output_size;
        layout
    }

    /// Test helper: create a run of empty workspaces for layout tests.
    #[cfg(test)]
    pub fn ensure_workspace(&mut self, index: usize) {
        while self.workspaces.len() <= index && self.workspaces.len() < MAX_WORKSPACES {
            self.workspaces.push(self.new_workspace());
        }
    }

    /// Keep niri's trailing empty workspace: after a window lands in the last
    /// workspace, append a fresh empty one so there is always somewhere to go.
    pub fn ensure_trailing_workspace(&mut self) {
        if self.workspaces.len() >= MAX_WORKSPACES {
            return;
        }
        if self.workspaces.last().is_some_and(Workspace::has_windows) {
            self.workspaces.push(self.new_workspace());
        }
    }

    /// Drop empty workspaces the user has left. The active workspace may stay
    /// empty, and the last workspace is always the empty trailing one.
    pub fn clean_up_workspaces(&mut self) {
        let mut index = 0;
        while index + 1 < self.workspaces.len() {
            if index != self.active_workspace && !self.workspaces[index].has_windows() {
                self.workspaces.remove(index);
                if self.active_workspace > index {
                    self.active_workspace -= 1;
                }
            } else {
                index += 1;
            }
        }
        self.ensure_trailing_workspace();
    }

    pub fn active_layout(&self) -> &Layout {
        &self.workspaces[self.active_workspace].scroll
    }

    pub fn active_layout_mut(&mut self) -> &mut Layout {
        &mut self.workspaces[self.active_workspace].scroll
    }

    pub fn camera_location(&self) -> Point<f64, Logical> {
        self.space
            .output_geometry(&self.output)
            .map(|geometry| geometry.loc.to_f64())
            .unwrap_or_else(|| Point::from((0.0, 0.0)))
    }

    /// Screen-space pivot point for Overview zoom: the center of the active
    /// workspace's working area so zoomed-out windows stay centered clear of
    /// exclusive-zone bars.
    pub fn overview_pivot(&self) -> Point<f64, Logical> {
        rect_center(self.workspaces[self.active_workspace].scroll.working_area)
    }

    /// Map a screen-space pointer location on this output into workspace world
    /// coordinates, inverting the current Overview zoom around `overview_pivot`.
    pub fn overview_screen_to_world(&self, screen: Point<f64, Logical>) -> Point<f64, Logical> {
        overview_screen_to_world(
            screen,
            self.overview_pivot(),
            self.camera_location(),
            self.overview_zoom.value,
        )
    }

    /// World-space rectangle visible on this output at its current Overview
    /// zoom. Used for window culling and frame-callback visibility.
    pub fn visible_world_viewport(&self) -> Rectangle<f64, Logical> {
        let output_size = self.workspaces[self.active_workspace].output_size();
        overview_cull_viewport(
            self.camera_location(),
            output_size,
            self.overview_pivot(),
            self.overview_zoom.value,
        )
    }

    /// Location of the window's toplevel surface origin in space coordinates.
    ///
    /// `Space` stores element locations as window-geometry locations, while
    /// `Window::surface_under` expects points relative to the surface origin.
    /// Client-side decorations place their geometry inside the surface (for
    /// example shadow margins), so pointer hit-testing must subtract that
    /// offset or clicks land up-left of the cursor.
    pub fn window_surface_origin(&self, window: &Window) -> Option<Point<i32, Logical>> {
        Some(self.space.element_location(window)? - window.geometry().loc)
    }

    /// Monitor rectangle in the compositor-global logical coordinate space.
    pub fn global_rect(&self) -> Option<Rectangle<f64, Logical>> {
        let mode = self.output.current_mode()?;
        let logical = mode.size.to_logical(1);
        let size = self.output.current_transform().transform_size(logical);
        Some(Rectangle::new(
            self.output.current_location().to_f64(),
            size.to_f64(),
        ))
    }

    /// Keep the animated camera as the output's position in the workspace
    /// `Space`. Do not transform windows to pan.
    pub fn map_output(&mut self) {
        // Do not clamp the animated value: the spring is allowed to overshoot
        // the fit target so the bounce is visible even at the first/last
        // column. The target itself is clamped by the layout.
        let location = Point::from((
            self.view_x.value.round() as i32,
            self.view_y.value.round() as i32,
        ));
        self.space.map_output(&self.output, location);
    }

    pub fn fullscreen_screen_rect(&self, workspace: &Workspace) -> Rectangle<f64, Logical> {
        Rectangle::from_size(workspace.scroll.output_size)
    }

    /// Apply the active workspace layout.
    ///
    /// Direct callers must not run this while a Move drag owns a preview;
    /// route through `EvaWM::apply_layout` or defer until the drop.
    pub fn apply_layout(&mut self, now: f64, config: AnimationConfig, animations_off: bool) {
        let index = self.active_workspace;
        let workspace = &mut self.workspaces[index];
        workspace.apply(&mut self.space);
        if workspace.mode == Mode::Scroll {
            let camera = workspace.scroll.camera_x;
            let mut config = config;
            if animations_off {
                config.off = true;
            }
            self.view_x.set_target(camera, now, config);
        }
        self.map_output();
    }

    /// Seed canvas geometry for every scroll window that does not have it yet.
    /// Existing canvas rectangles are preserved across mode switches.
    ///
    /// Newly arrived Scroll windows keep their relative Scroll layout (stacks,
    /// neighbouring columns, and gaps) while the group as a whole is translated
    /// to the center of the workspace's Canvas viewport.
    pub fn seed_canvas(&mut self, index: usize, now: f64, window_states: &WindowStates) {
        let unseeded: Vec<(Window, Rectangle<f64, Logical>)> = self.workspaces[index]
            .scroll
            .windows()
            .filter(|window| !self.workspaces[index].canvas.contains(window))
            .filter_map(|window| {
                // Prefer the settled scroll target. Seeding from a visual
                // transition rectangle would freeze a half-finished stack
                // animation into the canvas geometry.
                self.workspaces[index]
                    .scroll
                    .content_target_rect(window)
                    .or_else(|| self.current_content_world_rect(window, now, window_states))
                    .map(|rect| (window.clone(), rect))
            })
            .collect();
        if unseeded.is_empty() {
            return;
        }
        let workspace = &mut self.workspaces[index];
        let area = workspace.scroll.working_area;
        let camera = workspace.canvas.seed_camera(area);
        let offset = crate::workspace::canvas_seed_group_offset(
            unseeded.iter().map(|(_, rect)| rect),
            area,
            camera,
        );
        for (window, rect) in unseeded {
            let placed = Rectangle::new(rect.loc + offset, rect.size);
            workspace.canvas.insert(window, placed);
        }
    }

    #[cfg(any(feature = "nested", test))]
    pub fn resize_output(&mut self, size: Size<i32, Physical>) {
        let mode = smithay::output::Mode {
            size,
            refresh: 60_000,
        };
        self.output
            .change_current_state(Some(mode), None, None, None);
        self.output.set_preferred(mode);
        let logical = self
            .output
            .current_transform()
            .transform_size(size.to_logical(1));
        for workspace in &mut self.workspaces {
            workspace.scroll.set_output_size(logical);
        }
    }

    pub fn update_working_area(&mut self) {
        let zone = layer_map_for_output(&self.output).non_exclusive_zone();
        for workspace in &mut self.workspaces {
            workspace.scroll.set_working_area(zone);
        }
    }

    /// Bounds of the window's content in screen space, including an active
    /// window animation. Used to start fullscreen transitions where the
    /// window actually is, not at the output origin.
    pub fn current_content_screen_rect(
        &self,
        window: &Window,
        now: f64,
        window_states: &WindowStates,
    ) -> Option<Rectangle<f64, Logical>> {
        let rect = self.current_content_world_rect(window, now, window_states)?;
        Some(Rectangle::new(rect.loc - self.camera_location(), rect.size))
    }

    pub fn current_content_world_rect(
        &self,
        window: &Window,
        now: f64,
        window_states: &WindowStates,
    ) -> Option<Rectangle<f64, Logical>> {
        if let Some(style) = window_states.transition_style(window, now) {
            let location = if style.screen_space {
                style.rect.loc + self.camera_location()
            } else {
                style.rect.loc
            };
            return Some(Rectangle::new(location, style.rect.size));
        }
        let active = &self.workspaces[self.active_workspace];
        if active.is_fullscreen(window) {
            return Some(Rectangle::new(
                self.camera_location(),
                active.scroll.output_size,
            ));
        }
        if let Some(location) = self.space.element_location(window) {
            let committed = window.geometry().size;
            if committed.w > 0 && committed.h > 0 {
                return Some(Rectangle::new(location.to_f64(), committed.to_f64()));
            }
        }
        self.workspaces
            .iter()
            .find_map(|workspace| workspace.content_target_rect_for_mode(window))
    }

    fn layer_under_in(
        &self,
        pos: Point<f64, Logical>,
        layers: &[WlrLayer],
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let map = layer_map_for_output(&self.output);
        for &layer in layers {
            for layer_surface in map.layers_on(layer).rev() {
                let Some(geometry) = map.layer_geometry(layer_surface) else {
                    continue;
                };
                if let Some((surface, offset)) =
                    layer_surface.surface_under(pos - geometry.loc.to_f64(), WindowSurfaceType::ALL)
                {
                    return Some((surface, (geometry.loc + offset).to_f64()));
                }
            }
        }
        None
    }

    pub fn layer_above_windows_under(
        &self,
        pos: Point<f64, Logical>,
        fullscreen_above_top: bool,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let layers: &[WlrLayer] = if fullscreen_above_top {
            &[WlrLayer::Overlay]
        } else {
            &[WlrLayer::Overlay, WlrLayer::Top]
        };
        self.layer_under_in(pos, layers)
    }

    pub fn layer_below_windows_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.layer_under_in(pos, &[WlrLayer::Bottom])
    }
}

/// Centre of a rectangle in compositor-global coordinates. Used for the
/// initial cursor position and for `focus-monitor-*` pointer warps.
pub(crate) fn rect_center(rect: Rectangle<f64, Logical>) -> Point<f64, Logical> {
    rect.loc + Point::from((rect.size.w / 2.0, rect.size.h / 2.0))
}

/// Invert Overview zoom around `pivot`: maps an output-local screen point into
/// workspace world coordinates.
pub(crate) fn overview_screen_to_world(
    screen: Point<f64, Logical>,
    pivot: Point<f64, Logical>,
    camera: Point<f64, Logical>,
    zoom: f64,
) -> Point<f64, Logical> {
    let zoom = zoom.max(0.05);
    let unscaled = pivot + Point::from(((screen.x - pivot.x) / zoom, (screen.y - pivot.y) / zoom));
    camera + unscaled
}

/// Visible world rectangle for `output_size` at Overview `zoom` around `pivot`.
pub(crate) fn overview_cull_viewport(
    camera: Point<f64, Logical>,
    output_size: Size<f64, Logical>,
    pivot: Point<f64, Logical>,
    zoom: f64,
) -> Rectangle<f64, Logical> {
    if (zoom - 1.0).abs() <= 0.001 {
        return Rectangle::new(camera, output_size);
    }
    let top_left = overview_screen_to_world(Point::from((0.0, 0.0)), pivot, camera, zoom);
    let bottom_right = overview_screen_to_world(
        Point::from((output_size.w, output_size.h)),
        pivot,
        camera,
        zoom,
    );
    Rectangle::new(
        top_left,
        Size::from((bottom_right.x - top_left.x, bottom_right.y - top_left.y)),
    )
}

/// Clamp a global pointer point to an output rectangle. When `restrict` is
/// set, only that output is considered (used while dragging). Otherwise the
/// nearest output is chosen, so the pointer cannot get lost in gaps between
/// monitors.
pub(crate) fn clamp_pointer_to_output(
    rects: &[Option<Rectangle<f64, Logical>>],
    point: Point<f64, Logical>,
    restrict: Option<usize>,
) -> Option<(usize, Point<f64, Logical>)> {
    let mut best: Option<(f64, usize, Point<f64, Logical>)> = None;
    for (index, rect) in rects.iter().enumerate() {
        if restrict.is_some_and(|restrict| restrict != index) {
            continue;
        }
        let Some(rect) = rect else {
            continue;
        };
        let max_x = (rect.loc.x + rect.size.w - 1.0).max(rect.loc.x);
        let max_y = (rect.loc.y + rect.size.h - 1.0).max(rect.loc.y);
        let clamped = Point::from((
            point.x.clamp(rect.loc.x, max_x),
            point.y.clamp(rect.loc.y, max_y),
        ));
        let dx = clamped.x - point.x;
        let dy = clamped.y - point.y;
        let distance = dx * dx + dy * dy;
        if best.as_ref().is_none_or(|(old, _, _)| distance < *old) {
            best = Some((distance, index, clamped));
        }
    }
    best.map(|(_, index, point)| (index, point))
}

pub(crate) fn output_in_direction(
    rects: &[Option<Rectangle<f64, Logical>>],
    current: usize,
    direction: Direction,
) -> Option<usize> {
    let current_rect = rects.get(current).copied().flatten()?;
    let current_center = rect_center(current_rect);
    let mut best: Option<(f64, usize)> = None;

    for (index, rect) in rects.iter().enumerate() {
        if index == current {
            continue;
        }
        let Some(rect) = rect else {
            continue;
        };
        let center = rect_center(*rect);
        let (primary, perpendicular, overlaps) = match direction {
            Direction::Left if center.x < current_center.x => (
                current_center.x - center.x,
                (center.y - current_center.y).abs(),
                ranges_overlap(
                    rect.loc.y,
                    rect.size.h,
                    current_rect.loc.y,
                    current_rect.size.h,
                ),
            ),
            Direction::Right if center.x > current_center.x => (
                center.x - current_center.x,
                (center.y - current_center.y).abs(),
                ranges_overlap(
                    rect.loc.y,
                    rect.size.h,
                    current_rect.loc.y,
                    current_rect.size.h,
                ),
            ),
            Direction::Up if center.y < current_center.y => (
                current_center.y - center.y,
                (center.x - current_center.x).abs(),
                ranges_overlap(
                    rect.loc.x,
                    rect.size.w,
                    current_rect.loc.x,
                    current_rect.size.w,
                ),
            ),
            Direction::Down if center.y > current_center.y => (
                center.y - current_center.y,
                (center.x - current_center.x).abs(),
                ranges_overlap(
                    rect.loc.x,
                    rect.size.w,
                    current_rect.loc.x,
                    current_rect.size.w,
                ),
            ),
            _ => continue,
        };
        let score = primary + perpendicular * 0.5 + if overlaps { 0.0 } else { 10_000.0 };
        if best.as_ref().is_none_or(|(old, _)| score < *old) {
            best = Some((score, index));
        }
    }

    best.map(|(_, index)| index)
}

/// Place explicitly positioned outputs first, then fill the gaps left-to-right
/// for outputs without a usable position. Overlapping explicit positions fall
/// back to automatic placement.
pub(crate) fn auto_output_positions(
    sizes: &[Size<i32, Logical>],
    configured: &[Option<Point<i32, Logical>>],
) -> Vec<Point<i32, Logical>> {
    let mut result = vec![None; sizes.len()];
    let mut placed: Vec<Rectangle<f64, Logical>> = Vec::new();

    for (index, position) in configured.iter().enumerate().take(sizes.len()) {
        let Some(position) = position else {
            continue;
        };
        let rect = Rectangle::new(position.to_f64(), sizes[index].to_f64());
        if placed.iter().any(|other| other.overlaps(rect)) {
            continue;
        }
        result[index] = Some(*position);
        placed.push(rect);
    }

    let mut cursor = 0.0;
    for (index, size) in sizes.iter().enumerate() {
        if result[index].is_some() {
            continue;
        }
        let size = size.to_f64();
        loop {
            let rect = Rectangle::new(Point::from((cursor, 0.0)), size);
            if let Some(other) = placed.iter().find(|other| other.overlaps(rect)) {
                cursor = other.loc.x + other.size.w;
                continue;
            }
            result[index] = Some(Point::from((cursor.round() as i32, 0)));
            placed.push(rect);
            cursor += size.w;
            break;
        }
    }

    result
        .into_iter()
        .map(|position| position.unwrap_or_default())
        .collect()
}

fn ranges_overlap(a: f64, alen: f64, b: f64, blen: f64) -> bool {
    a < b + blen && b < a + alen
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Direction;
    use smithay::output::{Mode, PhysicalProperties, Subpixel};
    use smithay::utils::Transform;

    fn test_output() -> Output {
        let output = Output::new(
            "test-output".to_string(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "EvaWM".into(),
                model: "Test".into(),
                serial_number: "test".into(),
            },
        );
        let mode = Mode {
            size: (1280, 800).into(),
            refresh: 60_000,
        };
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            None,
            Some((0, 0).into()),
        );
        output.set_preferred(mode);
        output
    }

    #[test]
    fn output_state_starts_with_one_dynamic_workspace() {
        let state = OutputState::new(test_output(), &Config::default());
        assert_eq!(state.workspaces.len(), 1);
        assert_eq!(state.active_workspace, 0);
        assert_eq!(state.workspaces[0].mode, crate::workspace::Mode::Scroll);
        assert!(state.workspaces[0].canvas.rects.is_empty());
        assert_eq!(
            state.space.output_geometry(&state.output),
            Some(smithay::utils::Rectangle::new(
                Point::from((0, 0)),
                Size::from((1280, 800)),
            ))
        );
    }

    #[test]
    fn fullscreen_overlay_is_pinned_to_output_origin() {
        let mut state = OutputState::new(test_output(), &Config::default());
        state.view_y.value = 50.0;
        state.map_output();
        assert_eq!(
            state.fullscreen_screen_rect(&state.workspaces[0]),
            Rectangle::new(Point::from((0.0, 0.0)), Size::from((1280.0, 800.0)))
        );
    }

    #[test]
    fn ensure_workspace_copies_layout_options() {
        let mut state = OutputState::new(test_output(), &Config::default());
        state.ensure_workspace(2);
        assert_eq!(state.workspaces.len(), 3);
        for workspace in &state.workspaces {
            let layout = &workspace.scroll;
            assert_eq!(layout.gap, state.workspaces[0].gap());
            assert_eq!(
                layout.default_proportion,
                state.workspaces[0].scroll.default_proportion
            );
            assert_eq!(layout.working_area, state.workspaces[0].working_area());
            assert!(workspace.canvas.rects.is_empty());
        }
    }

    #[test]
    fn ensure_workspace_is_capped_at_the_bar_limit() {
        let mut state = OutputState::new(test_output(), &Config::default());
        state.ensure_workspace(20);
        assert_eq!(state.workspaces.len(), MAX_WORKSPACES);
    }

    #[test]
    fn cleanup_keeps_the_active_empty_workspace_and_the_trailing_one() {
        let mut state = OutputState::new(test_output(), &Config::default());
        state.ensure_workspace(1);
        state.clean_up_workspaces();
        assert_eq!(state.workspaces.len(), 2);
        assert_eq!(state.active_workspace, 0);
    }

    #[test]
    fn cleanup_removes_empty_workspaces_around_the_active_one() {
        let mut state = OutputState::new(test_output(), &Config::default());
        state.ensure_workspace(4);
        let active_id = state.workspaces[4].id;
        state.active_workspace = 4;
        state.clean_up_workspaces();
        assert_eq!(state.workspaces.len(), 1);
        assert_eq!(state.workspaces[0].id, active_id);
        assert_eq!(state.active_workspace, 0);
    }

    #[test]
    fn seed_canvas_is_a_noop_without_windows() {
        let mut state = OutputState::new(test_output(), &Config::default());
        state.seed_canvas(0, 0.0, &WindowStates::default());
        assert!(state.workspaces[0].canvas.rects.is_empty());
        assert!(state.workspaces[0].canvas.z_order.is_empty());
    }

    #[test]
    fn output_global_rect_respects_transform_and_position() {
        let output = test_output();
        output.change_current_state(
            Some(Mode {
                size: (1920, 1080).into(),
                refresh: 60_000,
            }),
            Some(Transform::_90),
            None,
            Some((100, 200).into()),
        );
        let state = OutputState::new(output, &Config::default());
        assert_eq!(
            state.global_rect(),
            Some(Rectangle::new(
                Point::from((100.0, 200.0)),
                Size::from((1080.0, 1920.0)),
            ))
        );
    }

    #[test]
    fn output_lookup_uses_global_rectangles() {
        let rects = [
            Some(Rectangle::new(
                Point::from((0.0, 0.0)),
                Size::from((1920.0, 1080.0)),
            )),
            Some(Rectangle::new(
                Point::from((1920.0, 0.0)),
                Size::from((1080.0, 1920.0)),
            )),
        ];
        assert_eq!(output_in_direction(&rects, 0, Direction::Right), Some(1));
        assert_eq!(output_in_direction(&rects, 1, Direction::Left), Some(0));
        assert_eq!(output_in_direction(&rects, 0, Direction::Left), None);
    }

    #[test]
    fn pointer_clamps_to_output_rectangles() {
        let rects = [
            Some(Rectangle::new(
                Point::from((0.0, 0.0)),
                Size::from((1920.0, 1080.0)),
            )),
            Some(Rectangle::new(
                Point::from((1920.0, 0.0)),
                Size::from((1920.0, 1080.0)),
            )),
        ];
        assert_eq!(
            clamp_pointer_to_output(&rects, Point::from((100.0, 100.0)), None),
            Some((0, Point::from((100.0, 100.0))))
        );
        assert_eq!(
            clamp_pointer_to_output(&rects, Point::from((2500.0, 100.0)), None),
            Some((1, Point::from((2500.0, 100.0))))
        );
        assert_eq!(
            clamp_pointer_to_output(&rects, Point::from((-50.0, -20.0)), None),
            Some((0, Point::from((0.0, 0.0))))
        );
        assert_eq!(
            clamp_pointer_to_output(&rects, Point::from((2500.0, 100.0)), Some(0)),
            Some((0, Point::from((1919.0, 100.0))))
        );
    }

    #[test]
    fn rectangle_center_uses_global_location() {
        let primary = Rectangle::new(Point::from((0.0, 1080.0)), Size::from((1920.0, 1080.0)));
        assert_eq!(rect_center(primary), Point::from((960.0, 1620.0)));

        let rotated = Rectangle::new(Point::from((1920.0, 0.0)), Size::from((1080.0, 1920.0)));
        assert_eq!(rect_center(rotated), Point::from((2460.0, 960.0)));
    }

    #[test]
    fn auto_layout_places_unconfigured_outputs_left_to_right() {
        let sizes = [
            Size::from((1920, 1080)),
            Size::from((1080, 1920)),
            Size::from((1280, 720)),
        ];
        let positions = auto_output_positions(&sizes, &[None, None, None]);
        assert_eq!(
            positions,
            vec![
                Point::from((0, 0)),
                Point::from((1920, 0)),
                Point::from((3000, 0)),
            ]
        );

        let positions = auto_output_positions(&sizes, &[Some(Point::from((3000, 0))), None, None]);
        assert_eq!(positions[0], Point::from((3000, 0)));
        assert_eq!(positions[1], Point::from((0, 0)));
        assert_eq!(positions[2], Point::from((1080, 0)));
    }

    #[test]
    fn auto_layout_rejects_overlapping_explicit_positions() {
        let sizes = [Size::from((1920, 1080)), Size::from((1920, 1080))];
        let positions = auto_output_positions(
            &sizes,
            &[Some(Point::from((0, 0))), Some(Point::from((0, 0)))],
        );
        assert_eq!(positions[0], Point::from((0, 0)));
        assert_eq!(positions[1], Point::from((1920, 0)));
    }

    #[test]
    fn resize_output_updates_every_workspace() {
        let mut state = OutputState::new(test_output(), &Config::default());
        state.ensure_workspace(2);
        state.resize_output(Size::from((1920, 1080)));
        for workspace in &state.workspaces {
            let layout = &workspace.scroll;
            assert_eq!(layout.output_size, Size::from((1920.0, 1080.0)));
            assert_eq!(layout.working_area.size, Size::from((1920.0, 1080.0)));
        }
    }

    #[test]
    fn overview_screen_to_world_inverts_zoom_around_pivot() {
        let pivot = Point::from((960.0, 560.0));
        let camera = Point::from((200.0, -100.0));

        // Pivot stays invariant at any zoom.
        assert_eq!(
            overview_screen_to_world(pivot, pivot, camera, 0.5),
            camera + pivot
        );
        // At 0.5 zoom, halfway from pivot to screen left edge maps to x = 0 in
        // unscaled screen coordinates (world x = camera.x).
        assert_eq!(
            overview_screen_to_world(Point::from((480.0, 560.0)), pivot, camera, 0.5),
            Point::from((200.0, 460.0))
        );
        // At 1.0 zoom, identity screen + camera.
        assert_eq!(
            overview_screen_to_world(Point::from((120.0, 340.0)), pivot, camera, 1.0),
            Point::from((320.0, 240.0))
        );
    }

    #[test]
    fn overview_cull_viewport_expands_by_inverse_zoom() {
        let camera = Point::from((100.0, 50.0));
        let output_size = Size::from((1920.0, 1080.0));
        let pivot = Point::from((960.0, 540.0));

        // Unzoomed matches the exact output viewport.
        assert_eq!(
            overview_cull_viewport(camera, output_size, pivot, 1.0),
            Rectangle::new(camera, output_size)
        );
        // At 0.5 zoom centered at (960, 540), visible world area doubles in
        // width and height around the camera center.
        let zoomed = overview_cull_viewport(camera, output_size, pivot, 0.5);
        assert_eq!(zoomed.loc, Point::from((-860.0, -490.0)));
        assert_eq!(zoomed.size, Size::from((3840.0, 2160.0)));
    }
}
