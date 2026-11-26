use smithay::utils::{Logical, Rectangle, Size};

use super::{Column, Layout, MIN_COLUMN_WIDTH, MIN_TILE_HEIGHT, Tile};
impl Layout {
    pub fn resize_width(&mut self, fraction: f64) {
        let Some(column) = self.columns.get_mut(self.active) else {
            return;
        };
        let max = (self.working_area.size.w - 2.0 * self.gap).max(MIN_COLUMN_WIDTH);
        column.set_fixed_width((column.width * (1.0 + fraction)).clamp(MIN_COLUMN_WIDTH, max));
    }

    /// `resize-height`: change the active tile's height by a fraction of
    /// its current height.
    pub fn resize_height(&mut self, fraction: f64) {
        let column_index = self.active;
        let Some(column) = self.columns.get(column_index) else {
            return;
        };
        let tile_index = column.active;
        let Some(tile) = column.tiles.get(tile_index) else {
            return;
        };
        let height = tile.height * (1.0 + fraction);
        self.set_tile_fixed_height(column_index, tile_index, height);
        if let Some(tile) = self
            .columns
            .get(column_index)
            .and_then(|column| column.tiles.get(tile_index))
        {
            tracing::debug!(
                column = column_index,
                tile = tile_index,
                height = tile.fixed_height.unwrap_or(height),
                fixed = ?tile.fixed_height,
                "keyboard tile height resize"
            );
        }
    }

    pub fn set_tile_fixed_height(&mut self, column_index: usize, tile_index: usize, height: f64) {
        let inset = self.frame_inset();
        let Some(column) = self.columns.get_mut(column_index) else {
            return;
        };
        let max = max_tile_height_for(&self.working_area, self.gap, inset, column, tile_index);
        let single_tile = column.tiles.len() == 1;
        if let Some(tile) = column.tiles.get_mut(tile_index) {
            let min = (f64::from(tile.min_size.h) + 2.0 * inset)
                .max(MIN_TILE_HEIGHT)
                .min(max);
            let height = height.max(min).min(max);
            // Only clear `fixed_height` when a single tile fills the column; in
            // a stack, `None` means equal auto-split rather than maximum height.
            let fixed = if single_tile && height >= max - 0.5 {
                None
            } else {
                Some(height)
            };
            tile.fixed_height = fixed;
            tracing::trace!(height, ?fixed, tile = tile_index, "tile height resized");
        }
    }

    pub fn maximize_column(&mut self) {
        let max_width = (self.working_area.size.w - 2.0 * self.gap).max(MIN_COLUMN_WIDTH);
        let Some(column) = self.columns.get_mut(self.active) else {
            return;
        };
        column.expanded = None;

        if let Some((width, proportion)) = column.saved.take() {
            column.width = width;
            column.proportion = proportion;
        } else {
            column.saved = Some((column.width, column.proportion));
            column.proportion = None;
            column.width = max_width;
        }
    }

    /// Niri's `expand-column-to-available-width`: fill the width left over
    /// after every fully visible column, keeping those neighbours on screen.
    /// Invoking it again restores the previous width.
    pub fn expand_column_to_available_width(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        if let Some((width, proportion)) = self.columns[self.active].expanded.take() {
            let column = &mut self.columns[self.active];
            column.width = width;
            column.proportion = proportion;
            column.saved = None;
            return;
        }

        let gap = self.gap;
        let area = self.working_area;
        let view_left = area.loc.x + self.camera_x;
        let view_right = view_left + area.size.w;
        let mut width_taken = 0.0;
        let mut leftmost_x = None;
        let mut active_x = None;
        let mut counted_neighbour = false;

        for (index, column) in self.columns.iter().enumerate() {
            if column.x + 0.5 < view_left + gap {
                continue;
            }
            leftmost_x.get_or_insert(column.x);
            if view_right + 0.5 < column.x + column.width + gap {
                break;
            }
            if index == self.active {
                active_x = Some(column.x);
            } else {
                counted_neighbour = true;
            }
            width_taken += column.width + gap;
        }

        if active_x.is_none() {
            return;
        }
        let available = area.size.w - gap - width_taken;
        if available <= 0.5 {
            return;
        }

        if !counted_neighbour {
            // Only the active column is fully visible. Maximize it, but keep
            // an expand marker so invoking the action again restores the
            // previous width instead of requiring a different action.
            let column = &self.columns[self.active];
            if column.saved.is_some() {
                return;
            }
            let old_width = column.width;
            let old_proportion = column.proportion;
            self.maximize_column();
            self.columns[self.active].expanded = Some((old_width, old_proportion));
            return;
        }

        let old_width = self.columns[self.active].width;
        let old_proportion = self.columns[self.active].proportion;
        let column = &mut self.columns[self.active];
        column.saved = None;
        column.set_fixed_width(old_width + available);
        column.expanded = Some((old_width, old_proportion));
        // Shift following columns before clamping so `max_camera_x()` reflects
        // the expanded total width.
        for following in &mut self.columns[self.active + 1..] {
            following.x += available;
        }

        if let Some(leftmost_x) = leftmost_x {
            self.camera_x = (leftmost_x - area.loc.x - gap).clamp(0.0, self.max_camera_x());
        }
    }

