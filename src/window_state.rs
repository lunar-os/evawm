//! Per-window state: animations and cached border buffers.
//!
//! Keeping both in one map means window creation, lookup, and destruction have
//! a single owner instead of parallel per-window maps.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use smithay::{
    desktop::Window,
    utils::{Logical, Point, Rectangle, Scale, Size},
};

use crate::{
    animation::{Animation, AnimationConfig, OpenAnimationConfig},
    borders::BorderBuffers,
};

struct SizeAnimation {
    from: Size<f64, Logical>,
    to: Size<f64, Logical>,
    animation: Animation,
}

struct RectTransition {
    from: Rectangle<f64, Logical>,
    to: Rectangle<f64, Logical>,
    border_from: f32,
    border_to: f32,
    /// Render above the layer-shell top layer (fullscreen) rather than in the
    /// normal tiled window stack.
    on_top: bool,
    /// Rectangles are in screen space (fullscreen) instead of world space.
    screen_space: bool,
    animation: Animation,
}

#[derive(Debug, Clone, Copy)]
pub struct TransitionStyle {
    pub rect: Rectangle<f64, Logical>,
    pub border_alpha: f32,
    pub on_top: bool,
    pub screen_space: bool,
}

struct StoredSnapshot {
    element: crate::snapshot::SnapshotRenderElement,
    world_rect: Rectangle<f64, Logical>,
}

struct ClosingSnapshot {
    element: crate::snapshot::SnapshotRenderElement,
    from: Rectangle<f64, Logical>,
    to: Rectangle<f64, Logical>,
    animation: Animation,
    output_index: usize,
    /// Stable workspace id the tile belonged to. A closing snapshot is only
    /// drawn while that workspace is active on its output; otherwise a window
    /// closed on a hidden workspace would flash over the visible workspace.
    workspace_id: u64,
    /// The visual animation reached its end.
    finished: bool,
}

/// Grace period after releasing an interactive mouse resize during which an
/// in-flight size-changing commit still suppresses the post-commit resize
/// animation.
const RESIZE_SUPPRESSION_GRACE: Duration = Duration::from_millis(300);

/// Wall-clock timeout before sending another interactive canvas configure even
/// if the client has not committed the previous size yet.
const CANVAS_CONFIGURE_TIMEOUT: Duration = Duration::from_millis(150);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ResizeSuppression {
    #[default]
    None,
    /// An interactive mouse resize is currently held.
    Dragging,
    /// The drag ended; keep suppressing until the final size-changing commit
    /// arrives or this wall-clock deadline passes.
    PendingUntil(Instant),
}

impl ResizeSuppression {
    fn begin_drag(&mut self) {
        *self = Self::Dragging;
    }

    fn end_drag(&mut self, now: Instant) {
        if matches!(self, Self::Dragging) {
            *self = Self::PendingUntil(now + RESIZE_SUPPRESSION_GRACE);
        }
    }

    fn clear(&mut self) {
        *self = Self::None;
    }

    /// Evaluate whether a commit at `now` should suppress the resize animation,
    /// and advance the suppression state accordingly.
    fn consume_commit(&mut self, now: Instant, drag_active: bool, size_changed: bool) -> bool {
        if drag_active {
            return !matches!(self, Self::None);
        }
        match *self {
            Self::None => false,
            Self::Dragging => {
                // Safety net if a drag ended without calling `end_mouse_resize`.
                *self = if size_changed {
                    Self::None
                } else {
                    Self::PendingUntil(now + RESIZE_SUPPRESSION_GRACE)
                };
                true
            }
            Self::PendingUntil(until) => {
                if now > until {
                    *self = Self::None;
                    false
                } else {
                    if size_changed {
                        *self = Self::None;
                    }
                    true
                }
            }
        }
    }
}

