//! Cached solid-color buffers for window borders.
//!
//! Each border ring (outer and inner) is drawn as four thin edge strips (top,
//! bottom, left, right) around an open interior so semi-transparent windows
//! show the wallpaper or underlying windows cleanly. Left and right strips sit
//! strictly between top and bottom so corners never double-blend at partial
//! alpha. The buffers are cached per window so their IDs and commit counters
//! stay stable for the damage tracker.

use smithay::{
    backend::renderer::{Color32F, element::solid::SolidColorBuffer},
    utils::{Logical, Point, Rectangle, Size},
};

/// Border geometry and colors captured for a window. The renderer turns this
/// into four-strip solid-color rings.
#[derive(Debug, Clone, Copy)]
pub struct BorderStyle {
    pub outer_width: f64,
    pub inner_width: f64,
    pub outer_color: Color32F,
    pub inner_color: Color32F,
}

#[derive(Default)]
pub struct BorderStrip {
    pub buffer: SolidColorBuffer,
    pub rect: Rectangle<i32, Logical>,
}

impl BorderStrip {
    fn update(&mut self, rect: Rectangle<i32, Logical>, color: Color32F) {
        self.rect = rect;
        if rect.size.w > 0 && rect.size.h > 0 {
            self.buffer.update(rect.size, color);
        }
    }

    pub fn is_visible(&self) -> bool {
        self.rect.size.w > 0 && self.rect.size.h > 0
    }
}

#[derive(Default)]
pub struct BorderRing {
    pub top: BorderStrip,
    pub bottom: BorderStrip,
    pub left: BorderStrip,
    pub right: BorderStrip,
}

impl BorderRing {
    pub fn strips(&self) -> [&BorderStrip; 4] {
        [&self.top, &self.bottom, &self.left, &self.right]
    }

    fn update(&mut self, inset: i32, size: Size<i32, Logical>, thickness: i32, color: Color32F) {
        let w = size.w.max(0);
        let h = size.h.max(0);
        let t = thickness.max(0);
        if w == 0 || h == 0 || t == 0 {
            let empty = Rectangle::from_size(Size::from((0, 0)));
            self.top.update(empty, color);
            self.bottom.update(empty, color);
            self.left.update(empty, color);
            self.right.update(empty, color);
            return;
        }

        let top_h = t.min(h);
        let bottom_h = t.min(h - top_h);
        let mid_h = h - top_h - bottom_h;
        let left_w = t.min(w);
        let right_w = t.min(w - left_w);

        self.top.update(
            Rectangle::new(Point::from((inset, inset)), Size::from((w, top_h))),
            color,
        );
        self.bottom.update(
            Rectangle::new(
                Point::from((inset, inset + h - bottom_h)),
                Size::from((w, bottom_h)),
            ),
            color,
        );
        self.left.update(
            Rectangle::new(
                Point::from((inset, inset + top_h)),
                Size::from((left_w, mid_h)),
            ),
            color,
        );
        self.right.update(
            Rectangle::new(
                Point::from((inset + w - right_w, inset + top_h)),
                Size::from((right_w, mid_h)),
            ),
            color,
        );
    }
}

#[derive(Default)]
pub struct BorderBuffers {
    pub outer: BorderRing,
    pub inner: BorderRing,
}

impl BorderBuffers {
    /// Update the outer and inner four-strip rings. Sizes and offsets are
    /// logical pixels relative to the tile origin; the renderer scales them to
    /// physical pixels.
    pub fn update(
        &mut self,
        tile: Size<f64, Logical>,
        outer_width: f64,
        inner_width: f64,
        outer_color: Color32F,
        inner_color: Color32F,
    ) {
        let outer_size = Size::from((
            tile.w.max(0.0).round() as i32,
            tile.h.max(0.0).round() as i32,
        ));
        let outer_px = outer_width.max(0.0).round() as i32;
        let inner_px = inner_width.max(0.0).round() as i32;
        let inner_size = Size::from((
            (outer_size.w - 2 * outer_px).max(0),
            (outer_size.h - 2 * outer_px).max(0),
        ));

        self.outer.update(0, outer_size, outer_px, outer_color);
        self.inner
            .update(outer_px, inner_size, inner_px, inner_color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn border_rings_form_non_overlapping_strips_around_open_interior() {
        let mut buffers = BorderBuffers::default();
        buffers.update(
            Size::from((100.0, 50.0)),
            3.0,
            2.0,
            Color32F::new(1.0, 0.0, 0.0, 1.0),
            Color32F::new(0.0, 1.0, 0.0, 1.0),
        );

        assert_eq!(
            buffers.outer.top.rect,
            Rectangle::new((0, 0).into(), (100, 3).into())
        );
        assert_eq!(
            buffers.outer.bottom.rect,
            Rectangle::new((0, 47).into(), (100, 3).into())
        );
        assert_eq!(
            buffers.outer.left.rect,
            Rectangle::new((0, 3).into(), (3, 44).into())
        );
        assert_eq!(
            buffers.outer.right.rect,
            Rectangle::new((97, 3).into(), (3, 44).into())
        );

        assert_eq!(
            buffers.inner.top.rect,
            Rectangle::new((3, 3).into(), (94, 2).into())
        );
        assert_eq!(
            buffers.inner.bottom.rect,
            Rectangle::new((3, 45).into(), (94, 2).into())
        );
        assert_eq!(
            buffers.inner.left.rect,
            Rectangle::new((3, 5).into(), (2, 40).into())
        );
        assert_eq!(
            buffers.inner.right.rect,
            Rectangle::new((95, 5).into(), (2, 40).into())
        );
    }

    #[test]
    fn disabled_outer_ring_hides_outer_strips_and_keeps_inner_at_origin() {
        let mut buffers = BorderBuffers::default();
        buffers.update(
            Size::from((100.0, 50.0)),
            0.0,
            1.0,
            Color32F::BLACK,
            Color32F::BLACK,
        );

        assert!(
            buffers
                .outer
                .strips()
                .iter()
                .all(|strip| !strip.is_visible())
        );
        assert_eq!(
            buffers.inner.top.rect,
            Rectangle::new((0, 0).into(), (100, 1).into())
        );
        assert_eq!(
            buffers.inner.bottom.rect,
            Rectangle::new((0, 49).into(), (100, 1).into())
        );
        assert_eq!(
            buffers.inner.left.rect,
            Rectangle::new((0, 1).into(), (1, 48).into())
        );
        assert_eq!(
            buffers.inner.right.rect,
            Rectangle::new((99, 1).into(), (1, 48).into())
        );
    }

    #[test]
    fn tiny_tile_clamps_without_overlap_or_negative_sizes() {
        let mut buffers = BorderBuffers::default();
        buffers.update(
            Size::from((4.0, 4.0)),
            3.0,
            2.0,
            Color32F::BLACK,
            Color32F::BLACK,
        );

        assert_eq!(
            buffers.outer.top.rect,
            Rectangle::new((0, 0).into(), (4, 3).into())
        );
        assert_eq!(
            buffers.outer.bottom.rect,
            Rectangle::new((0, 3).into(), (4, 1).into())
        );
        assert!(!buffers.outer.left.is_visible());
        assert!(!buffers.outer.right.is_visible());
        assert!(
            buffers
                .inner
                .strips()
                .iter()
                .all(|strip| !strip.is_visible())
        );
    }
}
