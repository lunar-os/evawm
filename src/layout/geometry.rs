use smithay::{
    desktop::{Space, Window},
    utils::{Point, Size},
};

use super::{
    Layout, MIN_COLUMN_WIDTH,
    constraints::{configure_tile, fixed_client_size},
};

impl Layout {
    #[allow(clippy::too_many_lines)]
    pub(super) fn apply_geometry(
        &mut self,
        space: &mut Space<Window>,
        fullscreen: Option<&Window>,
    ) {
        let g = self.gap;
        let area = self.working_area;
        let top = area.loc.y + g;
        let full_height = self.full_height();
        let inset = self.frame_inset();

        // First pass: widths and world X positions. Fixed-size single-window
        // columns use the client's constrained size rather than the tile size.
        let max_width = (area.size.w - 2.0 * g).max(1.0);
        let mut x = area.loc.x + g;
        for column in &mut self.columns {
            if let Some(proportion) = column.proportion {
                column.width = resolve_width(area.size.w, g, proportion);
            }
            if column.tiles.len() == 1 {
                let (fixed_width, fixed_height) = fixed_client_size(&column.tiles[0]);
                if let Some(width) = fixed_width {
                    column.width = (width + 2.0 * inset).clamp(1.0, max_width);
                    column.proportion = None;
                }
                if let Some(height) = fixed_height {
                    column.tiles[0].fixed_height =
                        Some((height + 2.0 * inset).clamp(1.0, full_height));
                }
            }

            // A stack must be at least as wide as every tile's client minimum,
            // and no wider than every tile's maximum. Keep a constrained client
            // aligned with its frame even when the column would otherwise be
            // resized past it.
            let mut min_tile_width = 1.0_f64;
            let mut max_tile_width = f64::INFINITY;
            for tile in &column.tiles {
                min_tile_width = min_tile_width.max(f64::from(tile.min_size.w) + 2.0 * inset);
                if tile.max_size.w > 0 {
                    max_tile_width = max_tile_width.min(f64::from(tile.max_size.w) + 2.0 * inset);
                }
            }
            let upper = if max_tile_width.is_finite() {
                max_tile_width
                    .max(min_tile_width)
                    .min(max_width.max(min_tile_width))
            } else {
                max_width.max(min_tile_width)
            };
            column.width = column.width.max(min_tile_width).min(upper);
            column.x = x;
            x += column.width + g;
        }

        // Second pass: distribute tile heights, place, and configure.
        let mut fixed_buf: Vec<Option<f64>> = Vec::new();
        let mut min_buf: Vec<f64> = Vec::new();
        let mut scratch: Vec<Option<f64>> = Vec::new();
        let mut heights_buf: Vec<f64> = Vec::new();

        for column in &mut self.columns {
            let single_height = if column.tiles.len() == 1 {
                let tile = &column.tiles[0];
                let content = full_height.max(1.0);
                let min_h = (f64::from(tile.min_size.h) + 2.0 * inset).max(1.0);
                let height = match tile.fixed_height {
                    Some(fixed) => fixed.min(content).max(min_h),
                    None => {
                        if min_h > content + 0.5 {
                            min_h
                        } else {
                            content
                        }
                    }
                };
                Some(height)
            } else {
                fixed_buf.clear();
                fixed_buf.extend(column.tiles.iter().map(|tile| tile.fixed_height));
                min_buf.clear();
                min_buf.extend(
                    column
                        .tiles
                        .iter()
                        .map(|tile| (f64::from(tile.min_size.h) + 2.0 * inset).max(1.0)),
                );
                distribute_heights_into(
                    full_height,
                    g,
                    &fixed_buf,
                    &min_buf,
                    &mut scratch,
                    &mut heights_buf,
                );
                None
            };

            let mut y = top;
            let mut target_y = top;
            for (index, tile) in column.tiles.iter_mut().enumerate() {
                let height = single_height.unwrap_or_else(|| heights_buf[index]);
                tile.height = height;
                tile.y = y;
                tile.target_y = target_y;
                let committed = tile.window.geometry().size;
                // A fullscreen overlay commits the full output height. Do not
                // feed that back into the scroll layout: the column and every
                // sibling keep their pre-fullscreen geometry.
                let visible_height = if fullscreen == Some(&tile.window) || committed.h <= 0 {
                    height
                } else {
                    f64::from(committed.h) + 2.0 * inset
                };
                tile.rendered_height = visible_height;
                let location = Point::from((
                    (column.x + inset).round() as i32,
                    (y + inset).round() as i32,
                ));
                space.map_element(tile.window.clone(), location, false);
                tracing::trace!(
                    ?location,
                    target_height = height,
                    visible_height,
                    "tile placed"
                );

                if fullscreen != Some(&tile.window) {
                    let size = Size::from((
                        (column.width - 2.0 * inset).round().max(1.0) as i32,
                        (height - 2.0 * inset).round().max(1.0) as i32,
                    ));
                    configure_tile(tile, size, false);
                }

                y += visible_height + g;
                target_y += height + g;
            }
        }

        tracing::debug!(
            active = self.active,
            active_tile = self.columns.get(self.active).map(|column| column.active),
            columns = self.columns.len(),
            tiles = self
                .columns
                .iter()
                .map(|column| column.tiles.len())
                .sum::<usize>(),
            camera = self.camera_x,
            working_x = self.working_area.loc.x,
            working_y = self.working_area.loc.y,
            working_w = self.working_area.size.w,
            working_h = self.working_area.size.h,
            "layout applied"
        );
    }
}

