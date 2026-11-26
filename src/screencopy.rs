//! wlr-screencopy support.
//!
//! This intentionally advertises SHM capture only. It is enough for grim,
//! wfreeze, slurp, and xdg-desktop-portal-wlr; adding linux-dmabuf capture can
//! come later without changing the protocol shape.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, ContextId, ExportMem, Offscreen, Renderer, Texture as _,
            damage::OutputDamageTracker,
            element::utils::{Relocate, RelocateRenderElement},
            gles::GlesTexture,
        },
    },
    output::Output,
    reexports::{
        wayland_protocols_wlr::screencopy::v1::server::{
            zwlr_screencopy_frame_v1::{self, Flags, ZwlrScreencopyFrameV1},
            zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
            protocol::{wl_buffer::WlBuffer, wl_shm::Format},
        },
    },
    utils::{
        Buffer, Clock, Logical, Monotonic, Physical, Point, Rectangle, Scale, Size, Transform,
    },
    wayland::shm,
};

use crate::{render::build_elements_for_output, state::EvaWM};

const VERSION: u32 = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
struct ScreencopyDamageKey {
    output_index: usize,
    size: Size<i32, Physical>,
    transform: Transform,
    region_loc: Point<i32, Physical>,
}

/// Reusable offscreen texture and damage tracker for screencopy frames so
/// continuous screen sharing does not allocate/free a full-res GL texture per frame.
#[derive(Default)]
pub(crate) struct ScreencopyCache {
    texture: Option<GlesTexture>,
    texture_size: Size<i32, Buffer>,
    context: Option<ContextId<GlesTexture>>,
    damage: Option<OutputDamageTracker>,
    damage_key: Option<ScreencopyDamageKey>,
}

pub fn init(display: &DisplayHandle) {
    display.create_global::<EvaWM, ZwlrScreencopyManagerV1, _>(VERSION, ());
}

/// Per-manager map of `output.name() -> (last presented sequence, timestamp)`.
/// `xdg-desktop-portal-wlr` reuses one manager across preview and stream casts,
/// so a `copy_with_damage` request is deferred only when the output has not
/// changed *and* this manager already received that frame recently.
type ManagerSeqMap = Arc<Mutex<HashMap<String, (u64, Instant)>>>;

const IDLE_RECOPY_WINDOW: Duration = Duration::from_millis(250);

struct ScreencopyFrameData {
    output: Output,
    buffer_size: Size<i32, Physical>,
    region_loc: Point<i32, Physical>,
    overlay_cursor: bool,
    copied: AtomicBool,
    manager_seqs: ManagerSeqMap,
}

/// A `copy_with_damage` request deferred until the target output actually
/// presents a new frame.
pub struct PendingScreencopy {
    frame: ZwlrScreencopyFrameV1,
    buffer: WlBuffer,
    output: Output,
    buffer_size: Size<i32, Physical>,
    region_loc: Point<i32, Physical>,
    overlay_cursor: bool,
    manager_seqs: ManagerSeqMap,
}

fn fail_frame(frame: New<ZwlrScreencopyFrameV1>, data_init: &mut DataInit<'_, EvaWM>) {
    let frame = data_init.init(frame, None);
    frame.failed();
}

