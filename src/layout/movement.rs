use smithay::desktop::Window;

use super::{Column, Layout, Tile};

impl Layout {
    fn min_tile_height(&self, tile: &Tile) -> f64 {
        (f64::from(tile.min_size.h) + 2.0 * self.frame_inset()).max(1.0)
    }

    /// Whether one more tile can join `column` without forcing the stack past
    /// the working-area height. Fixed/minimum clients must keep their size.
    fn can_stack_with(&self, column: usize, extra: &Tile) -> bool {
        let Some(column) = self.columns.get(column) else {
            return false;
        };
        let mut required = self.gap * column.tiles.len() as f64;
        for tile in &column.tiles {
            required += tile
                .fixed_height
                .unwrap_or(0.0)
                .max(self.min_tile_height(tile));
        }
        required += extra
            .fixed_height
            .unwrap_or(0.0)
            .max(self.min_tile_height(extra));
        required <= self.full_height() + 0.5
    }

    fn expel_active_tile(&mut self, right: bool) {
        let source = self.active;
        let column = &mut self.columns[source];
        let source_tile = column.active;
        if source_tile >= column.tiles.len() {
            return;
        }
        let mut tile = column.tiles.remove(source_tile);
        tile.fixed_height = None;
        column.active = source_tile.min(column.tiles.len().saturating_sub(1));
        let width = column.width;
        let target = if right { source + 1 } else { source };
        self.columns
            .insert(target, Column::single(tile, width, None));
        self.active = target;
    }

    fn drop_index_for_x(&self, x: f64) -> usize {
        let mut insert = 0;
        for (index, column) in self.columns.iter().enumerate() {
            if x > column.x + column.width / 2.0 {
                insert = index + 1;
            }
        }
        insert.min(self.columns.len())
    }

    /// Move the active window into a new column immediately to the left.
    pub fn move_window_left(&mut self) {
        let Some(column) = self.columns.get(self.active) else {
            return;
        };
        if column.tiles.len() <= 1 {
            self.move_left();
            return;
        }
        self.expel_active_tile(false);
    }

    /// Move the active window into a new column immediately to the right.
    pub fn move_window_right(&mut self) {
        let Some(column) = self.columns.get(self.active) else {
            return;
        };
        if column.tiles.len() <= 1 {
            self.move_right();
            return;
        }
        self.expel_active_tile(true);
    }

    pub fn move_left(&mut self) {
        if self.active > 0 {
            self.columns.swap(self.active, self.active - 1);
            self.active -= 1;
        }
    }

    pub fn move_right(&mut self) {
        if self.active + 1 < self.columns.len() {
            self.columns.swap(self.active, self.active + 1);
            self.active += 1;
        }
    }

    pub fn move_first(&mut self) {
        if self.active > 0 {
            let column = self.columns.remove(self.active);
            self.columns.insert(0, column);
            self.active = 0;
        }
    }

    pub fn move_last(&mut self) {
        if self.active + 1 < self.columns.len() {
            let column = self.columns.remove(self.active);
            self.columns.push(column);
            self.active = self.columns.len() - 1;
        }
    }

    pub fn move_up(&mut self) {
        let Some(column) = self.columns.get_mut(self.active) else {
            return;
        };
        if column.active > 0 {
            let index = column.active;
            column.tiles.swap(index, index - 1);
            column.active -= 1;
        }
    }

    pub fn move_down(&mut self) {
        let Some(column) = self.columns.get_mut(self.active) else {
            return;
        };
        if column.active + 1 < column.tiles.len() {
            let index = column.active;
            column.tiles.swap(index, index + 1);
            column.active += 1;
        }
    }

    /// Niri's `consume-or-expel-window-left`.
    pub fn consume_or_expel_left(&mut self) {
        let Some(source_column) = self.columns.get(self.active) else {
            return;
        };
        if source_column.active >= source_column.tiles.len() {
            return;
        }

        if source_column.tiles.len() == 1 {
            if self.active == 0 {
                return;
            }
            let target = self.active - 1;
            if !self.can_stack_with(target, &self.columns[self.active].tiles[0]) {
                return;
            }
            let column = self.columns.remove(self.active);
            let tile = column.tiles.into_iter().next().unwrap();
            let target_column = &mut self.columns[target];
            target_column.tiles.push(tile);
            target_column.active = target_column.tiles.len() - 1;
            self.active = target;
        } else {
            self.expel_active_tile(false);
        }
    }