#[derive(Default)]
struct WindowAnimation {
    borders: BorderBuffers,
    open: Option<Animation>,
    open_snapshot: crate::snapshot::SnapshotBuffer,
    unmap_snapshot: Option<StoredSnapshot>,
    resize: Option<SizeAnimation>,
    transition: Option<RectTransition>,
    closing: Option<ClosingSnapshot>,
    started: bool,
    last_size: Option<Size<i32, Logical>>,
    /// Suppress the post-commit resize animation while a mouse resize is in
    /// progress or waiting for its final commit. Niri only animates manual
    /// (keyboard/preset) resizes, not interactive mouse resizes.
    resize_suppression: ResizeSuppression,
    /// Latest interactive canvas size waiting for the next frame. Coalescing
    /// pointer-rate updates avoids flooding slow clients without making the
    /// client wait for a commit.
    queued_canvas_size: Option<Size<i32, Logical>>,
    /// Last interactive canvas size configure sent to the client. The next
    /// request waits for a commit (or a short timeout) so Firefox/Chromium
    /// cannot be flooded by a 165 Hz mouse.
    canvas_request: Option<Size<i32, Logical>>,
    canvas_request_at: Option<Instant>,
    /// A canvas mouse resize owns the gesture until the final commit.
    canvas_resizing: bool,
    /// Mouse released; keep the xdg Resizing state until the final commit or
    /// timeout.
    canvas_finishing: bool,
    /// Distinguishes an old finish timer from a newer resize gesture.
    canvas_generation: u64,
}

