use std::{sync::Mutex, time::Duration};

use anyhow::{Result, anyhow};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::{
                Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                solid::SolidColorRenderElement,
                surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
                utils::RescaleRenderElement,
            },
            gles::GlesRenderer,
        },
    },
    desktop::utils::{OutputPresentationFeedback, take_presentation_feedback_surface_tree},
    input::pointer::{CursorImageAttributes, CursorImageStatus},
    output::Output,
    reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
    render_elements,
    utils::{IsAlive, Point, Scale, Transform},
    wayland::{
        compositor::with_states, presentation::Refresh, shell::wlr_layer::Layer as WlrLayer,
    },
};

use crate::state::EvaWM;
#[cfg(feature = "nested")]
use crate::state::NestedBackend;

mod border;
mod layer;
pub(crate) mod window;

render_elements! {
    pub PointerRenderElement<=GlesRenderer>;
    Surface=WaylandSurfaceRenderElement<GlesRenderer>,
    Memory=MemoryRenderBufferRenderElement<GlesRenderer>,
}

render_elements! {
    pub WindowInnerRenderElements<=GlesRenderer>;
    Layer=WaylandSurfaceRenderElement<GlesRenderer>,
    ScaledWindow=RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>,
    Border=SolidColorRenderElement,
    Snapshot=crate::snapshot::SnapshotRenderElement,
    ScaledSnapshot=RescaleRenderElement<crate::snapshot::SnapshotRenderElement>,
}

render_elements! {
    pub OutputRenderElements<=GlesRenderer>;
    Pointer=PointerRenderElement,
    Layer=WaylandSurfaceRenderElement<GlesRenderer>,
    Window=WindowInnerRenderElements,
    OverviewWindow=RescaleRenderElement<WindowInnerRenderElements>,
}

#[cfg(feature = "nested")]
pub fn render(state: &mut EvaWM) -> Result<()> {
    render_output(state, 0)
}

pub fn render_output(state: &mut EvaWM, output_index: usize) -> Result<()> {
    // libseat has paused the DRM session (VT switch or suspend). Do not touch
    // the device until resume; pending redraws are replaced by a full redraw.
    if state.session_paused {
        return Ok(());
    }
    if state.session().is_some_and(|session| !session.is_active()) {
        // libseat can revoke DRM before PauseSession is delivered. Stop
        // rendering immediately; ActivateSession will clear this and force a
        // full redraw.
        state.session_paused = true;
        return Ok(());
    }
    // Coalesce interactive canvas resize updates to one configure per frame.
    state.flush_canvas_configures();
    let now = state.animation_now();
    state.tick_animations_for_output(output_index, now);

    if state.monitors_off
        || !state
            .outputs
            .get(output_index)
            .is_some_and(|output| output.needs_redraw)
    {
        return Ok(());
    }

    // Clear the flag before building the frame, not after it. Work done while
    // rendering (space refresh, popup cleanup, a client commit) may request
    // another frame; that request must survive this render instead of being
    // overwritten by the post-render animation check.
    state.outputs[output_index].needs_redraw = false;

    let full_redraw = state.outputs[output_index].full_redraw > 0;
    if full_redraw && let Some(session) = state.session() {
        session.reset_buffer_ages_output(output_index);
    }

    let (render_result, schedule_other_outputs) = match state.backend.take() {
        #[cfg(feature = "nested")]
        Some(crate::state::Backend::Nested(mut nested)) => {
            let result = render_nested(&mut nested, state, output_index, full_redraw);
            state.backend = Some(crate::state::Backend::Nested(nested));
            (result, false)
        }
        Some(crate::state::Backend::Session(mut session)) => {
            let result = session.render(state, output_index);
            state.backend = Some(crate::state::Backend::Session(session));
            result
        }
        None => return Err(anyhow!("no backend")),
    };
    if schedule_other_outputs {
        // A full DRM device reset during error recovery canceled pending page
        // flips on every other output; re-schedule them even if this output's
        // retry failed so they do not stay frozen waiting for a VBlank that
        // will never arrive.
        for other in 0..state.outputs.len() {
            if other != output_index {
                state.request_redraw_output(other);
            }
        }
    }
    let presented = render_result?;

    let animating = state.output_animations_active(output_index);
    if let Some(output) = state.outputs.get_mut(output_index) {
        output.needs_redraw |= animating;
        output.full_redraw = output.full_redraw.saturating_sub(1);
        if output.render_retries > 0 {
            tracing::info!(
                output = output_index,
                retries = output.render_retries,
                "render recovered"
            );
        }
        output.render_retries = 0;
        output.render_retry_armed = false;
    }
    if presented && let Some(output) = state.outputs.get_mut(output_index) {
        output.present_seq = output.present_seq.wrapping_add(1);
    }
    state.handle_frame_callbacks(output_index, presented);
    if presented && !state.pending_screencopies.is_empty() {
        crate::screencopy::flush_pending_for_output(state, output_index);
    }
    if !state.idle_inhibitors.is_empty() {
        // Visibility can change without an inhibitor event (workspace switch,
        // canvas pan, window move). Refresh while any inhibitor exists.
        state.refresh_idle_inhibit();
    }
    // Refreshing Space state is cheap and keeps hidden outputs ready for
    // their next frame without rendering them.
    for output in &mut state.outputs {
        output.space.refresh();
    }
    state.popups.cleanup();
    let _ = state.display_handle.flush_clients();

    if animating && state.is_nested() {
        state.request_redraw_output(output_index);
    }

    Ok(())
}

