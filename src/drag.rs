//! Interactive drag state: column moves and mouse resizes.

use smithay::{
    desktop::Window,
    input::pointer::CursorIcon,
    utils::{Logical, Point, SERIAL_COUNTER, Size},
};

use crate::{
    layout::{MIN_COLUMN_WIDTH, ResizeEdges, resized_width},
    state::EvaWM,
    workspace::canvas_pixel_size,
};

pub enum Drag {
    /// Drag a single window.
    Move {
        window: Window,
        button: u32,
        /// Pointer position minus the window content origin at grab time, in
        /// content-local coordinates. Output independent.
        offset: Point<f64, Logical>,
        source_output: usize,
        source_workspace: usize,
        source_mode: crate::workspace::Mode,
        /// Output whose `Space` currently maps the dragged window.
        visual_output: usize,
        /// Whether the preview is currently mapped in `visual_output`.
        preview_mapped: bool,
        /// Source canvas rectangle, for restoring on cancel.
        original_rect: Option<smithay::utils::Rectangle<f64, Logical>>,
    },
    ColumnResize {
        window: Window,
        source_output: usize,
        source_workspace: usize,
        index: usize,
        tile: usize,
        button: u32,
        /// Screen-space pointer position at grab time. Screen deltas keep the
        /// grabbed edge under the cursor when the camera moves.
        start: Point<f64, Logical>,
        initial_width: f64,
        initial_height: f64,
        /// Height of the tile above when dragging a top edge.
        initial_previous_height: Option<f64>,
        edges: ResizeEdges,
    },
    CanvasResize {
        window: Window,
        source_output: usize,
        source_workspace: usize,
        button: u32,
        /// World-space pointer position at grab time.
        start: Point<f64, Logical>,
        initial: smithay::utils::Rectangle<f64, Logical>,
        edges: ResizeEdges,
    },
    Pan {
        button: u32,
        /// Screen-space pointer position at grab time.
        start: Point<f64, Logical>,
        start_view: Point<f64, Logical>,
    },
}

impl Drag {
    pub fn button(&self) -> u32 {
        match self {
            Drag::Move { button, .. }
            | Drag::ColumnResize { button, .. }
            | Drag::CanvasResize { button, .. }
            | Drag::Pan { button, .. } => *button,
        }
    }
}

/// Panning translates every element, so it keeps a full redraw. Moves and
/// resizes rely on the output damage tracker's old/new geometry damage.
fn drag_uses_full_redraw(drag: Option<&Drag>) -> bool {
    drag.is_some_and(|drag| matches!(drag, Drag::Pan { .. }))
}

impl EvaWM {
    /// Start dragging `window` as a single tile, not its whole column.
    pub fn begin_window_move(&mut self, window: &Window, button: u32) {
        if self.active_fullscreen().is_some() {
            return;
        }
        let source_output = self.active_output;
        let source_workspace = self.active_workspace();
        let source_mode = self.workspaces()[source_workspace].mode;
        if !self.workspaces()[source_workspace].contains(window) {
            return;
        }
        let Some(origin) = self.space().element_location(window) else {
            return;
        };

        // A quick second drag can begin while an open/resize/layout transition
        // is still running. Settle it first so the preview does not briefly use
        // the transition's old rect or scale.
        self.window_states.settle_transitions(window);

        let world = self
            .output_state()
            .overview_screen_to_world(self.pointer_pos);
        let original_rect = (source_mode == crate::workspace::Mode::Canvas)
            .then(|| self.workspaces()[source_workspace].canvas.rect(window))
            .flatten();
        self.space_mut().raise_element(window, false);
        self.set_drag_cursor(CursorIcon::Grabbing);
        self.drag = Some(Drag::Move {
            window: window.clone(),
            button,
            offset: world - origin.to_f64(),
            source_output,
            source_workspace,
            source_mode,
            visual_output: source_output,
            preview_mapped: true,
            original_rect,
        });
        self.clear_pointer_focus(self.event_time_msec());
    }

    fn place_drag_preview(
        &mut self,
        target_output: usize,
        window: &Window,
        location: Point<f64, Logical>,
        mapped: &mut bool,
    ) {
        let space = &mut self.outputs[target_output].space;
        if *mapped {
            space.relocate_element(window, location.to_i32_round());
        } else {
            space.map_element(window.clone(), location.to_i32_round(), false);
            // A newly mapped preview must stay above the target output's
            // existing windows until the drop.
            space.raise_element(window, false);
            *mapped = true;
        }
    }

