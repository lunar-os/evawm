//! Close-animation cleanup.
//!
//! Closing is snapshot-based: the client is asked to close, and when its
//! surface is destroyed the compositor animates the last tile snapshot. There
//! is no compositor-owned layout removal or refusal timeout.

use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::state::EvaWM;

pub(crate) fn shrink_rect(rect: Rectangle<f64, Logical>, scale: f64) -> Rectangle<f64, Logical> {
    let scale = scale.clamp(0.0, 1.0);
    let size = Size::from((rect.size.w * scale, rect.size.h * scale));
    Rectangle::new(
        rect.loc + Point::from(((rect.size.w - size.w) / 2.0, (rect.size.h - size.h) / 2.0)),
        size,
    )
}

impl EvaWM {
    /// Remove windows whose snapshot close animation finished.
    pub(crate) fn finish_closed_windows(&mut self, current_output: usize) {
        let finished = self.window_states.take_finished_closes();
        if finished.is_empty() {
            return;
        }
        for (window, output) in finished {
            // A window that unmapped (starting a close snapshot) and then
            // remapped before the animation ended now lives in a workspace.
            if self.workspace_for_window(&window).is_some() {
                continue;
            }
            if let Some(output_state) = self.outputs.get_mut(output) {
                output_state.space.unmap_elem(&window);
            }
            self.window_states.remove(&window);
            if output != current_output {
                self.request_redraw_output(output);
            }
        }
    }
}