    /// Keep the active column expanded after it moved to another output.
    /// Stores a restore width if the source column was expanded without one
    /// (for example when a single window left a maximized multi-tile column)
    /// and fits the width to the target working area.
    pub(crate) fn ensure_active_column_maximized(&mut self) {
        let max_width = (self.working_area.size.w - 2.0 * self.gap).max(MIN_COLUMN_WIDTH);
        let Some(column) = self.columns.get_mut(self.active) else {
            return;
        };
        if column.saved.is_none() {
            column.saved = Some((column.width, column.proportion));
        }
        column.expanded = None;
        column.proportion = None;
        column.width = max_width;
    }
}

pub(crate) fn clamp_client_size(
    mut size: Size<i32, Logical>,
    min: Size<i32, Logical>,
    max: Size<i32, Logical>,
    fullscreen: bool,
) -> Size<i32, Logical> {
    if fullscreen {
        return size;
    }
    if max.w > 0 {
        size.w = size.w.min(max.w);
    }
    if max.h > 0 {
        size.h = size.h.min(max.h);
    }
    if min.w > 0 {
        size.w = size.w.max(min.w);
    }
    if min.h > 0 {
        size.h = size.h.max(min.h);
    }
    tracing::trace!(?min, ?max, ?size, "client size constraints");
    size
}

pub(super) fn fixed_client_size(tile: &Tile) -> (Option<f64>, Option<f64>) {
    let width = (tile.min_size.w > 0 && tile.min_size.w == tile.max_size.w)
        .then_some(f64::from(tile.max_size.w));
    let height = (tile.min_size.h > 0 && tile.min_size.h == tile.max_size.h)
        .then_some(f64::from(tile.max_size.h));
    (width, height)
}

pub(super) fn configure_tile(tile: &mut Tile, mut size: Size<i32, Logical>, fullscreen: bool) {
    let Some(toplevel) = tile.window.toplevel().cloned() else {
        return;
    };

    size = clamp_client_size(size, tile.min_size, tile.max_size, fullscreen);

    if tile.configured_size == Some(size) && tile.fullscreen_configured == fullscreen {
        return;
    }
    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
    toplevel.with_pending_state(|state| {
        state.size = Some(size);
        if fullscreen {
            state.states.set(State::Fullscreen);
            state.states.unset(State::TiledLeft);
            state.states.unset(State::TiledRight);
            state.states.unset(State::TiledTop);
            state.states.unset(State::TiledBottom);
        } else {
            state.states.unset(State::Fullscreen);
            state.states.set(State::TiledLeft);
            state.states.set(State::TiledRight);
            state.states.set(State::TiledTop);
            state.states.set(State::TiledBottom);
        }
    });
    toplevel.send_pending_configure();
    tile.configured_size = Some(size);
    tile.fullscreen_configured = fullscreen;
    tracing::trace!(?size, fullscreen, "sent size configure");
}

pub(super) fn max_tile_height_for(
    working_area: &Rectangle<f64, Logical>,
    gap: f64,
    inset: f64,
    column: &Column,
    tile_index: usize,
) -> f64 {
    let full = (working_area.size.h - 2.0 * gap).max(1.0);
    let gaps = gap * column.tiles.len().saturating_sub(1) as f64;
    let reserved: f64 = column
        .tiles
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != tile_index)
        .map(|(_, tile)| {
            let client_min = (f64::from(tile.min_size.h) + 2.0 * inset).max(MIN_TILE_HEIGHT);
            tile.fixed_height
                .map(|height| height.max(client_min))
                .unwrap_or(client_min)
        })
        .sum();
    (full - gaps - reserved).max(MIN_TILE_HEIGHT).min(full)
}
