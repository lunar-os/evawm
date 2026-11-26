//! Workspace identity and per-mode layout state.
//!
//! A workspace owns the scroll layout, the canvas layout, and the shared
//! focused window. Mode-specific callers access `.scroll` or `.canvas`
//! explicitly so scroll methods cannot accidentally run in canvas mode.

use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
};

use smithay::{
    desktop::{Space, Window},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle, Size},
};

use crate::layout::Layout;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Scroll,
    Canvas,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Free canvas geometry for one workspace.
#[derive(Debug, Default)]
pub struct CanvasLayout {
    pub rects: HashMap<Window, Rectangle<f64, Logical>>,
    pub z_order: Vec<Window>,
    /// Previous canvas rectangle while a window is maximized.
    pub maximized: Option<(Window, Rectangle<f64, Logical>)>,
    /// Most recent non-maximized rectangle for this window. Used to make
    /// maximize a real toggle even if a no-op resize or another action cleared
    /// `maximized` while the window was already at the viewport size.
    pub last_restore: Option<(Window, Rectangle<f64, Logical>)>,
    /// Last canvas camera position saved when leaving canvas mode or switching
    /// away from this workspace. Used to place newly arrived Scroll windows
    /// into the view the user left behind instead of at Scroll strip offsets.
    pub last_camera: Option<Point<f64, Logical>>,
}

/// Bounding box enclosing a non-empty set of rectangles.
fn bounding_box<'a>(
    rects: impl IntoIterator<Item = &'a Rectangle<f64, Logical>>,
) -> Option<Rectangle<f64, Logical>> {
    rects.into_iter().copied().reduce(Rectangle::merge)
}

/// Round a logical canvas rectangle's size to integer client pixels (>= 1x1).
pub(crate) fn canvas_pixel_size(rect: Rectangle<f64, Logical>) -> Size<i32, Logical> {
    Size::from((
        rect.size.w.round().max(1.0) as i32,
        rect.size.h.round().max(1.0) as i32,
    ))
}

/// Translation offset that centers a group of Scroll rectangles inside
/// `camera`'s visible working area while preserving every window's relative
/// position (stacked tiles, neighbouring columns, and gaps).
pub(crate) fn canvas_seed_group_offset<'a>(
    rects: impl IntoIterator<Item = &'a Rectangle<f64, Logical>>,
    area: Rectangle<f64, Logical>,
    camera: Point<f64, Logical>,
) -> Point<f64, Logical> {
    let Some(bounds) = bounding_box(rects) else {
        return Point::from((0.0, 0.0));
    };
    let bounds_center = bounds.loc + Point::from((bounds.size.w / 2.0, bounds.size.h / 2.0));
    let view_center = camera + area.loc + Point::from((area.size.w / 2.0, area.size.h / 2.0));
    view_center - bounds_center
}

fn bounding_box_center_camera<'a>(
    rects: impl IntoIterator<Item = &'a Rectangle<f64, Logical>>,
    area: Rectangle<f64, Logical>,
) -> Point<f64, Logical> {
    let Some(bounds) = bounding_box(rects) else {
        return Point::from((0.0, 0.0));
    };
    Point::from((
        bounds.loc.x + bounds.size.w / 2.0 - area.loc.x - area.size.w / 2.0,
        bounds.loc.y + bounds.size.h / 2.0 - area.loc.y - area.size.h / 2.0,
    ))
}

/// Compare two logical rectangles with a small tolerance. Canvas geometry is
/// fractional; comparing exact f64 values makes no-op resizes look real.
pub(crate) fn rects_approx_eq(a: Rectangle<f64, Logical>, b: Rectangle<f64, Logical>) -> bool {
    (a.loc.x - b.loc.x).abs() <= 0.5
        && (a.loc.y - b.loc.y).abs() <= 0.5
        && (a.size.w - b.size.w).abs() <= 0.5
        && (a.size.h - b.size.h).abs() <= 0.5
}

#[derive(Debug)]
pub struct CanvasRemovedWindow {
    pub window: Window,
    pub rect: Rectangle<f64, Logical>,
    pub z_index: usize,
}

impl CanvasLayout {
    /// Camera top-left to use when placing newly arrived Scroll windows into
    /// this workspace's Canvas.
    pub fn seed_camera(&self, area: Rectangle<f64, Logical>) -> Point<f64, Logical> {
        self.last_camera
            .unwrap_or_else(|| bounding_box_center_camera(self.rects.values(), area))
    }