    /// Restore a dragged window to its source workspace without dropping it.
    pub fn cancel_drag(&mut self) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        match drag {
            Drag::Move {
                window,
                source_output,
                source_workspace,
                source_mode,
                visual_output,
                original_rect,
                ..
            } => {
                if visual_output != source_output {
                    self.outputs[visual_output].space.unmap_elem(&window);
                }
                if source_mode == crate::workspace::Mode::Canvas
                    && let Some(rect) = original_rect
                {
                    self.outputs[source_output].workspaces[source_workspace]
                        .canvas
                        .set_rect(&window, rect);
                }
                {
                    let output = &mut self.outputs[source_output];
                    let workspace = &mut output.workspaces[source_workspace];
                    workspace.apply(&mut output.space);
                    output.map_output();
                }
                self.active_output = source_output;
                let serial = SERIAL_COUNTER.next_serial();
                self.focus_window(Some(&window), serial);
                self.mark_full_redraw_output(source_output);
                if visual_output != source_output {
                    self.mark_full_redraw_output(visual_output);
                }
            }
            Drag::CanvasResize { window, .. } => {
                self.window_states
                    .end_mouse_resize(&window, std::time::Instant::now());
                self.finish_canvas_resize(&window);
            }
            Drag::ColumnResize {
                window,
                source_output,
                ..
            } => {
                self.window_states
                    .end_mouse_resize(&window, std::time::Instant::now());
                // The column keeps its last dragged width; just repaint the
                // source output if the layout was changed before cancellation.
                self.request_redraw_output(source_output);
            }
            Drag::Pan { .. } => {}
        }
        if let Some(status) = self.drag_cursor.take() {
            self.set_cursor_image(status);
        }
        self.sync_pointer_focus(self.event_time_msec());
        self.request_redraw();
    }

    /// Drop drag state for a window that is being destroyed.
    ///
    /// Unlike [`Self::cancel_drag`], this never reinserts or focuses the
    /// window. A client can disappear at any point during a drag; keeping the
    /// dead `Window` in the drag state would let later pointer motion remap it
    /// into a `Space` and retain its client buffers.
    pub fn forget_dragged_window(&mut self, window: &Window) -> bool {
        let relevant = match &self.drag {
            Some(Drag::Move {
                window: dragged, ..
            })
            | Some(Drag::CanvasResize {
                window: dragged, ..
            })
            | Some(Drag::ColumnResize {
                window: dragged, ..
            }) => dragged == window,
            _ => false,
        };
        if !relevant {
            return false;
        }

        let Some(drag) = self.drag.take() else {
            return false;
        };
        if let Drag::Move {
            source_output,
            visual_output,
            ..
        } = drag
        {
            if let Some(output) = self.outputs.get_mut(visual_output) {
                output.space.unmap_elem(window);
            }
            if visual_output != source_output
                && let Some(output) = self.outputs.get_mut(source_output)
            {
                output.space.unmap_elem(window);
            }
            // The preview was mapped in one or both outputs; force those areas
            // to repaint without the dead element.
            self.mark_full_redraw_output(source_output);
            if visual_output != source_output {
                self.mark_full_redraw_output(visual_output);
            }
        } else {
            self.request_redraw();
        }

        if let Some(status) = self.drag_cursor.take() {
            self.set_cursor_image(status);
        }
        self.sync_pointer_focus(self.event_time_msec());
        true
    }

    pub fn end_drag(&mut self) {
        if let Some(drag) = self.drag.take() {
            match drag {
                Drag::Move {
                    window,
                    offset,
                    source_output,
                    source_workspace,
                    source_mode,
                    ..
                } => {
                    let target_output = self.pointer_output;
                    if target_output != source_output {
                        let output = &self.outputs[target_output];
                        let location = output.overview_screen_to_world(self.pointer_pos) - offset;
                        self.drop_window_from_drag(
                            window,
                            source_output,
                            source_workspace,
                            source_mode,
                            target_output,
                            location,
                        );
                    } else {
                        let world = self
                            .output_state()
                            .overview_screen_to_world(self.pointer_pos);
                        let index = self.active_workspace();
                        if self.workspaces()[index].mode == crate::workspace::Mode::Canvas {
                            let serial = SERIAL_COUNTER.next_serial();
                            self.focus_window(Some(&window), serial);
                        } else {
                            let inset = self.workspaces()[index].frame_inset();
                            let x = world.x - offset.x - inset;
                            let now = self.animation_now();
                            let before = self.capture_content_world_rects(now);
                            self.output_state_mut().workspaces[index]
                                .scroll
                                .move_window_to_position(&window, x);
                            self.apply_layout();
                            // Animate the dropped window from where it was released
                            // into its settled tile, and shift the windows that made
                            // room for it. Without this the drop snaps in one frame.
                            self.start_pending_layout_transitions(&before, now, true);
                            let serial = SERIAL_COUNTER.next_serial();
                            self.focus_window(Some(&window), serial);
                        }
                    }
                }
                Drag::ColumnResize {
                    window,
                    source_output,
                    ..
                } => {
                    // Niri animates the resized column into view on release.
                    let now = self.animation_now();
                    self.window_states
                        .end_mouse_resize(&window, std::time::Instant::now());
                    let config = self.config.animations.view_movement;
                    let animations_off = self.config.animations.off;
                    if let Some(output) = self.outputs.get_mut(source_output) {
                        output.apply_layout(now, config, animations_off);
                    }
                }
                Drag::CanvasResize {
                    window,
                    source_output,
                    source_workspace,
                    ..
                } => {
                    self.window_states
                        .end_mouse_resize(&window, std::time::Instant::now());
                    // Re-send the final size after the last motion event so a
                    // configure that was coalesced or ignored cannot leave the
                    // client smaller than its canvas frame. Keep the xdg
                    // Resizing state until the final commit (or a short
                    // timeout), matching Niri's final-configure wait.
                    if let Some(rect) = self.outputs[source_output].workspaces[source_workspace]
                        .canvas
                        .rect(&window)
                    {
                        self.configure_canvas_window(&window, canvas_pixel_size(rect));
                    }
                    self.window_states.begin_canvas_finish(&window);
                    self.schedule_canvas_resize_finish(window.clone());
                    let serial = SERIAL_COUNTER.next_serial();
                    self.focus_window(Some(&window), serial);
                }
                Drag::Pan { .. } => {}
            }
            if let Some(status) = self.drag_cursor.take() {
                self.set_cursor_image(status);
            }
            self.sync_pointer_focus(self.event_time_msec());
        }
        self.request_redraw();
    }

    pub fn advance_drag(&mut self, pos: Point<f64, Logical>) {
        let Some(mut drag) = self.drag.take() else {
            return;
        };

        match &mut drag {
            Drag::Move {
                window,
                offset,
                source_output,
                source_workspace,
                source_mode,
                visual_output,
                preview_mapped,
                ..
            } => {
                let target_output = self.pointer_output;
                let target = &self.outputs[target_output];
                let location = target.overview_screen_to_world(self.pointer_pos) - *offset;

                if target_output != *visual_output {
                    self.outputs[*visual_output].space.unmap_elem(window);
                    *visual_output = target_output;
                    *preview_mapped = false;
                }

                if target_output == *source_output && *source_mode == crate::workspace::Mode::Canvas
                {
                    let Some(mut rect) = self.outputs[*source_output].workspaces[*source_workspace]
                        .canvas
                        .rect(window)
                    else {
                        self.drag = Some(drag);
                        return;
                    };
                    rect.loc = location;
                    let workspace = &mut self.outputs[*source_output].workspaces[*source_workspace];
                    if workspace
                        .canvas
                        .maximized
                        .as_ref()
                        .is_some_and(|(maximized, _)| maximized == window)
                    {
                        workspace.canvas.maximized = None;
                    }
                    workspace.canvas.last_restore = Some((window.clone(), rect));
                    workspace.canvas.set_rect(window, rect);
                    // The window is normally already mapped while dragging.
                    // Relocate it without re-inserting or changing activation;
                    // mapping with `activate = true` on every pointer event
                    // activates/deactivates the whole stack and causes visible
                    // stutter from client reconfigures.
                    self.place_drag_preview(target_output, window, location, preview_mapped);
                } else {
                    self.place_drag_preview(target_output, window, location, preview_mapped);
                }
            }
            Drag::ColumnResize {
                source_output,
                source_workspace,
                index,
                tile,
                start,
                initial_width,
                initial_height,
                initial_previous_height,
                edges,
                ..
            } => {
                let zoom = self.outputs[*source_output].overview_zoom.value.max(0.05);
                let dx = (self.pointer_pos.x - start.x) / zoom;
                let dy = (self.pointer_pos.y - start.y) / zoom;

                let working_index = *source_workspace;
                let output = &mut self.outputs[*source_output];
                let gap = output.workspaces[working_index].gap();
                let max_width = (output.workspaces[working_index].working_area().size.w
                    - 2.0 * gap)
                    .max(MIN_COLUMN_WIDTH);

                let mut camera_delta = 0.0;
                if let Some(column) = output.workspaces[working_index]
                    .scroll
                    .columns
                    .get_mut(*index)
                    && edges.horizontal()
                {
                    let old_width = column.width;
                    let new_width =
                        resized_width(*initial_width, dx, edges.left, MIN_COLUMN_WIDTH, max_width);
                    column.set_fixed_width(new_width);
                    if edges.left {
                        // Keep the right edge anchored on screen while the
                        // left edge moves.
                        camera_delta = new_width - old_width;
                    }
                }

                if edges.top && *tile > 0 {
                    // Move the boundary between the previous tile and this one
                    // instead of growing this tile downward from the column top.
                    let previous = initial_previous_height.unwrap_or(0.0) + dy;
                    let current = *initial_height - dy;
                    output.workspaces[working_index]
                        .scroll
                        .set_tile_fixed_height(*index, *tile - 1, previous);
                    output.workspaces[working_index]
                        .scroll
                        .set_tile_fixed_height(*index, *tile, current);
                } else if edges.bottom {
                    output.workspaces[working_index]
                        .scroll
                        .set_tile_fixed_height(*index, *tile, *initial_height + dy);
                }

                if camera_delta != 0.0 {
                    let camera = output.workspaces[working_index]
                        .scroll
                        .clamp_camera_x(output.view_x.value + camera_delta);
                    output.view_x.snap(camera);
                    output.workspaces[working_index].scroll.camera_x = camera;
                }
                output.workspaces[working_index].apply_windows(&mut output.space);
                output.map_output();
            }
            Drag::CanvasResize {
                window,
                source_output,
                source_workspace,
                start,
                initial,
                edges,
                ..
            } => {
                let dx = pos.x - start.x;
                let dy = pos.y - start.y;
                let (min, max) =
                    self.canvas_constraints_at(*source_output, *source_workspace, window);
                let mut left = initial.loc.x;
                let mut top = initial.loc.y;
                let mut right = initial.loc.x + initial.size.w;
                let mut bottom = initial.loc.y + initial.size.h;
                if edges.left {
                    left += dx;
                }
                if edges.right {
                    right += dx;
                }
                if edges.top {
                    top += dy;
                }
                if edges.bottom {
                    bottom += dy;
                }
                let mut width = (right - left).max(1.0);
                let mut height = (bottom - top).max(1.0);
                if min.w > 0 {
                    width = width.max(f64::from(min.w));
                }
                if min.h > 0 {
                    height = height.max(f64::from(min.h));
                }
                if max.w > 0 {
                    width = width.min(f64::from(max.w));
                }
                if max.h > 0 {
                    height = height.min(f64::from(max.h));
                }
                if edges.left {
                    left = right - width;
                }
                if edges.top {
                    top = bottom - height;
                }
                let rect = smithay::utils::Rectangle::new(
                    Point::from((left, top)),
                    Size::from((width, height)),
                );
                if let Some(toplevel) = window.toplevel() {
                    toplevel.with_pending_state(|state| {
                        state.states.set(
                            smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Resizing,
                        );
                    });
                }
                self.canvas_update_rect_at(*source_output, *source_workspace, window, rect);
                self.queue_canvas_configure(
                    window,
                    Size::from((width.round() as i32, height.round() as i32)),
                );
            }
            Drag::Pan {
                start, start_view, ..
            } => {
                let pointer_pos = self.pointer_pos;
                let output = self.output_state_mut();
                let zoom = output.overview_zoom.value.max(0.05);
                let dx = (pointer_pos.x - start.x) / zoom;
                let dy = (pointer_pos.y - start.y) / zoom;
                // Dragging moves the canvas with the pointer: the camera goes
                // opposite to the drag so content follows the hand.
                output.view_x.snap(start_view.x - dx);
                output.view_y.snap(start_view.y - dy);
                output.map_output();
            }
        }

        let mut dirty = [self.active_output; 2];
        let dirty_count = match &drag {
            Drag::Move {
                source_output,
                visual_output,
                ..
            } => {
                dirty[0] = *source_output;
                dirty[1] = *visual_output;
                usize::from(dirty[0] != dirty[1]) + 1
            }
            Drag::ColumnResize { source_output, .. } | Drag::CanvasResize { source_output, .. } => {
                dirty[0] = *source_output;
                1
            }
            _ => 1,
        };
        // Panning translates every element; keep the full redraw there.
        // Moves and resizes are tracked by normal damage, which damages both
        // the old and new element rectangles.
        let full_redraw = drag_uses_full_redraw(Some(&drag));
        self.drag = Some(drag);
        for &index in &dirty[..dirty_count] {
            if full_redraw {
                self.mark_full_redraw_output(index);
            } else {
                self.request_redraw_output(index);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Drag, drag_uses_full_redraw};
    use smithay::utils::Point;

    #[test]
    fn only_panning_forces_a_full_redraw() {
        assert!(!drag_uses_full_redraw(None));
        let pan = Drag::Pan {
            button: 0x110,
            start: Point::from((10.0, 20.0)),
            start_view: Point::from((0.0, 0.0)),
        };
        assert!(drag_uses_full_redraw(Some(&pan)));
    }
}
