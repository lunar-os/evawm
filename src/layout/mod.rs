use crate::config::BorderConfig;
use smithay::{
    desktop::{Space, Window},
    input::pointer::CursorIcon,
    utils::{Logical, Point, Rectangle, Size},
};

mod camera;
pub(crate) mod constraints;
mod geometry;
mod movement;

#[cfg(test)]
mod tests;

use constraints::configure_tile;
use geometry::resolve_width;

/// Minimum tile height used by interactive and keyboard resize.
pub const MIN_TILE_HEIGHT: f64 = 80.0;
/// Minimum column width.
pub const MIN_COLUMN_WIDTH: f64 = 120.0;

#[derive(Debug, Clone)]
pub struct Tile {
    pub window: Window,
    /// Fixed tile height set by resize; `None` shares the remaining column
    /// height with the other auto tiles.
    pub fixed_height: Option<f64>,
    /// Target height from the last layout pass.
    pub height: f64,
    /// Actual committed height used for stacking and hit-testing. Clients such
    /// as foot round to character cells, so this can be smaller than `height`.
    /// Includes the border inset on both sides.
    pub rendered_height: f64,
    /// Visual top edge of the tile including its border.
    pub y: f64,
    /// Top edge from the target layout, using target tile heights rather than
    /// the client's committed heights. Used as the target for layout
    /// transitions.
    pub target_y: f64,
    /// Cached xdg min/max size constraints, updated on commit.
    pub min_size: Size<i32, Logical>,
    pub max_size: Size<i32, Logical>,
    /// Size of the most recent configure sent to the client.
    configured_size: Option<Size<i32, Logical>>,
    /// Whether the last configure had the xdg FULLSCREEN state set.
    fullscreen_configured: bool,
}

#[derive(Debug, Clone)]
pub struct Column {
    pub tiles: Vec<Tile>,
    pub active: usize,
    pub width: f64,
    pub x: f64,
    /// Set for columns still using the configured default width, so an output
    /// or working-area resize keeps the niri proportion instead of a stale
    /// pixel width.
    proportion: Option<f64>,
    /// Width and proportion before `maximize`, restored on the next toggle.
    saved: Option<(f64, Option<f64>)>,
    /// Width and proportion before `expand-column-to-available-width`,
    /// restored when the action is invoked again.
    expanded: Option<(f64, Option<f64>)>,
}

impl Column {
    pub(super) fn single(tile: Tile, width: f64, proportion: Option<f64>) -> Self {
        Self {
            tiles: vec![tile],
            active: 0,
            width,
            x: 0.0,
            proportion,
            saved: None,
            expanded: None,
        }
    }

    /// Set an explicit width, detaching it from the configured proportion.
    pub fn set_fixed_width(&mut self, width: f64) {
        self.width = width;
        self.proportion = None;
        self.saved = None;
        self.expanded = None;
    }

    pub fn active_tile(&self) -> Option<&Tile> {
        self.tiles.get(self.active)
    }

    pub fn active_window(&self) -> Option<&Window> {
        self.active_tile().map(|tile| &tile.window)
    }

    /// Whether this column is expanded by `maximize-column`.
    pub(crate) fn is_maximized(&self) -> bool {
        self.saved.is_some()
    }
}

/// Which tile edges an interactive resize grabbed. Niri picks by thirds of
/// the tile: left/right third for width, top/bottom third for height.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResizeEdges {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

impl ResizeEdges {
    pub fn horizontal(self) -> bool {
        self.left || self.right
    }

    pub fn is_empty(self) -> bool {
        !self.horizontal() && !self.top && !self.bottom
    }

    pub fn cursor_icon(self) -> CursorIcon {
        match self {
            Self {
                left: true,
                top: true,
                ..
            } => CursorIcon::NwResize,
            Self {
                right: true,
                top: true,
                ..
            } => CursorIcon::NeResize,
            Self {
                left: true,
                bottom: true,
                ..
            } => CursorIcon::SwResize,
            Self {
                right: true,
                bottom: true,
                ..
            } => CursorIcon::SeResize,
            Self { left: true, .. } => CursorIcon::WResize,
            Self { right: true, .. } => CursorIcon::EResize,
            Self { top: true, .. } => CursorIcon::NResize,
            Self { bottom: true, .. } => CursorIcon::SResize,
            _ => CursorIcon::Default,
        }
    }
}