    pub fn contains(&self, window: &Window) -> bool {
        self.rects.contains_key(window)
    }

    pub fn rect(&self, window: &Window) -> Option<Rectangle<f64, Logical>> {
        self.rects.get(window).copied()
    }

    pub fn set_rect(&mut self, window: &Window, rect: Rectangle<f64, Logical>) {
        if let Some(existing) = self.rects.get_mut(window) {
            *existing = rect;
        }
    }

    pub fn raise(&mut self, window: &Window) {
        if let Some(index) = self.z_order.iter().position(|entry| entry == window) {
            let window = self.z_order.remove(index);
            self.z_order.push(window);
        }
    }

    pub fn focused_window(&self) -> Option<&Window> {
        self.z_order.last()
    }

    pub fn focus_direction(
        &self,
        current: Option<&Window>,
        direction: Direction,
    ) -> Option<Window> {
        let current_rect = current.and_then(|window| self.rect(window))?;
        let current_center =
            current_rect.loc + Point::from((current_rect.size.w / 2.0, current_rect.size.h / 2.0));
        let mut best: Option<(f64, Window)> = None;
        for window in &self.z_order {
            if current == Some(window) {
                continue;
            }
            let Some(rect) = self.rect(window) else {
                continue;
            };
            let center = rect.loc + Point::from((rect.size.w / 2.0, rect.size.h / 2.0));
            let dx = center.x - current_center.x;
            let dy = center.y - current_center.y;
            let in_direction = match direction {
                Direction::Left => dx < -0.5,
                Direction::Right => dx > 0.5,
                Direction::Up => dy < -0.5,
                Direction::Down => dy > 0.5,
            };
            if !in_direction {
                continue;
            }
            let score = dx * dx + dy * dy;
            if best
                .as_ref()
                .is_none_or(|(best_score, _)| score < *best_score)
            {
                best = Some((score, window.clone()));
            }
        }
        best.map(|(_, window)| window)
    }

    pub fn remove(&mut self, window: &Window) {
        self.rects.remove(window);
        self.z_order.retain(|entry| entry != window);
        if self.maximized.as_ref().is_some_and(|(w, _)| w == window) {
            self.maximized = None;
        }
        if self.last_restore.as_ref().is_some_and(|(w, _)| w == window) {
            self.last_restore = None;
        }
    }

    pub fn take(&mut self, window: &Window) -> Option<CanvasRemovedWindow> {
        let rect = self.rects.remove(window)?;
        let z_index = self
            .z_order
            .iter()
            .position(|entry| entry == window)
            .unwrap_or(0);
        self.z_order.retain(|entry| entry != window);
        if self.maximized.as_ref().is_some_and(|(w, _)| w == window) {
            self.maximized = None;
        }
        if self.last_restore.as_ref().is_some_and(|(w, _)| w == window) {
            self.last_restore = None;
        }
        Some(CanvasRemovedWindow {
            window: window.clone(),
            rect,
            z_index,
        })
    }

    pub fn insert(&mut self, window: Window, rect: Rectangle<f64, Logical>) {
        if !self.z_order.iter().any(|entry| entry == &window) {
            self.z_order.push(window.clone());
        }
        self.rects.insert(window, rect);
    }

    pub fn insert_at(&mut self, window: Window, rect: Rectangle<f64, Logical>, index: usize) {
        self.rects.insert(window.clone(), rect);
        if let Some(existing) = self.z_order.iter().position(|entry| entry == &window) {
            self.z_order.remove(existing);
        }
        let index = index.min(self.z_order.len());
        self.z_order.insert(index, window);
    }
}

#[derive(Debug)]
pub struct Workspace {
    /// Stable IPC identifier. Never reused for the life of the process.
    pub id: u64,
    pub mode: Mode,
    pub scroll: Layout,
    pub canvas: CanvasLayout,
    focus: Option<Window>,
    pub fullscreen: Option<Window>,
}

static NEXT_WORKSPACE_ID: AtomicU64 = AtomicU64::new(1);

/// Maximum dynamic workspaces per output (also matches `Mod+1..9`).
pub const MAX_WORKSPACES: usize = 9;