/// Collect committed presentation feedback for surfaces rendered on this output.
pub(crate) fn take_output_presentation_feedback(
    state: &EvaWM,
    output_index: usize,
) -> OutputPresentationFeedback {
    let output = state.outputs[output_index].output.clone();
    let mut feedback = OutputPresentationFeedback::new(&output);

    state.for_each_frame_callback_surface(output_index, |surface| {
        take_presentation_feedback_surface_tree(
            surface,
            &mut feedback,
            |_, _| Some(output.clone()),
            |_, _| wp_presentation_feedback::Kind::empty(),
        );
    });

    feedback
}

fn feedback_refresh(mode: Option<smithay::output::Mode>) -> Refresh {
    mode.and_then(|mode| {
        if mode.refresh <= 0 {
            return None;
        }
        let seconds = 1_000.0 / f64::from(mode.refresh);
        (seconds.is_finite() && seconds > 0.0)
            .then(|| Refresh::fixed(Duration::from_secs_f64(seconds)))
    })
    .unwrap_or(Refresh::Unknown)
}

pub(crate) fn present_feedback(
    mut feedback: OutputPresentationFeedback,
    output: &Output,
    metadata: Option<&smithay::backend::drm::DrmEventMetadata>,
) {
    let refresh = feedback_refresh(output.current_mode());
    let clock = smithay::utils::Clock::<smithay::utils::Monotonic>::new();
    let (time, seq, flags) = match metadata {
        Some(meta) => {
            let (time, hw_clock) = match meta.time {
                smithay::backend::drm::DrmEventTime::Monotonic(duration) => (duration, true),
                smithay::backend::drm::DrmEventTime::Realtime(_) => (clock.now().into(), false),
            };
            let mut flags = wp_presentation_feedback::Kind::Vsync
                | wp_presentation_feedback::Kind::HwCompletion;
            if hw_clock {
                flags |= wp_presentation_feedback::Kind::HwClock;
            }
            (time, u64::from(meta.sequence), flags)
        }
        None => (clock.now().into(), 0, wp_presentation_feedback::Kind::Vsync),
    };
    feedback.presented::<Duration, smithay::utils::Monotonic>(time, refresh, seq, flags);
}

#[cfg(feature = "nested")]
fn render_nested(
    nested: &mut NestedBackend,
    state: &mut EvaWM,
    output_index: usize,
    full_redraw: bool,
) -> Result<bool> {
    let age = if full_redraw {
        0
    } else {
        nested.graphics.buffer_age().unwrap_or(0)
    };

    let damage = {
        let (renderer, mut framebuffer) =
            nested.graphics.bind().map_err(|err| anyhow!("{err:?}"))?;

        let elements = build_elements_for_output(renderer, state, output_index, true);

        let result = nested
            .damage_tracker
            .render_output(
                renderer,
                &mut framebuffer,
                age,
                &elements,
                state.config.layout.background.components(),
            )
            .map_err(|err| anyhow!("{err:?}"))?;

        if result.damage.is_some()
            && let Err(err) = result.sync.wait()
        {
            tracing::warn!("error waiting for nested frame completion: {err:?}");
        }

        result.damage
    };

    tracing::trace!(
        output = output_index,
        windows = state.outputs[output_index].space.elements().count(),
        has_damage = damage.is_some(),
        "render nested"
    );

    if let Some(damage) = damage {
        nested
            .graphics
            .submit(Some(damage))
            .map_err(|err| anyhow!("{err:?}"))?;
        let feedback = take_output_presentation_feedback(state, output_index);
        present_feedback(feedback, &state.outputs[output_index].output, None);
    }

    Ok(damage.is_some())
}

