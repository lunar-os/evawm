use super::Layout;

impl Layout {
    /// Center the active column explicitly (`Mod+C`).
    pub fn center(&mut self) {
        let Some(column) = self.columns.get(self.active) else {
            self.camera_x = 0.0;
            return;
        };
        let content_x = column.x - self.working_area.loc.x;
        let target = content_x + column.width / 2.0 - self.working_area.size.w / 2.0;
        // Explicit centering may intentionally expose empty space at the
        // first/last column. Keep it until another layout/focus action.
        self.camera_x = target;
    }

    /// Clamp a camera value for this workspace.
    pub fn clamp_camera_x(&self, value: f64) -> f64 {
        value.clamp(0.0, self.max_camera_x())
    }

    /// Largest camera offset that still leaves no empty space to the right of
    /// the last column.
    pub(super) fn max_camera_x(&self) -> f64 {
        let Some(last) = self.columns.last() else {
            return 0.0;
        };
        let area_right = self.working_area.loc.x + self.working_area.size.w;
        (last.x + last.width + self.gap - area_right).max(0.0)
    }

    /// Keep the active column fully visible, moving the camera only as much as
    /// necessary. This is niri's `center-focused-column "never"` behavior.
    pub(super) fn fit_active(&mut self) {
        let Some(column) = self.columns.get(self.active) else {
            self.camera_x = 0.0;
            return;
        };

        let target = fit_offset(
            self.camera_x,
            column.x - self.working_area.loc.x,
            column.width,
            self.working_area.size.w,
            self.gap,
        );

        self.camera_x = target.clamp(0.0, self.max_camera_x());
    }
}

/// Camera offset that keeps a column visible with niri's `never` policy.
///
/// `content_x` is the column's left edge in working-area coordinates. The
/// returned offset leaves `gap` padding around the column when possible,
/// leaves an already-visible column alone, and otherwise aligns to the side
/// requiring the smaller movement.
pub(super) fn fit_offset(camera: f64, content_x: f64, width: f64, view_w: f64, gap: f64) -> f64 {
    // A column at least as wide as the view is left-aligned.
    if width >= view_w {
        return content_x;
    }

    let padding = ((view_w - width) / 2.0).clamp(0.0, gap);
    let left = content_x - padding;
    let right = content_x + width + padding - view_w;

    // Visible iff right <= camera <= left.
    if right <= camera && camera <= left {
        camera
    } else if (camera - left).abs() <= (camera - right).abs() {
        left
    } else {
        right
    }
}