impl Workspace {
    pub fn new(layout: Layout) -> Self {
        Self {
            id: NEXT_WORKSPACE_ID.fetch_add(1, Ordering::Relaxed),
            mode: Mode::Scroll,
            scroll: layout,
            canvas: CanvasLayout::default(),
            focus: None,
            fullscreen: None,
        }
    }

    pub fn contains(&self, window: &Window) -> bool {
        self.scroll.tile_index(window).is_some() || self.canvas.contains(window)
    }

    pub fn has_windows(&self) -> bool {
        self.scroll.windows().next().is_some() || !self.canvas.z_order.is_empty()
    }

    pub fn focused_window(&self) -> Option<&Window> {
        match self.mode {
            Mode::Scroll => self.scroll.focused_window(),
            Mode::Canvas => self.focus.as_ref().filter(|w| self.canvas.contains(w)),
        }
    }

    pub fn set_focus(&mut self, window: Option<Window>) {
        self.focus = window;
    }

    pub fn sync_scroll_focus(&mut self) {
        if self.mode == Mode::Scroll {
            self.focus = self.scroll.focused_window().cloned();
        }
    }

    /// Force a client-size configure for the active mode. Canvas rectangles
    /// and scroll tiles keep independent geometry, so entering either mode
    /// must re-assert its sizes; otherwise a client can stay at the previous
    /// mode's size while the frame uses the new mode's rectangle.
    pub fn refresh_configured_sizes(&mut self) {
        self.scroll.invalidate_configured_sizes();
        if self.mode == Mode::Scroll {
            return;
        }
        for window in &self.canvas.z_order {
            if self.fullscreen.as_ref() == Some(window) {
                continue;
            }
            let Some(rect) = self.canvas.rects.get(window).copied() else {
                continue;
            };
            self.scroll
                .configure_window_size(window, canvas_pixel_size(rect));
        }
    }

    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<&Window> {
        self.scroll
            .windows()
            .find(|window| {
                window
                    .toplevel()
                    .is_some_and(|toplevel| toplevel.wl_surface() == surface)
            })
            .or_else(|| {
                self.canvas.z_order.iter().find(|window| {
                    window
                        .toplevel()
                        .is_some_and(|toplevel| toplevel.wl_surface() == surface)
                })
            })
    }

    pub fn remove_window(&mut self, window: &Window) {
        self.scroll.remove_window(window);
        self.canvas.remove(window);
        if self.fullscreen.as_ref() == Some(window) {
            self.fullscreen = None;
        }
        if self.focus.as_ref() == Some(window) {
            self.focus = match self.mode {
                Mode::Scroll => self.scroll.focused_window().cloned(),
                Mode::Canvas => self.canvas.focused_window().cloned(),
            };
        }
    }

    pub fn is_fullscreen(&self, window: &Window) -> bool {
        self.fullscreen.as_ref() == Some(window)
    }

    pub fn fullscreen_window(&self) -> Option<&Window> {
        self.fullscreen.as_ref()
    }

    pub fn has_fullscreen(&self) -> bool {
        self.fullscreen.is_some()
    }

    pub fn enter_fullscreen(&mut self, window: Window) {
        self.fullscreen = Some(window);
    }

    pub fn exit_fullscreen(&mut self) -> Option<Window> {
        self.fullscreen.take()
    }

    pub fn windows(&self) -> impl Iterator<Item = &Window> {
        self.scroll.windows()
    }

    /// Canvas rectangle used by `Mod+F` maximize at the origin.
    #[cfg(test)]
    pub fn canvas_maximized_rect(&self) -> Rectangle<f64, Logical> {
        self.canvas_maximized_rect_at(Point::from((0.0, 0.0)))
    }

    /// Maximize into the viewport whose top-left is `camera` in world space.
    pub fn canvas_maximized_rect_at(&self, camera: Point<f64, Logical>) -> Rectangle<f64, Logical> {
        let area = self.scroll.working_area;
        let gap = self.scroll.gap;
        let inset = self.scroll.frame_inset();
        Rectangle::new(
            area.loc + camera + Point::from((gap + inset, gap + inset)),
            Size::from((
                (area.size.w - 2.0 * gap - 2.0 * inset).max(1.0),
                (area.size.h - 2.0 * gap - 2.0 * inset).max(1.0),
            )),
        )
    }