impl GlobalDispatch<ZwlrScreencopyManagerV1, ()> for EvaWM {
    fn bind(
        _state: &mut Self,
        _display: &DisplayHandle,
        _client: &Client,
        manager: New<ZwlrScreencopyManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(manager, Arc::new(Mutex::new(HashMap::new())));
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ManagerSeqMap> for EvaWM {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _manager: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        manager_seqs: &ManagerSeqMap,
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let (frame, overlay_cursor, buffer_size, region_loc, output) = match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => {
                let Some(output) = Output::from_resource(&output) else {
                    fail_frame(frame, data_init);
                    return;
                };
                let Some(mode) = output.current_mode() else {
                    fail_frame(frame, data_init);
                    return;
                };
                if mode.size.w <= 0 || mode.size.h <= 0 {
                    fail_frame(frame, data_init);
                    return;
                }
                (
                    frame,
                    overlay_cursor != 0,
                    mode.size,
                    Point::from((0, 0)),
                    output,
                )
            }
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                x,
                y,
                width,
                height,
                output,
            } => {
                if width <= 0 || height <= 0 {
                    fail_frame(frame, data_init);
                    return;
                }

                let Some(output) = Output::from_resource(&output) else {
                    fail_frame(frame, data_init);
                    return;
                };
                let Some(mode) = output.current_mode() else {
                    fail_frame(frame, data_init);
                    return;
                };

                let transform = output.current_transform();
                let output_physical_size = transform.transform_size(mode.size);
                let output_rect = Rectangle::from_size(output_physical_size);
                let scale = output.current_scale().fractional_scale();
                let rect = Rectangle::<i32, Logical>::new(
                    Point::from((x, y)),
                    Size::from((width, height)),
                );
                let physical_rect = rect.to_physical_precise_round(scale);
                let Some(clamped_rect) = physical_rect.intersection(output_rect) else {
                    fail_frame(frame, data_init);
                    return;
                };
                let untransformed_rect = transform
                    .invert()
                    .transform_rect_in(clamped_rect, &output_physical_size);
                if untransformed_rect.size.w <= 0 || untransformed_rect.size.h <= 0 {
                    fail_frame(frame, data_init);
                    return;
                }
                (
                    frame,
                    overlay_cursor != 0,
                    untransformed_rect.size,
                    clamped_rect.loc,
                    output,
                )
            }
            zwlr_screencopy_manager_v1::Request::Destroy => return,
            _ => return,
        };

        let frame = data_init.init(
            frame,
            Some(ScreencopyFrameData {
                output,
                buffer_size,
                region_loc,
                overlay_cursor,
                copied: AtomicBool::new(false),
                manager_seqs: Arc::clone(manager_seqs),
            }),
        );