/// Distribute the column height between tiles.
///
/// `full_height` is the inner column height (working area minus the two outer
/// gaps). Fixed tiles keep their height; auto tiles split what remains. A tile
/// with a client minimum height takes that minimum first, so the remaining
/// auto tiles shrink instead of pushing the stack below the working area.
#[cfg(test)]
pub(super) fn distribute_heights(
    full_height: f64,
    gap: f64,
    fixed: &[Option<f64>],
    min: &[f64],
) -> Vec<f64> {
    let mut scratch = Vec::new();
    let mut out = Vec::new();
    distribute_heights_into(full_height, gap, fixed, min, &mut scratch, &mut out);
    out
}

fn distribute_heights_into(
    full_height: f64,
    gap: f64,
    fixed: &[Option<f64>],
    min: &[f64],
    heights: &mut Vec<Option<f64>>,
    out: &mut Vec<f64>,
) {
    out.clear();
    heights.clear();
    let count = fixed.len();
    if count == 0 {
        return;
    }

    let content = (full_height - gap * count.saturating_sub(1) as f64).max(count as f64);
    let minimum = |index: usize| min.get(index).copied().unwrap_or(1.0).max(1.0);
    heights.extend(
        fixed
            .iter()
            .enumerate()
            .map(|(index, height)| height.map(|height| height.max(minimum(index)))),
    );

    let auto_count = heights.iter().filter(|height| height.is_none()).count();
    // Fixed tiles can be larger than the available column after an output or
    // working-area shrink. Scale them down before distributing space to auto
    // tiles so auto tiles' minimum heights cannot push the stack off-screen.
    if auto_count > 0 {
        let fixed_sum: f64 = heights.iter().filter_map(|height| *height).sum();
        let auto_min_sum: f64 = heights
            .iter()
            .enumerate()
            .filter(|(_, height)| height.is_none())
            .map(|(index, _)| minimum(index))
            .sum();
        let available_for_fixed = (content - auto_min_sum).max(0.0);
        if fixed_sum > available_for_fixed && fixed_sum > 0.0 {
            let scale = available_for_fixed / fixed_sum;
            for (index, height) in heights.iter_mut().enumerate() {
                if let Some(value) = height {
                    *value = (*value * scale).max(minimum(index));
                }
            }
        }
    } else {
        let fixed_sum: f64 = heights.iter().filter_map(|height| *height).sum();
        if fixed_sum <= content || fixed_sum <= 0.0 {
            out.extend(heights.iter().map(|height| height.unwrap_or(1.0).max(1.0)));
            return;
        }
        let scale = content / fixed_sum;
        out.extend(
            heights
                .iter()
                .enumerate()
                .map(|(index, height)| (height.unwrap_or(1.0) * scale).max(minimum(index))),
        );
        return;
    }

    let mut remaining = content - heights.iter().filter_map(|height| *height).sum::<f64>();
    let mut remaining_auto = auto_count;

    // Allocate minimum heights to auto tiles that cannot fit in the current
    // equal share, then redistribute the remainder. This converges because
    // each iteration assigns at least one tile out of the auto set.
    while remaining_auto > 0 {
        let share = (remaining / remaining_auto as f64).max(1.0);
        let mut forced_any = false;
        for (index, height) in heights.iter_mut().enumerate() {
            if height.is_none() && minimum(index) > share + 0.5 {
                let value = minimum(index);
                *height = Some(value);
                remaining -= value;
                remaining_auto -= 1;
                forced_any = true;
            }
        }
        if !forced_any {
            for height in heights.iter_mut() {
                if height.is_none() {
                    *height = Some(share);
                }
            }
            break;
        }
    }

    out.extend(heights.iter().map(|height| height.unwrap_or(1.0).max(1.0)));
}

/// Niri's proportional column width:
/// `(working_width - gaps) * proportion - gaps`.
///
/// The extra `gaps` keeps two 50% columns plus both outer gaps and the gap
/// between them inside the working area, so switching between them does not
/// scroll the camera.
pub(super) fn resolve_width(working_width: f64, gap: f64, proportion: f64) -> f64 {
    ((working_width - gap) * proportion - gap).max(MIN_COLUMN_WIDTH)
}