    /// Bounding box of all windows in this workspace's canvas layout.
    pub fn canvas_bounds(&self) -> Option<Rectangle<f64, Logical>> {
        self.canvas.rects.values().copied().reduce(Rectangle::merge)
    }

    /// Output-travel target for the current mode.
    pub fn camera_target(&self) -> Point<f64, Logical> {
        match self.mode {
            Mode::Scroll => Point::from((self.scroll.camera_x, 0.0)),
            Mode::Canvas => {
                let focused = self
                    .focused_window()
                    .or_else(|| self.canvas.focused_window());
                let rect = focused.and_then(|window| self.canvas.rects.get(window));
                let Some(rect) = rect else {
                    return Point::from((0.0, 0.0));
                };
                let area = self.scroll.working_area;
                Point::from((
                    rect.loc.x + rect.size.w / 2.0 - area.loc.x - area.size.w / 2.0,
                    rect.loc.y + rect.size.h / 2.0 - area.loc.y - area.size.h / 2.0,
                ))
            }
        }
    }

    /// Target content rectangle for layout transitions in the active mode.
    pub fn content_target_rect_for_mode(&self, window: &Window) -> Option<Rectangle<f64, Logical>> {
        match self.mode {
            Mode::Scroll => self.scroll.content_target_rect(window),
            Mode::Canvas => self.canvas.rect(window),
        }
    }

    pub fn apply(&mut self, space: &mut Space<Window>) {
        match self.mode {
            Mode::Scroll => self.scroll.apply(space, self.fullscreen.as_ref()),
            Mode::Canvas => self.map_canvas(space, true),
        }
    }

    pub fn apply_windows(&mut self, space: &mut Space<Window>) {
        match self.mode {
            Mode::Scroll => self.scroll.apply_windows(space, self.fullscreen.as_ref()),
            Mode::Canvas => self.map_canvas(space, false),
        }
    }

    pub fn output_size(&self) -> Size<f64, Logical> {
        self.scroll.output_size
    }

    pub fn working_area(&self) -> Rectangle<f64, Logical> {
        self.scroll.working_area
    }

    pub fn gap(&self) -> f64 {
        self.scroll.gap
    }

    pub fn frame_inset(&self) -> f64 {
        self.scroll.frame_inset()
    }

    pub fn window_is_fixed_size(&self, window: &Window) -> bool {
        self.scroll.window_is_fixed_size(window)
    }

    pub fn update_window_constraints(
        &mut self,
        window: &Window,
        min_size: Size<i32, Logical>,
        max_size: Size<i32, Logical>,
    ) -> bool {
        self.scroll
            .update_window_constraints(window, min_size, max_size)
    }

    /// Recompute `self.focus` from the active mode after removing a window.
    pub fn refresh_focus_for_mode(&mut self) {
        self.focus = match self.mode {
            Mode::Scroll => self.scroll.focused_window().cloned(),
            Mode::Canvas => self.canvas.focused_window().cloned(),
        };
    }