        frame.buffer(
            Format::Xrgb8888,
            buffer_size.w as u32,
            buffer_size.h as u32,
            (buffer_size.w * 4) as u32,
        );
        if frame.version() >= 3 {
            frame.buffer_done();
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, Option<ScreencopyFrameData>> for EvaWM {
    fn request(
        state: &mut Self,
        _client: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &Option<ScreencopyFrameData>,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let (buffer, with_damage) = match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => (buffer, false),
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => (buffer, true),
            zwlr_screencopy_frame_v1::Request::Destroy => {
                state
                    .pending_screencopies
                    .retain(|pending| pending.frame != *frame);
                return;
            }
            _ => return,
        };

        let Some(data) = data else {
            // `fail_frame` already sent `frame.failed()` on creation.
            return;
        };

        if data.copied.swap(true, Ordering::SeqCst) {
            frame.post_error(
                zwlr_screencopy_frame_v1::Error::AlreadyUsed,
                "copy was already requested",
            );
            return;
        }

        if let Err(err) = validate_shm_buffer(&buffer, data.buffer_size) {
            frame.post_error(
                zwlr_screencopy_frame_v1::Error::InvalidBuffer,
                err.to_string(),
            );
            return;
        }

        let current_seq = state
            .outputs
            .iter()
            .find(|output| output.output == data.output)
            .map(|output| output.present_seq);

        if with_damage {
            let output_name = data.output.name();
            let already_sent_recently = current_seq.is_some_and(|seq| {
                data.manager_seqs
                    .lock()
                    .ok()
                    .and_then(|seqs| seqs.get(&output_name).copied())
                    .is_some_and(|(last_seq, last_at)| {
                        last_seq == seq && last_at.elapsed() < IDLE_RECOPY_WINDOW
                    })
            });
            if already_sent_recently {
                // We already delivered this exact presented frame to this
                // client; wait until the output presents new damage so an idle
                // desktop does not busy-spin.
                state.pending_screencopies.push(PendingScreencopy {
                    frame: frame.clone(),
                    buffer,
                    output: data.output.clone(),
                    buffer_size: data.buffer_size,
                    region_loc: data.region_loc,
                    overlay_cursor: data.overlay_cursor,
                    manager_seqs: Arc::clone(&data.manager_seqs),
                });
                return;
            }
        }

        if complete_copy(
            state,
            frame,
            &buffer,
            &data.output,
            data.buffer_size,
            data.region_loc,
            data.overlay_cursor,
            with_damage,
        ) && let Some(seq) = current_seq
            && let Ok(mut seqs) = data.manager_seqs.lock()
        {
            seqs.insert(data.output.name(), (seq, Instant::now()));
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        frame: &ZwlrScreencopyFrameV1,
        _data: &Option<ScreencopyFrameData>,
    ) {
        state
            .pending_screencopies
            .retain(|pending| pending.frame != *frame);
    }
}

/// Fulfill any deferred `copy_with_damage` requests waiting on `output_index`
/// right after that output presents a frame with damage.
pub(crate) fn flush_pending_for_output(state: &mut EvaWM, output_index: usize) {
    let Some((target_output, current_seq)) = state
        .outputs
        .get(output_index)
        .map(|o| (o.output.clone(), o.present_seq))
    else {
        return;
    };
    let mut ready = Vec::new();
    let mut i = 0;
    while i < state.pending_screencopies.len() {
        if !state.pending_screencopies[i].frame.is_alive() {
            state.pending_screencopies.swap_remove(i);
        } else if state.pending_screencopies[i].output == target_output {
            ready.push(state.pending_screencopies.swap_remove(i));
        } else {
            i += 1;
        }
    }

    for pending in ready {
        if complete_copy(
            state,
            &pending.frame,
            &pending.buffer,
            &pending.output,
            pending.buffer_size,
            pending.region_loc,
            pending.overlay_cursor,
            true,
        ) && let Ok(mut seqs) = pending.manager_seqs.lock()
        {
            seqs.insert(pending.output.name(), (current_seq, Instant::now()));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn complete_copy(
    state: &mut EvaWM,
    frame: &ZwlrScreencopyFrameV1,
    buffer: &WlBuffer,
    output: &Output,
    buffer_size: Size<i32, Physical>,
    region_loc: Point<i32, Physical>,
    overlay_cursor: bool,
    with_damage: bool,
) -> bool {
    if !frame.is_alive() {
        return false;
    }
    match copy_frame(
        state,
        output,
        buffer_size,
        region_loc,
        overlay_cursor,
        buffer,
    ) {
        Ok(()) => {
            frame.flags(Flags::empty());
            if with_damage {
                frame.damage(0, 0, buffer_size.w as u32, buffer_size.h as u32);
            }
            let timestamp: std::time::Duration = Clock::<Monotonic>::new().now().into();
            let secs = timestamp.as_secs();
            frame.ready((secs >> 32) as u32, secs as u32, timestamp.subsec_nanos());
            true
        }
        Err(err) => {
            tracing::warn!(?err, "screencopy failed");
            frame.failed();
            false
        }
    }
}

fn copy_frame(
    state: &mut EvaWM,
    output: &Output,
    buffer_size: Size<i32, Physical>,
    region_loc: Point<i32, Physical>,
    overlay_cursor: bool,
    buffer: &WlBuffer,
) -> Result<()> {
    ensure!(!state.session_paused, "session is paused");
    ensure!(
        buffer_size.w > 0 && buffer_size.h > 0,
        "invalid screencopy buffer dimensions"
    );
    let output_index = state
        .outputs
        .iter()
        .position(|candidate| candidate.output == *output)
        .context("screencopy output no longer exists")?;

    state
        .with_renderer(|renderer, state| {
            let transform = output.current_transform();
            let scale = Scale::from(output.current_scale().fractional_scale());
            let elements = build_elements_for_output(renderer, state, output_index, overlay_cursor);
            let background = state.config.layout.background.components();

            let size = buffer_size;
            let tex_size = size.to_logical(1).to_buffer(1, Transform::Normal);
            let context = renderer.context_id();
            let key = ScreencopyDamageKey {
                output_index,
                size,
                transform,
                region_loc,
            };
            let cache = &mut state.screencopy_cache;
            if cache.texture.is_none()
                || cache.texture_size != tex_size
                || cache.context.as_ref() != Some(&context)
            {
                let texture: GlesTexture = renderer
                    .create_buffer(Fourcc::Xrgb8888, tex_size)
                    .context("error creating screencopy texture")?;
                cache.texture = Some(texture);
                cache.texture_size = tex_size;
                cache.context = Some(context);
                cache.damage = None;
                cache.damage_key = None;
            }
            let age = if cache.damage.is_some() && cache.damage_key == Some(key) {
                1
            } else {
                cache.damage = Some(OutputDamageTracker::new(size, scale, transform));
                cache.damage_key = Some(key);
                0
            };

            let texture = cache
                .texture
                .as_mut()
                .context("missing screencopy texture")?;
            let damage_tracker = cache
                .damage
                .as_mut()
                .context("missing screencopy damage tracker")?;
            let mut target = renderer
                .bind(texture)
                .context("error binding screencopy texture")?;
            if region_loc == Point::from((0, 0)) {
                damage_tracker
                    .render_output(renderer, &mut target, age, &elements, background)
                    .context("error rendering screencopy frame")?;
            } else {
                let offset = region_loc.upscale(-1);
                let relocated: Vec<_> = elements
                    .into_iter()
                    .map(|element| {
                        RelocateRenderElement::from_element(element, offset, Relocate::Relative)
                    })
                    .collect();
                damage_tracker
                    .render_output(renderer, &mut target, age, &relocated, background)
                    .context("error rendering screencopy frame")?;
            }

            let mapping = renderer
                .copy_framebuffer(
                    &target,
                    Rectangle::from_size(target.size()),
                    Fourcc::Xrgb8888,
                )
                .context("error copying screencopy framebuffer")?;
            let bytes = renderer
                .map_texture(&mapping)
                .context("error mapping screencopy texture")?;

            write_shm(buffer, size, bytes)?;
            Ok(())
        })
        .context("no renderer for screencopy")?
}

fn validate_shm_layout(
    data: &shm::BufferData,
    pool_len: usize,
    size: Size<i32, Physical>,
) -> Result<(usize, usize, usize, usize)> {
    ensure!(
        data.format == Format::Xrgb8888 || data.format == Format::Argb8888,
        "unsupported shm format {:?}",
        data.format
    );
    ensure!(
        size.w > 0 && size.h > 0 && data.width == size.w && data.height == size.h,
        "screencopy buffer size does not match the frame"
    );

    let row_len = size.w as usize * 4;
    let height = size.h as usize;
    let offset = usize::try_from(data.offset).context("negative shm buffer offset")?;
    let stride = usize::try_from(data.stride).context("negative shm buffer stride")?;
    let end = stride
        .checked_mul(height.saturating_sub(1))
        .and_then(|len| len.checked_add(row_len))
        .and_then(|len| len.checked_add(offset));
    ensure!(
        stride >= row_len && end.is_some_and(|end| end <= pool_len),
        "screencopy buffer does not fit its shm pool"
    );
    Ok((offset, stride, row_len, height))
}

fn validate_shm_buffer(buffer: &WlBuffer, size: Size<i32, Physical>) -> Result<()> {
    shm::with_buffer_contents(buffer, |_pool, pool_len, data| {
        validate_shm_layout(&data, pool_len, size).map(|_| ())
    })
    .context("expected an shm screencopy buffer")?
}

fn write_shm(buffer: &WlBuffer, size: Size<i32, Physical>, source: &[u8]) -> Result<()> {
    shm::with_buffer_contents_mut(buffer, |pool, pool_len, data| {
        let (offset, stride, row_len, height) = validate_shm_layout(&data, pool_len, size)?;
        ensure!(source.len() >= row_len * height, "short screencopy mapping");

        // SAFETY: `validate_shm_layout` verified `offset + stride * (height - 1) + row_len <= pool_len`
        // and `stride >= row_len`; `source.len() >= row_len * height`; `source` (GL mapping) and
        // `pool` (client SHM mapping) do not overlap; Smithay installs a SIGBUS guard around
        // `with_buffer_contents_mut` in case the client truncates the pool.
        unsafe {
            let destination = pool.add(offset);
            if stride == row_len {
                std::ptr::copy_nonoverlapping(source.as_ptr(), destination, row_len * height);
            } else {
                for y in 0..height {
                    std::ptr::copy_nonoverlapping(
                        source.as_ptr().add(y * row_len),
                        destination.add(y * stride),
                        row_len,
                    );
                }
            }
        }
        Ok(())
    })
    .context("expected an shm screencopy buffer")?
}