pub fn build_elements_for_output(
    renderer: &mut GlesRenderer,
    state: &mut EvaWM,
    output_index: usize,
    include_cursor: bool,
) -> Vec<OutputRenderElements> {
    if let CursorImageStatus::Surface(surface) = &state.cursor_status
        && !surface.alive()
    {
        state.cursor_status = CursorImageStatus::default_named();
    }

    let scale = Scale::from(
        state.outputs[output_index]
            .output
            .current_scale()
            .fractional_scale(),
    );
    let status = state.cursor_status.clone();

    let (window_elements, above_top_elements) = window::elements(renderer, state, output_index);
    let overview_zoom = state.outputs[output_index].overview_zoom.value.max(0.05);
    let overview_active = (overview_zoom - 1.0).abs() > 0.001;
    let overview_pivot = state.outputs[output_index]
        .overview_pivot()
        .to_physical(scale)
        .to_i32_round();
    let wrap_window = |element: WindowInnerRenderElements| {
        if overview_active {
            OutputRenderElements::OverviewWindow(RescaleRenderElement::from_element(
                element,
                overview_pivot,
                overview_zoom,
            ))
        } else {
            OutputRenderElements::Window(element)
        }
    };

    // Refresh cached named cursor buffer before building the output list.
    let draw_cursor = include_cursor && output_index == state.pointer_output;
    if draw_cursor {
        let cursor_scale = scale.x.round().max(1.0) as u32;
        if let CursorImageStatus::Named(icon) = &status
            && (state.cursor_icon.as_ref() != Some(icon) || state.cursor_scale != cursor_scale)
        {
            let (buffer, hotspot) = state
                .cursor_cache
                .entry((*icon, cursor_scale))
                .or_insert_with(|| {
                    let image = state.cursor.image_for(*icon, cursor_scale);
                    let buffer = MemoryRenderBuffer::from_slice(
                        &image.pixels_rgba,
                        Fourcc::Argb8888,
                        (image.width as i32, image.height as i32),
                        cursor_scale as i32,
                        Transform::Normal,
                        None,
                    );
                    let hotspot = Point::from((
                        (image.xhot as f64 / f64::from(cursor_scale)).round() as i32,
                        (image.yhot as f64 / f64::from(cursor_scale)).round() as i32,
                    ));
                    (buffer, hotspot)
                });
            state.cursor_buffer = buffer.clone();
            state.cursor_hotspot = *hotspot;
            state.cursor_icon = Some(*icon);
            state.cursor_scale = cursor_scale;
        }
    }

    let output = &state.outputs[output_index].output;
    let mut elements: Vec<OutputRenderElements> =
        Vec::with_capacity(above_top_elements.len() + window_elements.len() + 4);

    // Top-to-bottom: the damage tracker renders the list in reverse. Fullscreen
    // windows go above the Top layer (bars) but below Overlay (launchers).
    if draw_cursor {
        let hotspot = match &status {
            CursorImageStatus::Surface(surface) => with_states(surface, |states| {
                states
                    .data_map
                    .get::<Mutex<CursorImageAttributes>>()
                    .and_then(|attrs| attrs.lock().ok().map(|guard| guard.hotspot))
            })
            .unwrap_or_default(),
            _ => state.cursor_hotspot,
        };
        let cursor_location = (state.pointer_pos - hotspot.to_f64())
            .to_physical(scale)
            .to_i32_round();
        match &status {
            CursorImageStatus::Hidden => {}
            CursorImageStatus::Named(_) => {
                if let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    cursor_location.to_f64(),
                    &state.cursor_buffer,
                    None,
                    None,
                    None,
                    Kind::Cursor,
                ) {
                    elements.push(OutputRenderElements::Pointer(PointerRenderElement::from(
                        element,
                    )));
                }
            }
            CursorImageStatus::Surface(surface) => {
                let rendered: Vec<PointerRenderElement> = render_elements_from_surface_tree(
                    renderer,
                    surface,
                    cursor_location,
                    scale,
                    1.0,
                    Kind::Cursor,
                );
                elements.extend(rendered.into_iter().map(OutputRenderElements::Pointer));
            }
        }
    }
    if state.pointer_output == output_index
        && let Some(icon) = &state.dnd_icon
        && icon.surface.alive()
    {
        let location = (state.pointer_pos + icon.offset.to_f64())
            .to_physical(scale)
            .to_i32_round();
        let rendered = render_elements_from_surface_tree(
            renderer,
            &icon.surface,
            location,
            scale,
            1.0,
            Kind::Unspecified,
        );
        elements.extend(rendered.into_iter().map(OutputRenderElements::Layer));
    }
    layer::add_layer_elements(renderer, output, WlrLayer::Overlay, scale, &mut elements);
    elements.extend(above_top_elements.into_iter().map(wrap_window));
    layer::add_layer_elements(renderer, output, WlrLayer::Top, scale, &mut elements);
    elements.extend(window_elements.into_iter().map(wrap_window));
    layer::add_layer_elements(renderer, output, WlrLayer::Bottom, scale, &mut elements);
    layer::add_layer_elements(renderer, output, WlrLayer::Background, scale, &mut elements);

    elements
}

#[cfg(test)]
mod tests {
    use super::feedback_refresh;
    use smithay::output::Mode;
    use smithay::utils::Size;
    use smithay::wayland::presentation::Refresh;

    #[test]
    fn feedback_refresh_handles_zero_and_negative_refresh() {
        let size = Size::from((1920, 1080));
        assert!(matches!(
            feedback_refresh(Some(Mode { size, refresh: 0 })),
            Refresh::Unknown
        ));
        assert!(matches!(
            feedback_refresh(Some(Mode { size, refresh: -1 })),
            Refresh::Unknown
        ));
        assert!(matches!(
            feedback_refresh(Some(Mode {
                size,
                refresh: 60_000
            })),
            Refresh::Fixed(_)
        ));
    }
}