    fn map_canvas(&self, space: &mut Space<Window>, raise: bool) {
        for window in &self.canvas.z_order {
            let Some(rect) = self.canvas.rects.get(window) else {
                continue;
            };
            let location = Point::from((rect.loc.x.round() as i32, rect.loc.y.round() as i32));
            space.map_element(window.clone(), location, false);
        }
        if raise {
            // `map_element` only relocates an existing element; it does not
            // change Space z-order. Re-raise in canvas order on full apply.
            for window in &self.canvas.z_order {
                space.raise_element(window, false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BorderConfig;
    use smithay::utils::Size;

    fn layout() -> Layout {
        Layout::new(
            Size::from((1000, 800)),
            8.0,
            0.5,
            BorderConfig::default(),
            BorderConfig::outer(),
        )
    }

    #[test]
    fn canvas_layout_starts_empty() {
        let canvas = CanvasLayout::default();
        assert!(canvas.rects.is_empty());
        assert!(canvas.z_order.is_empty());
        assert_eq!(Mode::default(), Mode::Scroll);
    }

    #[test]
    fn rectangles_compare_with_a_subpixel_tolerance() {
        let a = Rectangle::new(Point::from((10.0, 20.0)), Size::from((100.0, 50.0)));
        let b = Rectangle::new(Point::from((10.25, 19.75)), Size::from((100.4, 50.2)));
        assert!(rects_approx_eq(a, b));
        assert!(!rects_approx_eq(
            a,
            Rectangle::new(Point::from((11.0, 20.0)), Size::from((100.0, 50.0)))
        ));
    }

    #[test]
    fn canvas_maximize_uses_working_area_gaps_and_border() {
        let workspace = Workspace::new(layout());
        let rect = workspace.canvas_maximized_rect();
        let inset = workspace.scroll.frame_inset();
        assert_eq!(rect.loc, Point::from((8.0 + inset, 8.0 + inset)));
        assert_eq!(
            rect.size,
            Size::from((1000.0 - 16.0 - 2.0 * inset, 800.0 - 16.0 - 2.0 * inset,))
        );
    }

    #[test]
    fn canvas_maximize_follows_the_current_view() {
        let workspace = Workspace::new(layout());
        let inset = workspace.scroll.frame_inset();
        let rect = workspace.canvas_maximized_rect_at(Point::from((120.0, -45.0)));
        assert_eq!(rect.loc, Point::from((128.0 + inset, -37.0 + inset)));
        assert!(workspace.canvas_bounds().is_none());
    }

    #[test]
    fn camera_target_follows_mode_and_focus() {
        let mut workspace = Workspace::new(layout());
        workspace.scroll.camera_x = 123.0;
        assert_eq!(workspace.camera_target(), Point::from((123.0, 0.0)));

        workspace.mode = Mode::Canvas;
        assert_eq!(workspace.camera_target(), Point::from((0.0, 0.0)));
    }

    #[test]
    fn canvas_seed_group_preserves_scroll_layout_centered_in_saved_view() {
        let area = Rectangle::new(Point::from((0.0, 30.0)), Size::from((1000.0, 800.0)));
        let camera = Point::from((-400.0, 200.0));
        // View center is (-400 + 0 + 500, 200 + 30 + 400) = (100, 630).

        // 2 stacked windows in one Scroll column + 1 window to the right, far
        // away at Scroll x = 2800..3610.
        let top_left = Rectangle::new(Point::from((2800.0, 10.0)), Size::from((400.0, 380.0)));
        let bottom_left = Rectangle::new(Point::from((2800.0, 400.0)), Size::from((400.0, 380.0)));
        let right = Rectangle::new(Point::from((3210.0, 10.0)), Size::from((400.0, 770.0)));
        let scroll_rects = [top_left, bottom_left, right];

        let offset = canvas_seed_group_offset(scroll_rects.iter(), area, camera);
        let placed: Vec<Rectangle<f64, Logical>> = scroll_rects
            .iter()
            .map(|r| Rectangle::new(r.loc + offset, r.size))
            .collect();

        // Relative layout is preserved identically (stack alignment + gaps).
        assert_eq!(placed[0].loc.x, placed[1].loc.x);
        assert_eq!(placed[1].loc.y - (placed[0].loc.y + placed[0].size.h), 10.0);
        assert_eq!(placed[2].loc.x - (placed[0].loc.x + placed[0].size.w), 10.0);
        assert_eq!(placed[2].loc.y, placed[0].loc.y);

        // And the whole 3-window group is centered at the saved Canvas view center.
        let bounds = bounding_box(placed.iter()).expect("non-empty group");
        let group_center = bounds.loc + Point::from((bounds.size.w / 2.0, bounds.size.h / 2.0));
        assert_eq!(group_center, Point::from((100.0, 630.0)));
    }

    #[test]
    fn seed_camera_prefers_last_camera_and_falls_back_to_cluster_center() {
        let area = Rectangle::new(Point::from((0.0, 0.0)), Size::from((1000.0, 800.0)));
        let rects = [
            Rectangle::new(Point::from((100.0, 200.0)), Size::from((200.0, 100.0))),
            Rectangle::new(Point::from((500.0, 400.0)), Size::from((200.0, 200.0))),
        ];
        // Bounding box is [100..700, 200..600] -> center (400, 400).
        // Camera centering 1000x800 on (400, 400) is (-100, 0).
        assert_eq!(
            bounding_box_center_camera(rects.iter(), area),
            Point::from((-100.0, 0.0))
        );

        let mut canvas = CanvasLayout::default();
        assert_eq!(canvas.seed_camera(area), Point::from((0.0, 0.0)));
        canvas.last_camera = Some(Point::from((350.0, -120.0)));
        assert_eq!(canvas.seed_camera(area), Point::from((350.0, -120.0)));
    }
}