    /// Niri's `consume-or-expel-window-right`.
    pub fn consume_or_expel_right(&mut self) {
        let Some(source_column) = self.columns.get(self.active) else {
            return;
        };
        if source_column.active >= source_column.tiles.len() {
            return;
        }

        if source_column.tiles.len() == 1 {
            if self.active + 1 >= self.columns.len() {
                return;
            }
            if !self.can_stack_with(self.active + 1, &self.columns[self.active].tiles[0]) {
                return;
            }
            let column = self.columns.remove(self.active);
            let tile = column.tiles.into_iter().next().unwrap();
            // After removal, the former right neighbour is at `self.active`.
            let target_column = &mut self.columns[self.active];
            target_column.tiles.push(tile);
            target_column.active = target_column.tiles.len() - 1;
        } else {
            self.expel_active_tile(true);
        }
    }

    /// Move a single dragged window to its own column at the drop `x`.
    ///
    /// Dragging never inserts into an existing stack; an accidental drop on
    /// another window still produces a separate column.
    pub fn move_window_to_position(&mut self, window: &Window, x: f64) {
        let Some((source_column, source_tile)) = self.tile_index(window) else {
            return;
        };

        let column = if self.columns[source_column].tiles.len() == 1 {
            let mut column = self.columns.remove(source_column);
            column.tiles[0].fixed_height = None;
            column.x = 0.0;
            column
        } else {
            let width = self.columns[source_column].width;
            let source = &mut self.columns[source_column];
            let mut tile = source.tiles.remove(source_tile);
            tile.fixed_height = None;
            if source_tile < source.active {
                source.active = source.active.saturating_sub(1);
            }
            source.active = source.active.min(source.tiles.len().saturating_sub(1));
            Column::single(tile, width, None)
        };

        let insert = self.drop_index_for_x(x);
        self.columns.insert(insert, column);
        self.active = insert;
    }

    /// Insert a single-tile column taken from another workspace or output at
    /// the drop `x`. The window is not yet part of this layout. Dragging never
    /// stacks into an existing column.
    pub fn insert_column_at_position(&mut self, mut column: Column, x: f64) -> bool {
        if column.tiles.len() != 1 {
            return false;
        }
        column.tiles[0].fixed_height = None;
        column.active = 0;
        column.x = 0.0;

        let insert = self.drop_index_for_x(x);
        self.columns.insert(insert, column);
        self.active = insert;
        true
    }

    /// Remove the active column for moving it to another workspace.
    pub fn take_active_column(&mut self) -> Option<Column> {
        if self.columns.is_empty() {
            return None;
        }
        let column = self.columns.remove(self.active);
        self.active = self.active.min(self.columns.len().saturating_sub(1));
        Some(column)
    }

    /// Insert a column after the active column (or at the start when empty).
    pub fn insert_column(&mut self, column: Column) {
        let index = if self.columns.is_empty() {
            0
        } else {
            (self.active + 1).min(self.columns.len())
        };
        self.columns.insert(index, column);
        self.active = index;
    }

    /// Remove a specific window for moving it to another workspace, returning
    /// a single-tile column. Works even when the window is not the scroll
    /// focus (canvas focus can differ from the active scroll tile).
    pub fn take_window_to_column(&mut self, window: &Window) -> Option<Column> {
        let (column_index, tile_index) = self.tile_index(window)?;
        if self.columns[column_index].tiles.len() <= 1 {
            let column = self.columns.remove(column_index);
            if column_index < self.active {
                self.active -= 1;
            } else {
                self.active = self.active.min(self.columns.len().saturating_sub(1));
            }
            return Some(column);
        }

        let width = self.columns[column_index].width;
        let column = &mut self.columns[column_index];
        let mut tile = column.tiles.remove(tile_index);
        tile.fixed_height = None;
        if tile_index < column.active {
            column.active -= 1;
        } else {
            column.active = column.active.min(column.tiles.len() - 1);
        }
        Some(Column::single(tile, width, None))
    }
}