/// Width after an interactive resize. Dragging the left edge left grows the
/// column, so its horizontal delta is inverted (niri does the same).
pub fn resized_width(initial: f64, pointer_dx: f64, left_edge: bool, min: f64, max: f64) -> f64 {
    let delta = if left_edge { -pointer_dx } else { pointer_dx };
    (initial + delta).clamp(min, max)
}

pub fn resize_edges(local: Point<f64, Logical>, width: f64, height: f64) -> ResizeEdges {
    let mut edges = ResizeEdges::default();
    if width > 0.0 {
        if local.x < width / 3.0 {
            edges.left = true;
        } else if local.x > width * 2.0 / 3.0 {
            edges.right = true;
        }
    }
    if height > 0.0 {
        if local.y < height / 3.0 {
            edges.top = true;
        } else if local.y > height * 2.0 / 3.0 {
            edges.bottom = true;
        }
    }
    edges
}

#[derive(Debug)]
pub struct Layout {
    pub columns: Vec<Column>,
    pub active: usize,
    pub camera_x: f64,
    /// Full output size, ignoring layer-shell exclusive zones. Fullscreen
    /// windows use this rather than `working_area`.
    pub output_size: Size<f64, Logical>,
    pub working_area: Rectangle<f64, Logical>,
    pub gap: f64,
    pub default_proportion: f64,
    pub inner_border: BorderConfig,
    pub outer_border: BorderConfig,
}

impl Layout {
    pub fn new(
        output_size: Size<i32, Logical>,
        gap: f64,
        default_proportion: f64,
        inner_border: BorderConfig,
        outer_border: BorderConfig,
    ) -> Self {
        Self {
            columns: Vec::new(),
            active: 0,
            camera_x: 0.0,
            output_size: output_size.to_f64(),
            working_area: Rectangle::new(Point::from((0.0, 0.0)), output_size.to_f64()),
            gap,
            default_proportion,
            inner_border,
            outer_border,
        }
    }

    /// Total space reserved for the inner and outer border rings on one side.
    pub fn frame_inset(&self) -> f64 {
        (self.inner_border.width + self.outer_border.width).max(0.0)
    }

    /// Current visual tile rectangle, including borders and the client's
    /// committed height. `None` if the window is not in this layout.
    pub fn tile_rect(&self, window: &Window) -> Option<Rectangle<f64, Logical>> {
        let (column_index, tile_index) = self.tile_index(window)?;
        let column = self.columns.get(column_index)?;
        let tile = column.tiles.get(tile_index)?;
        Some(Rectangle::new(
            Point::from((column.x, tile.y)),
            Size::from((column.width, tile.rendered_height)),
        ))
    }

    /// Target visual rectangle from the last layout pass, including the
    /// border inset. Unlike `tile_rect`, this uses the target tile height
    /// rather than the client's current committed height.
    pub fn tile_target_rect(&self, window: &Window) -> Option<Rectangle<f64, Logical>> {
        let (column_index, tile_index) = self.tile_index(window)?;
        let column = self.columns.get(column_index)?;
        let tile = column.tiles.get(tile_index)?;
        Some(Rectangle::new(
            Point::from((column.x, tile.target_y)),
            Size::from((column.width, tile.height)),
        ))
    }

    /// Target content rectangle, excluding the border rings.
    pub fn content_target_rect(&self, window: &Window) -> Option<Rectangle<f64, Logical>> {
        let rect = self.tile_target_rect(window)?;
        let inset = self.frame_inset();
        Some(Rectangle::new(
            rect.loc + Point::from((inset, inset)),
            Size::from((
                (rect.size.w - 2.0 * inset).max(1.0),
                (rect.size.h - 2.0 * inset).max(1.0),
            )),
        ))
    }

    /// Content size that a brand-new single-window column will receive on this
    /// layout, matching `apply_geometry` so the initial xdg configure matches
    /// the first mapped frame.
    pub fn initial_content_size(
        &self,
        min_size: Size<i32, Logical>,
        max_size: Size<i32, Logical>,
    ) -> Size<i32, Logical> {
        let inset = self.frame_inset();
        let max_width = (self.working_area.size.w - 2.0 * self.gap).max(1.0);
        let mut column_width =
            resolve_width(self.working_area.size.w, self.gap, self.default_proportion);
        if min_size.w > 0 && min_size.w == max_size.w {
            column_width = (f64::from(max_size.w) + 2.0 * inset).clamp(1.0, max_width);
        }
        let min_tile_width = (f64::from(min_size.w) + 2.0 * inset).max(1.0);
        let upper = if max_size.w > 0 {
            (f64::from(max_size.w) + 2.0 * inset)
                .max(min_tile_width)
                .min(max_width.max(min_tile_width))
        } else {
            max_width.max(min_tile_width)
        };
        column_width = column_width.max(min_tile_width).min(upper);

        let full_height = self.full_height();
        let height = if min_size.h > 0 && min_size.h == max_size.h {
            (f64::from(max_size.h) + 2.0 * inset).clamp(1.0, full_height)
        } else {
            full_height
        };
        let size = Size::from((
            (column_width - 2.0 * inset).round().max(1.0) as i32,
            (height - 2.0 * inset).round().max(1.0) as i32,
        ));
        constraints::clamp_client_size(size, min_size, max_size, false)
    }