pub struct CommitContext {
    pub animations_off: bool,
    pub open: OpenAnimationConfig,
    pub resize: AnimationConfig,
    pub window_drag_active: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CommitResult {
    /// A resize animation was started for this commit.
    pub resized: bool,
    /// The committed size differs from the previous commit (or this is the
    /// first size commit), so layout geometry may need to move the window.
    pub size_changed: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ResizeVisual {
    pub content_scale: Scale<f64>,
    pub tile_size: Size<f64, Logical>,
}

#[derive(Default)]
pub struct WindowStates {
    windows: HashMap<Window, WindowAnimation>,
    finished_closes: Vec<(Window, usize)>,
    queued_canvas_count: usize,
}

impl WindowStates {
    pub fn begin_mouse_resize(&mut self, window: &Window) {
        // A mouse resize owns the geometry. Do not let an open animation or a
        // previous layout transition keep drawing through the drag.
        self.settle_transitions(window);
        let entry = self.windows.entry(window.clone()).or_default();
        entry.resize_suppression.begin_drag();
        entry.resize = None;
    }

    pub fn end_mouse_resize(&mut self, window: &Window, now: Instant) {
        if let Some(entry) = self.windows.get_mut(window) {
            entry.resize_suppression.end_drag(now);
        }
    }

    /// Start an interactive canvas resize. The first configure is allowed
    /// immediately; subsequent ones are commit-gated.
    pub fn begin_canvas_resize(&mut self, window: &Window) {
        let entry = self.windows.entry(window.clone()).or_default();
        entry.canvas_resizing = true;
        entry.canvas_finishing = false;
        entry.canvas_request = None;
        entry.canvas_request_at = None;
        if entry.queued_canvas_size.take().is_some() {
            self.queued_canvas_count = self.queued_canvas_count.saturating_sub(1);
        }
        entry.canvas_generation = entry.canvas_generation.wrapping_add(1);
    }

    pub fn canvas_generation(&self, window: &Window) -> u64 {
        self.windows
            .get(window)
            .map_or(0, |entry| entry.canvas_generation)
    }

    /// Record the size configure most recently sent to the client.
    pub fn mark_canvas_configure(
        &mut self,
        window: &Window,
        size: Size<i32, Logical>,
        now: Instant,
    ) {
        let entry = self.windows.entry(window.clone()).or_default();
        entry.canvas_request = Some(size);
        entry.canvas_request_at = Some(now);
    }

    /// True while a canvas configure has been sent but not acknowledged.
    pub fn canvas_configure_pending(&self, window: &Window) -> bool {
        self.windows
            .get(window)
            .is_some_and(|entry| entry.canvas_request.is_some())
    }

    /// Take queued interactive sizes whose previous configure has been
    /// committed, or whose timeout has elapsed.
    pub fn take_ready_queued_canvas_sizes(
        &mut self,
        now: Instant,
    ) -> Vec<(Window, Size<i32, Logical>)> {
        if self.queued_canvas_count == 0 {
            return Vec::new();
        }
        let mut sizes = Vec::new();
        for (window, animation) in &mut self.windows {
            if animation.queued_canvas_size.is_none() {
                continue;
            }
            if animation.canvas_request.is_some()
                && animation.canvas_request_at.is_some_and(|sent| {
                    now.saturating_duration_since(sent) < CANVAS_CONFIGURE_TIMEOUT
                })
            {
                continue;
            }
            if let Some(size) = animation.queued_canvas_size.take() {
                self.queued_canvas_count = self.queued_canvas_count.saturating_sub(1);
                sizes.push((window.clone(), size));
            }
        }
        sizes
    }

    /// Note a client commit. Returns true when this was the final commit of a
    /// canvas resize and the xdg Resizing state should be cleared.
    pub fn note_canvas_commit(
        &mut self,
        window: &Window,
        size: Size<i32, Logical>,
        now: Instant,
    ) -> bool {
        let Some(entry) = self.windows.get_mut(window) else {
            return false;
        };
        let acked = match entry.canvas_request {
            Some(requested) => {
                ((requested.w - size.w).abs() <= 1 && (requested.h - size.h).abs() <= 1)
                    || entry.canvas_request_at.is_none_or(|sent| {
                        now.saturating_duration_since(sent) >= CANVAS_CONFIGURE_TIMEOUT
                    })
            }
            None => true,
        };
        if acked {
            entry.canvas_request = None;
            entry.canvas_request_at = None;
        }
        if entry.canvas_finishing && acked {
            entry.canvas_finishing = false;
            true
        } else {
            false
        }
    }

    /// Mark the mouse release. The xdg Resizing state remains until the final
    /// commit or the finish timeout.
    pub fn begin_canvas_finish(&mut self, window: &Window) {
        if let Some(entry) = self.windows.get_mut(window) {
            entry.canvas_finishing = true;
        }
    }

    /// Take the active canvas resize state. True means xdg Resizing should be
    /// cleared.
    pub fn take_canvas_resizing(&mut self, window: &Window) -> bool {
        let Some(entry) = self.windows.get_mut(window) else {
            return false;
        };
        if !entry.canvas_resizing {
            return false;
        }
        entry.canvas_resizing = false;
        entry.canvas_finishing = false;
        entry.canvas_request = None;
        entry.canvas_request_at = None;
        true
    }

    /// Keyboard/preset resizes opt back into the resize animation.
    pub fn clear_suppression(&mut self, window: &Window) {
        if let Some(entry) = self.windows.get_mut(window) {
            entry.resize_suppression.clear();
        }
    }

    pub fn remove(&mut self, window: &Window) {
        if self
            .windows
            .remove(window)
            .is_some_and(|entry| entry.queued_canvas_size.is_some())
        {
            self.queued_canvas_count = self.queued_canvas_count.saturating_sub(1);
        }
    }

    /// Drop transient open/resize/layout animations. Close snapshots are left
    /// alone so their fade-out can still finish.
    pub fn settle_transitions(&mut self, window: &Window) {
        if let Some(entry) = self.windows.get_mut(window)
            && entry.closing.is_none()
        {
            entry.open = None;
            entry.open_snapshot = crate::snapshot::SnapshotBuffer::default();
            entry.resize = None;
            entry.transition = None;
        }
    }

    /// Queue the latest interactive canvas size for the next render frame.
    pub fn queue_canvas_size(&mut self, window: &Window, size: Size<i32, Logical>) {
        let entry = self.windows.entry(window.clone()).or_default();
        if entry.queued_canvas_size.replace(size).is_none() {
            self.queued_canvas_count += 1;
        }
    }

    pub fn clear_queued_canvas_size(&mut self, window: &Window) {
        if let Some(entry) = self.windows.get_mut(window)
            && entry.queued_canvas_size.take().is_some()
        {
            self.queued_canvas_count = self.queued_canvas_count.saturating_sub(1);
        }
    }

    pub fn border_buffers_mut(&mut self, window: &Window) -> &mut BorderBuffers {
        &mut self.windows.entry(window.clone()).or_default().borders
    }

    pub fn on_commit(
        &mut self,
        window: &Window,
        size: Size<i32, Logical>,
        now: f64,
        wall_now: Instant,
        context: CommitContext,
    ) -> CommitResult {
        let mut result = CommitResult::default();
        let entry = self.windows.entry(window.clone()).or_default();
        if entry.closing.is_some() {
            // Close animations keep the last committed size while the surface
            // is fading out; do not start open/resize animations on top.
            if size.w > 0 && size.h > 0 {
                entry.last_size = Some(size);
            }
            return result;
        }

        if size.w > 0 && size.h > 0 {
            result.size_changed = entry.last_size != Some(size);

            if !entry.started {
                entry.started = true;
                if !context.animations_off && !context.open.animation.off {
                    entry.open = Some(Animation::new(context.open.animation, 0.0, 1.0, now));
                    tracing::debug!(?size, "window open animation started");
                }
            }

            let changed = entry
                .last_size
                .map(|old| (old.w - size.w).abs() > 1 || (old.h - size.h).abs() > 1)
                .unwrap_or(false);
            let suppressed = entry.resize_suppression.consume_commit(
                wall_now,
                context.window_drag_active,
                changed,
            );

            if changed
                && entry.transition.is_none()
                && !context.window_drag_active
                && !suppressed
                && !context.animations_off
                && !context.resize.off
            {
                let from = if let Some(resize) = &entry.resize {
                    let p = resize.animation.value(now);
                    Size::from((
                        (resize.from.w + (resize.to.w - resize.from.w) * p).max(1.0),
                        (resize.from.h + (resize.to.h - resize.from.h) * p).max(1.0),
                    ))
                } else {
                    let old = entry.last_size.unwrap_or(size);
                    Size::from((old.w as f64, old.h as f64))
                };

                let to = Size::from((size.w as f64, size.h as f64));
                tracing::trace!(?from, ?to, "resize animation");
                entry.resize = Some(SizeAnimation {
                    from,
                    to,
                    animation: Animation::new(context.resize, 0.0, 1.0, now),
                });
                result.resized = true;
            }

            entry.last_size = Some(size);
        } else {
            entry
                .resize_suppression
                .consume_commit(wall_now, context.window_drag_active, false);
        }

        result
    }

    pub fn open_snapshot_mut(&mut self, window: &Window) -> &mut crate::snapshot::SnapshotBuffer {
        &mut self
            .windows
            .entry(window.clone())
            .or_default()
            .open_snapshot
    }

    /// Returns `(alpha, scale)` for the window-open animation.
    pub fn open_style(
        &self,
        window: &Window,
        now: f64,
        config: OpenAnimationConfig,
    ) -> Option<(f32, f64)> {
        let animation = self.windows.get(window)?.open.as_ref()?;
        let scale_progress = animation.value(now);
        let scale = (config.scale_from + (1.0 - config.scale_from) * scale_progress).max(0.01);
        // Do not drive opacity from the overshooting spring value: on the
        // rebound it would dip below 1 and make an otherwise complete window
        // flash translucent. Opacity uses a monotonic progress instead.
        let opacity_progress = animation.monotonic_progress(now);
        let alpha = config.opacity_from + (1.0 - config.opacity_from) * opacity_progress;
        Some((alpha.clamp(0.0, 1.0) as f32, scale))
    }

    /// Animate a window between `from` and `to`. Rectangles are in screen
    /// space when `screen_space` is true (fullscreen), or world space
    /// otherwise (layout movement).
    #[allow(clippy::too_many_arguments)]
    fn start_transition(
        &mut self,
        window: &Window,
        from: Rectangle<f64, Logical>,
        to: Rectangle<f64, Logical>,
        border_from: f32,
        border_to: f32,
        on_top: bool,
        screen_space: bool,
        now: f64,
        config: AnimationConfig,
    ) -> bool {
        if config.off {
            return false;
        }
        tracing::trace!(
            ?from,
            ?to,
            border_from,
            border_to,
            on_top,
            "window transition started"
        );
        let entry = self.windows.entry(window.clone()).or_default();
        entry.open = None;
        entry.open_snapshot = crate::snapshot::SnapshotBuffer::default();
        entry.resize = None;
        entry.transition = Some(RectTransition {
            from,
            to,
            border_from,
            border_to,
            on_top,
            screen_space,
            animation: Animation::new(config, 0.0, 1.0, now),
        });
        true
    }

    /// Keep the last tile snapshot captured before the surface is destroyed.
    pub fn store_unmap_snapshot(
        &mut self,
        window: &Window,
        element: crate::snapshot::SnapshotRenderElement,
        world_rect: Rectangle<f64, Logical>,
    ) {
        let entry = self.windows.entry(window.clone()).or_default();
        if entry.unmap_snapshot.is_none() {
            entry.unmap_snapshot = Some(StoredSnapshot {
                element,
                world_rect,
            });
        }
    }

    pub fn take_unmap_snapshot(
        &mut self,
        window: &Window,
    ) -> Option<(
        crate::snapshot::SnapshotRenderElement,
        Rectangle<f64, Logical>,
    )> {
        self.windows
            .get_mut(window)
            .and_then(|entry| entry.unmap_snapshot.take())
            .map(|snapshot| (snapshot.element, snapshot.world_rect))
    }

    /// Snap open/resize/layout transitions when animations are disabled by a
    /// live config reload. Close snapshots are left to finish normally.
    pub fn settle_all_transitions(&mut self) {
        for animation in self.windows.values_mut() {
            animation.open = None;
            animation.open_snapshot = crate::snapshot::SnapshotBuffer::default();
            animation.resize = None;
            animation.transition = None;
        }
    }

    /// Start a close animation from a whole-tile snapshot.
    #[allow(clippy::too_many_arguments)]
    pub fn start_close_snapshot(
        &mut self,
        window: &Window,
        element: crate::snapshot::SnapshotRenderElement,
        from: Rectangle<f64, Logical>,
        to: Rectangle<f64, Logical>,
        output_index: usize,
        workspace_id: u64,
        now: f64,
        config: AnimationConfig,
    ) -> bool {
        if config.off {
            return false;
        }
        let entry = self.windows.entry(window.clone()).or_default();
        entry.open = None;
        entry.open_snapshot = crate::snapshot::SnapshotBuffer::default();
        entry.resize = None;
        entry.transition = None;
        tracing::debug!(?from, ?to, "close snapshot animation started");
        entry.closing = Some(ClosingSnapshot {
            element,
            from,
            to,
            animation: Animation::new(config, 0.0, 1.0, now),
            output_index,
            workspace_id,
            finished: false,
        });
        true
    }

    pub fn is_closing(&self, window: &Window) -> bool {
        self.windows
            .get(window)
            .is_some_and(|animation| animation.closing.is_some())
    }

    /// Closing snapshot elements for the visible workspace on one output.
    pub fn closing_elements(
        &self,
        output_index: usize,
        workspace_id: u64,
        now: f64,
        camera: Point<f64, Logical>,
    ) -> impl Iterator<Item = crate::snapshot::SnapshotRenderElement> + '_ {
        self.windows.values().filter_map(move |animation| {
            let snapshot = animation.closing.as_ref()?;
            if snapshot.finished
                || snapshot.output_index != output_index
                || snapshot.workspace_id != workspace_id
            {
                return None;
            }
            let p = snapshot.animation.value(now);
            let alpha = (1.0 - snapshot.animation.monotonic_progress(now)).clamp(0.0, 1.0) as f32;
            let rect = Rectangle::new(
                Point::from((
                    snapshot.from.loc.x + (snapshot.to.loc.x - snapshot.from.loc.x) * p - camera.x,
                    snapshot.from.loc.y + (snapshot.to.loc.y - snapshot.from.loc.y) * p - camera.y,
                )),
                Size::from((
                    (snapshot.from.size.w + (snapshot.to.size.w - snapshot.from.size.w) * p)
                        .max(1.0),
                    (snapshot.from.size.h + (snapshot.to.size.h - snapshot.from.size.h) * p)
                        .max(1.0),
                )),
            );
            Some(snapshot.element.clone().with_rect(rect).with_alpha(alpha))
        })
    }

    pub fn take_finished_closes(&mut self) -> Vec<(Window, usize)> {
        std::mem::take(&mut self.finished_closes)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start_fullscreen_transition(
        &mut self,
        window: &Window,
        from: Rectangle<f64, Logical>,
        to: Rectangle<f64, Logical>,
        border_from: f32,
        border_to: f32,
        now: f64,
        config: AnimationConfig,
    ) -> bool {
        self.start_transition(
            window,
            from,
            to,
            border_from,
            border_to,
            true,
            true,
            now,
            config,
        )
    }

    pub fn start_layout_transition(
        &mut self,
        window: &Window,
        from: Rectangle<f64, Logical>,
        to: Rectangle<f64, Logical>,
        now: f64,
        config: AnimationConfig,
    ) -> bool {
        self.start_transition(window, from, to, 1.0, 1.0, false, false, now, config)
    }

    pub fn cancel_transition(&mut self, window: &Window) {
        if let Some(entry) = self.windows.get_mut(window) {
            entry.transition = None;
        }
    }

    /// Current transition rectangle and presentation details.
    pub fn transition_style(&self, window: &Window, now: f64) -> Option<TransitionStyle> {
        let transition = self.windows.get(window)?.transition.as_ref()?;
        let p = transition.animation.value(now);
        let rect = Rectangle::new(
            Point::from((
                transition.from.loc.x + (transition.to.loc.x - transition.from.loc.x) * p,
                transition.from.loc.y + (transition.to.loc.y - transition.from.loc.y) * p,
            )),
            Size::from((
                (transition.from.size.w + (transition.to.size.w - transition.from.size.w) * p)
                    .max(1.0),
                (transition.from.size.h + (transition.to.size.h - transition.from.size.h) * p)
                    .max(1.0),
            )),
        );
        let border_progress = transition.animation.monotonic_progress(now) as f32;
        let border_alpha = transition.border_from
            + (transition.border_to - transition.border_from) * border_progress;
        Some(TransitionStyle {
            rect,
            border_alpha: border_alpha.clamp(0.0, 1.0),
            on_top: transition.on_top,
            screen_space: transition.screen_space,
        })
    }

    pub fn has_top_transition_in<'a>(&self, mut windows: impl Iterator<Item = &'a Window>) -> bool {
        windows.any(|window| {
            self.windows
                .get(window)
                .and_then(|animation| animation.transition.as_ref())
                .is_some_and(|transition| transition.on_top)
        })
    }

    /// Interpolated content scale and full tile size for the post-commit
    /// resize animation. The client reflows live; each committed size step is
    /// interpolated rather than snapped. The tile size includes both borders,
    /// so borders animate with the content instead of jumping on commit.
    pub fn resize_visual(&self, window: &Window, inset: f64, now: f64) -> Option<ResizeVisual> {
        let resize = self.windows.get(window)?.resize.as_ref()?;
        let committed = window.geometry().size;
        if committed.w <= 0 || committed.h <= 0 {
            return None;
        }

        let p = resize.animation.value(now);
        let width = (resize.from.w + (resize.to.w - resize.from.w) * p).max(1.0);
        let height = (resize.from.h + (resize.to.h - resize.from.h) * p).max(1.0);
        let inset = inset.max(0.0);
        Some(ResizeVisual {
            content_scale: Scale::from((width / committed.w as f64, height / committed.h as f64)),
            tile_size: Size::from((width + 2.0 * inset, height + 2.0 * inset)),
        })
    }

    /// Advance the open/resize/close animations for windows accepted by
    /// `matches`. Returns whether any matching window had an animation at the
    /// start of the tick (including the final snapping frame).
    ///
    /// The predicate receives the window and its closing-snapshot output index
    /// (if closing), so per-output frame pacing can tick in place without
    /// collecting owned windows.
    pub fn tick_windows_matching(
        &mut self,
        now: f64,
        mut matches: impl FnMut(&Window, Option<usize>) -> bool,
    ) -> bool {
        let mut active = false;

        for (window, animation) in &mut self.windows {
            let closing_output = animation
                .closing
                .as_ref()
                .filter(|closing| !closing.finished)
                .map(|closing| closing.output_index);
            let has_active = animation.open.is_some()
                || animation.resize.is_some()
                || animation.transition.is_some()
                || closing_output.is_some();
            if !has_active || !matches(window, closing_output) {
                continue;
            }
            if let Some(open) = &mut animation.open {
                if open.is_done(now) {
                    animation.open = None;
                    animation.open_snapshot = crate::snapshot::SnapshotBuffer::default();
                }
                active = true;
            }
            if let Some(resize) = &mut animation.resize {
                if resize.animation.is_done(now) {
                    animation.resize = None;
                }
                active = true;
            }
            if let Some(transition) = &mut animation.transition {
                if transition.animation.is_done(now) {
                    animation.transition = None;
                }
                active = true;
            }
            if let Some(closing) = &mut animation.closing
                && !closing.finished
            {
                if closing.animation.is_done(now) {
                    closing.finished = true;
                    self.finished_closes
                        .push((window.clone(), closing.output_index));
                }
                active = true;
            }
        }

        active
    }

    /// Output that owns an active closing snapshot. The Wayland window may
    /// already be gone from `Space`, so it cannot be found by surface lookup.
    pub fn closing_output_for(&self, window: &Window) -> Option<usize> {
        let closing = self.windows.get(window)?.closing.as_ref()?;
        (!closing.finished).then_some(closing.output_index)
    }

    /// Windows with an animation that still needs frames. Closing windows
    /// count until their close snapshot finishes.
    pub fn active_windows(&self) -> impl Iterator<Item = &Window> {
        self.windows.iter().filter_map(|(window, animation)| {
            let active = animation.open.is_some()
                || animation.resize.is_some()
                || animation.transition.is_some()
                || animation
                    .closing
                    .as_ref()
                    .is_some_and(|closing| !closing.finished);
            active.then_some(window)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_scale_interpolates_between_committed_sizes() {
        let resize = SizeAnimation {
            from: Size::from((100.0, 200.0)),
            to: Size::from((200.0, 400.0)),
            animation: Animation::new(
                AnimationConfig::easing(100, crate::animation::Curve::Linear),
                0.0,
                1.0,
                0.0,
            ),
        };

        // Pure scale math without a real Wayland window.
        let p = resize.animation.value(0.05);
        let width = resize.from.w + (resize.to.w - resize.from.w) * p;
        let height = resize.from.h + (resize.to.h - resize.from.h) * p;
        assert!((width - 150.0).abs() < 0.001);
        assert!((height - 300.0).abs() < 0.001);
    }

    #[test]
    fn open_animation_config_clamps_alpha() {
        let config = OpenAnimationConfig {
            animation: AnimationConfig::easing(100, crate::animation::Curve::Linear),
            scale_from: 0.8,
            opacity_from: 0.0,
        };
        let animation = Animation::new(config.animation, 0.0, 1.0, 0.0);
        let progress = animation.value(0.05);
        let alpha = config.opacity_from + (1.0 - config.opacity_from) * progress;
        assert!((alpha - 0.5).abs() < 0.001);
    }

    #[test]
    fn resize_suppression_survives_non_resize_commits_until_final_size_or_timeout() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut suppression = ResizeSuppression::default();
        suppression.begin_drag();
        assert!(suppression.consume_commit(ms(0), true, false));
        assert!(suppression.consume_commit(ms(100), true, true));

        // Release drag at +200 ms; non-resize frame commits must not clear
        // suppression before the final resized buffer lands.
        suppression.end_drag(ms(200));
        assert!(suppression.consume_commit(ms(220), false, false));
        assert!(suppression.consume_commit(ms(250), false, false));
        // Final resized buffer arrives: suppressed once, then cleared.
        assert!(suppression.consume_commit(ms(280), false, true));
        assert!(!suppression.consume_commit(ms(300), false, true));
    }

    #[test]
    fn resize_suppression_expires_after_grace_window() {
        let t0 = Instant::now();
        let mut suppression = ResizeSuppression::default();
        suppression.begin_drag();
        suppression.end_drag(t0);
        // No size change arrives within the grace window; later resizes animate.
        assert!(!suppression.consume_commit(
            t0 + RESIZE_SUPPRESSION_GRACE + Duration::from_millis(50),
            false,
            true
        ));
        assert_eq!(suppression, ResizeSuppression::None);
    }
}