    /// Append a new window as its own default-width column.
    pub fn add_window(&mut self, window: Window) {
        let width = resolve_width(self.working_area.size.w, self.gap, self.default_proportion);
        self.columns.push(Column::single(
            Tile {
                window,
                fixed_height: None,
                height: 0.0,
                rendered_height: 0.0,
                y: 0.0,
                target_y: 0.0,
                min_size: Size::from((0, 0)),
                max_size: Size::from((0, 0)),
                configured_size: None,
                fullscreen_configured: false,
            },
            width,
            Some(self.default_proportion),
        ));
        self.active = self.columns.len() - 1;
    }

    /// Update a tile's cached client constraints. Returns whether they changed.
    pub fn update_window_constraints(
        &mut self,
        window: &Window,
        min_size: Size<i32, Logical>,
        max_size: Size<i32, Logical>,
    ) -> bool {
        let Some((column, tile)) = self.tile_index(window) else {
            return false;
        };
        let tile = &mut self.columns[column].tiles[tile];
        if tile.min_size == min_size && tile.max_size == max_size {
            return false;
        }
        tile.min_size = min_size;
        tile.max_size = max_size;
        true
    }

    pub fn window_is_fixed_size(&self, window: &Window) -> bool {
        self.tile_index(window).is_some_and(|(column, tile)| {
            let tile = &self.columns[column].tiles[tile];
            tile.min_size.w > 0 && tile.min_size.h > 0 && tile.min_size == tile.max_size
        })
    }

    pub fn column_index(&self, window: &Window) -> Option<usize> {
        self.tile_index(window).map(|(column, _)| column)
    }

    /// Find a window's column and tile index.
    pub fn tile_index(&self, window: &Window) -> Option<(usize, usize)> {
        self.columns
            .iter()
            .enumerate()
            .find_map(|(column_index, column)| {
                column
                    .tiles
                    .iter()
                    .position(|tile| &tile.window == window)
                    .map(|tile_index| (column_index, tile_index))
            })
    }

    /// Window focused in the active column.
    pub fn focused_window(&self) -> Option<&Window> {
        self.columns
            .get(self.active)
            .and_then(Column::active_window)
    }

    /// Remove one window from the layout.
    pub fn remove_window(&mut self, window: &Window) {
        let Some((column_index, tile_index)) = self.tile_index(window) else {
            return;
        };
        let column = &mut self.columns[column_index];
        column.tiles.remove(tile_index);
        if column.tiles.is_empty() {
            self.columns.remove(column_index);
            if column_index < self.active {
                self.active = self.active.saturating_sub(1);
            }
        } else {
            if tile_index < column.active {
                column.active = column.active.saturating_sub(1);
            }
            column.active = column.active.min(column.tiles.len().saturating_sub(1));
        }
        self.active = self.active.min(self.columns.len().saturating_sub(1));
    }

    /// Window whose tile rectangle contains `point` (including its border).
    /// Used as a fallback for clicks on the border, where no client surface is
    /// under the pointer.
    pub fn window_at(&self, point: Point<f64, Logical>) -> Option<Window> {
        self.columns.iter().rev().find_map(|column| {
            column.tiles.iter().rev().find_map(|tile| {
                let rect = Rectangle::new(
                    Point::from((column.x, tile.y)),
                    Size::from((column.width, tile.rendered_height)),
                );
                rect.contains(point).then(|| tile.window.clone())
            })
        })
    }

    pub fn set_active_tile(&mut self, column_index: usize, tile_index: usize) -> bool {
        let Some(column) = self.columns.get_mut(column_index) else {
            return false;
        };
        if tile_index >= column.tiles.len() {
            return false;
        }
        let column_changed = column_index != self.active;
        let tile_changed = column.active != tile_index;
        column.active = tile_index;
        self.active = column_index;
        column_changed || tile_changed
    }

    pub fn focus_left(&mut self) {
        if self.active > 0 {
            self.active -= 1;
            self.fit_active();
        }
    }

    pub fn focus_right(&mut self) {
        if self.active + 1 < self.columns.len() {
            self.active += 1;
            self.fit_active();
        }
    }

    pub fn focus_up(&mut self) {
        if let Some(column) = self.columns.get_mut(self.active) {
            column.active = column.active.saturating_sub(1);
        }
    }

    pub fn focus_down(&mut self) {
        if let Some(column) = self.columns.get_mut(self.active) {
            column.active = (column.active + 1).min(column.tiles.len().saturating_sub(1));
        }
    }

    pub fn focus_first(&mut self) {
        if !self.columns.is_empty() {
            self.active = 0;
            self.fit_active();
        }
    }

    pub fn focus_last(&mut self) {
        if !self.columns.is_empty() {
            self.active = self.columns.len() - 1;
            self.fit_active();
        }
    }

    /// Recompute geometry, configure sizes, and set the target camera.
    pub fn apply(&mut self, space: &mut Space<Window>, fullscreen: Option<&Window>) {
        self.apply_geometry(space, fullscreen);
        self.fit_active();
    }

    /// Recompute geometry and send configures without touching the camera.
    pub fn apply_windows(&mut self, space: &mut Space<Window>, fullscreen: Option<&Window>) {
        self.apply_geometry(space, fullscreen);
    }

    /// Apply live config changes without touching window identity or focus.
    pub fn set_options(
        &mut self,
        gap: f64,
        default_proportion: f64,
        inner_border: BorderConfig,
        outer_border: BorderConfig,
    ) {
        let old_default = self.default_proportion;
        for column in &mut self.columns {
            if column.proportion == Some(old_default) {
                column.proportion = Some(default_proportion);
            }
        }
        self.gap = gap;
        self.default_proportion = default_proportion;
        self.inner_border = inner_border;
        self.outer_border = outer_border;
    }

    #[cfg(any(feature = "nested", test))]
    pub fn set_output_size(&mut self, size: Size<i32, Logical>) {
        self.output_size = size.to_f64();
        self.working_area = Rectangle::from_size(size.to_f64());
    }

    pub fn set_working_area(&mut self, area: Rectangle<i32, Logical>) {
        self.working_area = Rectangle::new(area.loc.to_f64(), area.size.to_f64());
    }

    /// Tile height that fills the working area between the outer gaps.
    pub fn full_height(&self) -> f64 {
        (self.working_area.size.h - 2.0 * self.gap).max(1.0)
    }

    pub fn windows(&self) -> impl Iterator<Item = &Window> {
        self.columns
            .iter()
            .flat_map(|column| column.tiles.iter().map(|tile| &tile.window))
    }

    /// Forget which client size we last configured. The next apply sends a
    /// configure even when the layout size is unchanged, which is required
    /// after canvas changed the client size outside this layout.
    pub fn invalidate_configured_sizes(&mut self) {
        for column in &mut self.columns {
            for tile in &mut column.tiles {
                tile.configured_size = None;
            }
        }
    }

    /// Forget the scroll configure for one window after canvas configured it
    /// to a different size. Without this, a copied/moved scroll tile can still
    /// claim it already configured the scroll size and skip the configure that
    /// would resize the client back from its canvas size.
    pub fn forget_configured_size(&mut self, window: &Window) {
        if let Some((column, tile)) = self.tile_index(window) {
            let tile = &mut self.columns[column].tiles[tile];
            tile.configured_size = None;
            tile.fullscreen_configured = false;
        }
    }

    /// Configure the fullscreen overlay to the full output size and set the
    /// xdg FULLSCREEN state. The layout itself stays unchanged.
    pub fn configure_fullscreen_window(&mut self, window: &Window, size: Size<i32, Logical>) {
        if let Some((column, tile)) = self.tile_index(window) {
            configure_tile(&mut self.columns[column].tiles[tile], size, true);
        }
    }

    /// Configure one window to a content size using this layout's constraints.
    /// Used when switching to canvas, where the canvas rectangle is
    /// authoritative for the client size.
    pub fn configure_window_size(&mut self, window: &Window, size: Size<i32, Logical>) {
        if let Some((column, tile)) = self.tile_index(window) {
            configure_tile(&mut self.columns[column].tiles[tile], size, false);
        }
    }

    /// Re-fit the camera after geometry changed without re-mapping windows.
    pub(crate) fn refit(&mut self) {
        self.fit_active();
    }
}
