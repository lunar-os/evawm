use std::{os::fd::OwnedFd, path::PathBuf, time::Duration};

use anyhow::{Result, anyhow};
use smithay::desktop::utils::OutputPresentationFeedback;
use smithay::{
    backend::{
        allocator::{
            Fourcc, Modifier,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmNode,
            compositor::{FrameFlags, PrimaryPlaneElement},
            exporter::gbm::{GbmFramebufferExporter, NodeFilter},
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::{EGLContext, EGLDevice, EGLDisplay, context::ContextPriority},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::gles::GlesRenderer,
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{all_gpus, primary_gpu},
    },
    output::{Mode as WlMode, Output, PhysicalProperties},
    reexports::{
        calloop::{
            EventLoop,
            timer::{TimeoutAction, Timer},
        },
        drm::control::{Device as DrmControlDevice, ModeTypeFlags, crtc},
        input::Libinput,
        rustix::fs::OFlags,
        wayland_server::Display,
    },
    utils::{DeviceFd, Transform},
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner, SimpleCrtcMapper};

use crate::{
    config::{Config, OutputConfig, OutputModeConfig},
    render::{OutputRenderElements, build_elements_for_output},
    state::{EvaWM, OutputState, auto_output_positions},
};

type SessionDrmOutput =
    DrmOutput<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>;

type SessionOutputManager = DrmOutputManager<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    (),
    DrmDeviceFd,
>;

struct SessionOutput {
    drm_output: SessionDrmOutput,
    output: Output,
    crtc: crtc::Handle,
    /// A page flip is queued for this output.
    frame_pending: bool,
    /// Feedback for the frame currently being scanned out.
    pending_feedback: Option<OutputPresentationFeedback>,
    /// Last non-identity gamma ramp for this output, reapplied after resume.
    gamma_ramp: Option<Vec<u16>>,
}

pub struct SessionBackend {
    session: LibSeatSession,
    pub renderer: GlesRenderer,
    pub render_node: Option<DrmNode>,
    manager: SessionOutputManager,
    outputs: Vec<SessionOutput>,
}

impl SessionBackend {
    pub fn change_vt(&mut self, vt: i32) -> bool {
        // Reset before the VT change so a warm night-light LUT cannot leak to
        // the VT we are leaving. If the switch fails, put the stored ramp back.
        self.reset_gamma();
        match self.session.change_vt(vt) {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!("failed to change VT to {vt}: {err:?}");
                self.reapply_gamma();
                false
            }
        }
    }

    pub fn reset_buffers(&mut self) {
        for output in &mut self.outputs {
            output.drm_output.reset_buffers();
        }
    }

    /// Re-activate DRM master after an unnotified VT switch and reset the
    /// device, swapchains, compositors, and gamma state so scanout resumes cleanly.
    pub fn reactivate_drm(&mut self) {
        self.reset_drm_state();
        self.reapply_gamma();
    }

    /// Perform a full DRM device reset (disabling all connectors, planes, and
    /// CRTCs) plus per-output compositor and buffer resets. Required when a
    /// foreign master such as `fbcon` left conflicting CRTC/connector bindings.
    fn reset_drm_state(&mut self) {
        {
            let mut locked = self.manager.lock();
            if let Err(err) = locked.activate(false) {
                tracing::warn!("failed to reactivate DRM session: {err:?}");
            }
            if let Err(err) = locked.device_mut().reset_state() {
                tracing::warn!("failed to reset DRM device state: {err:?}");
            }
        }
        for output in &mut self.outputs {
            output.frame_pending = false;
            output.pending_feedback = None;
            output.drm_output.reset_buffers();
            output.drm_output.with_compositor(|compositor| {
                if let Err(err) = compositor.reset_state() {
                    tracing::warn!("failed to reset DRM compositor state: {err:?}");
                }
            });
        }
    }

    /// Clear every CRTC, which puts the outputs into DPMS off. The next
    /// `render_frame` re-enables them.
    pub fn power_off(&mut self) {
        for output in &mut self.outputs {
            output.drm_output.with_compositor(|compositor| {
                if let Err(err) = compositor.clear() {
                    tracing::warn!("failed to clear CRTC for DPMS off: {err:?}");
                }
            });
            output.frame_pending = false;
            output.pending_feedback = None;
        }
    }

    pub fn is_active(&self) -> bool {
        self.session.is_active()
    }

    pub fn gamma_size(&self, index: usize) -> Option<u32> {
        let output = self.outputs.get(index)?;
        let info = self.manager.device().get_crtc(output.crtc).ok()?;
        (info.gamma_length() > 0).then_some(info.gamma_length())
    }

    pub fn set_gamma(&mut self, index: usize, ramp: Option<Vec<u16>>) -> Option<()> {
        let size = self.gamma_size(index)? as usize;
        match ramp {
            Some(ramp) if ramp.len() == size * 3 => {
                self.apply_gamma_sized(index, size, &ramp)?;
                self.outputs[index].gamma_ramp = Some(ramp);
            }
            Some(_) => {
                tracing::warn!("wrong gamma ramp length");
                return None;
            }
            None => {
                let identity = identity_gamma_ramp(size);
                self.apply_gamma_sized(index, size, &identity)?;
                self.outputs[index].gamma_ramp = None;
            }
        }
        Some(())
    }

    /// Record a client-provided gamma ramp while VT-paused so `reapply_gamma`
    /// applies it on resume without tearing down the client's control.
    pub fn store_gamma_paused(&mut self, index: usize, ramp: Option<Vec<u16>>) -> Option<()> {
        let size = self.gamma_size(index)? as usize;
        let store = match ramp {
            Some(ramp) if ramp.len() == size * 3 => Some(ramp),
            Some(_) => {
                tracing::warn!("wrong gamma ramp length");
                return None;
            }
            None => None,
        };
        self.outputs[index].gamma_ramp = store;
        Some(())
    }

    fn apply_gamma(&self, index: usize, ramp: &[u16]) -> Option<()> {
        let size = self.gamma_size(index)? as usize;
        self.apply_gamma_sized(index, size, ramp)
    }

    fn apply_gamma_sized(&self, index: usize, size: usize, ramp: &[u16]) -> Option<()> {
        let output = self.outputs.get(index)?;
        if ramp.len() != size * 3 {
            return None;
        }
        let (red, rest) = ramp.split_at(size);
        let (green, blue) = rest.split_at(size);
        self.manager
            .device()
            .set_gamma(output.crtc, red, green, blue)
            .map_err(|err| tracing::warn!("failed to set gamma on output {index}: {err:?}"))
            .ok()
    }

    /// Reapply the last client-set ramp after a VT switch or resume.
    pub fn reapply_gamma(&self) {
        for (index, output) in self.outputs.iter().enumerate() {
            if let Some(ramp) = output.gamma_ramp.as_deref() {
                let _ = self.apply_gamma(index, ramp);
            }
        }
    }

    /// Reset every CRTC with an active non-identity LUT back to identity
    /// without dropping the stored ramp. Used before VT changes and on exit.
    pub fn reset_gamma(&self) {
        if !self.is_active() {
            return;
        }
        for (index, output) in self.outputs.iter().enumerate() {
            if output.gamma_ramp.is_none() {
                continue;
            }
            let Some(size) = self.gamma_size(index).map(|size| size as usize) else {
                continue;
            };
            let ramp = identity_gamma_ramp(size);
            let _ = self.apply_gamma_sized(index, size, &ramp);
        }
    }

    pub fn reset_buffer_ages_output(&self, index: usize) {
        if let Some(output) = self.outputs.get(index) {
            output
                .drm_output
                .with_compositor(|compositor| compositor.reset_buffer_ages());
        }
    }

    pub fn has_frame_pending(&self, index: usize) -> bool {
        self.outputs
            .get(index)
            .is_some_and(|output| output.frame_pending)
    }

    pub fn output_index_for_crtc(&self, crtc: crtc::Handle) -> Option<usize> {
        self.outputs.iter().position(|output| output.crtc == crtc)
    }

    pub fn frame_submitted(
        &mut self,
        crtc: crtc::Handle,
        metadata: Option<&smithay::backend::drm::DrmEventMetadata>,
    ) {
        if let Some(output) = self.outputs.iter_mut().find(|output| output.crtc == crtc) {
            output.frame_pending = false;
            if let Err(err) = output.drm_output.frame_submitted() {
                tracing::warn!("frame_submitted failed for {crtc:?}: {err:?}");
            }
            if let Some(feedback) = output.pending_feedback.take() {
                crate::render::present_feedback(feedback, &output.output, metadata);
            }
        }
    }

    /// Render one output. Returns `(result, reset_other_outputs)` where
    /// `reset_other_outputs` is `true` whenever a full DRM device reset ran
    /// (even if the retry failed), so the caller reschedules other outputs.
    pub fn render(&mut self, state: &mut EvaWM, index: usize) -> (Result<bool>, bool) {
        match self.render_once(state, index) {
            Ok(presented) => (Ok(presented), false),
            Err(first) => {
                if !self.is_active() {
                    // libseat revoked DRM master mid-frame for a VT switch;
                    // wait for PauseSession/ActivateSession instead of fighting it.
                    state.session_paused = true;
                    return (Ok(false), false);
                }
                tracing::warn!(
                    "DRM frame failed on output {index}, resetting DRM device and retrying: {first:?}"
                );

                self.reset_drm_state();
                for state_output in &mut state.outputs {
                    state_output.needs_redraw = true;
                    state_output.full_redraw = state_output.full_redraw.max(1);
                }
                self.reapply_gamma();

                (self.render_once(state, index), true)
            }
        }
    }

    fn render_once(&mut self, state: &mut EvaWM, index: usize) -> Result<bool> {
        let elements = build_elements_for_output(&mut self.renderer, state, index, true);
        let background = state.config.layout.background.components();
        let output = self
            .outputs
            .get_mut(index)
            .ok_or_else(|| anyhow!("invalid output index {index}"))?;
        let result = output
            .drm_output
            .render_frame(
                &mut self.renderer,
                &elements,
                background,
                // Direct scan-out is deliberately disabled for now.
                FrameFlags::empty(),
            )
            .map_err(|err| anyhow!("{err:?}"))?;

        if result.needs_sync()
            && let PrimaryPlaneElement::Swapchain(element) = result.primary_element
            && let Err(err) = element.sync.wait()
        {
            tracing::warn!("error waiting for DRM frame completion: {err:?}");
        }

        if !result.is_empty {
            output
                .drm_output
                .queue_frame(())
                .map_err(|err| anyhow!("{err:?}"))?;
            output.pending_feedback = Some(crate::render::take_output_presentation_feedback(
                state, index,
            ));
            output.frame_pending = true;
        }
        Ok(!result.is_empty)
    }
}

impl Drop for SessionBackend {
    fn drop(&mut self) {
        // Normal quit and error paths both leave through here. Failures are
        // expected if libseat already paused or revoked the DRM device.
        self.reset_gamma();
    }
}

fn identity_gamma_ramp(size: usize) -> Vec<u16> {
    let mut ramp = vec![0u16; size * 3];
    let (red, rest) = ramp.split_at_mut(size);
    let (green, blue) = rest.split_at_mut(size);
    if size <= 1 {
        return ramp;
    }
    let denom = (size - 1) as u64;
    for (index, ((red, green), blue)) in red.iter_mut().zip(green).zip(blue).enumerate() {
        let value = ((u64::from(u16::MAX) * index as u64) / denom) as u16;
        *red = value;
        *green = value;
        *blue = value;
    }
    ramp
}

/// Delay before the next session-activation retry. Starts short and caps at
/// 2 s so a persistent DRM failure cannot busy-loop or leave input dead.
fn session_activation_retry_delay(retries: u8) -> Duration {
    let shift = u32::from(retries.min(3));
    Duration::from_millis((250u64 << shift).min(2_000))
}

/// Activate DRM after a pause. On failure, keep input suspended and schedule a
/// bounded retry instead of leaving the session permanently dead.
fn try_activate_session(state: &mut EvaWM, libinput: &mut Libinput) -> bool {
    if !state.session_paused {
        return true;
    }

    let activated = if let Some(session) = state.session_mut() {
        // Pass `disable_connectors = true` so Smithay performs a global atomic
        // reset before per-CRTC commits; fbcon or another compositor may have
        // left connectors/planes bound to different CRTCs.
        let activation = session.manager.lock().activate(true);
        match activation {
            Ok(()) => {
                // VT switches hand the CRTCs to fbcon/another session; reset
                // DRM buffers so the next frame re-commits scanout cleanly.
                session.reset_buffers();
                session.reapply_gamma();
                true
            }
            Err(err) => {
                // `DrmDevice::activate(true)` flips its active flag before
                // running `reset_state()`; pause again so retries still reset.
                session.manager.pause();
                tracing::warn!("failed to activate DRM: {err:?}");
                false
            }
        }
    } else {
        true
    };

    if activated {
        if let Err(err) = libinput.resume() {
            tracing::warn!("failed to resume libinput: {err:?}");
        }
        state.session_paused = false;
        for output in &mut state.outputs {
            output.render_retries = 0;
        }
        // The VT switch may have replaced or blanked the front buffer. Reset
        // buffer ages and force one full frame on every output.
        state.mark_full_redraw();
        state.request_redraw_all();
        if state.session_activation_retries > 0 {
            tracing::info!(
                retries = state.session_activation_retries,
                "session activation recovered"
            );
        }
        state.session_activation_retries = 0;
        state.session_activation_retry_armed = false;
        tracing::info!("session resumed");
    } else {
        state.session_activation_retries = state.session_activation_retries.saturating_add(1);
        let retries = state.session_activation_retries;
        if retries == 1 {
            tracing::error!("session activation failed; retrying");
        } else if retries == 8 {
            tracing::error!(
                "session activation still failing after 8 attempts; continuing recovery"
            );
        } else {
            tracing::debug!("session activation failed (attempt {retries})");
        }

        if !state.session_activation_retry_armed {
            let delay = session_activation_retry_delay(retries);
            let generation = state.session_activation_generation;
            let mut context = libinput.clone();
            let handle = state.handle.clone();
            match handle.insert_source(
                Timer::from_duration(delay),
                move |_, _, state: &mut EvaWM| {
                    state.session_activation_retry_armed = false;
                    if state.session_activation_generation == generation && state.session_paused {
                        try_activate_session(state, &mut context);
                    }
                    TimeoutAction::Drop
                },
            ) {
                Ok(_) => state.session_activation_retry_armed = true,
                Err(err) => tracing::warn!("failed to arm session activation retry: {err}"),
            }
        }
    }

    activated
}

pub fn run_session(run: Option<&str>, config: Config, config_path: Option<PathBuf>) -> Result<()> {
    let mut session_env_guard = crate::session_env::SessionEnvGuard::capture();
    let mut event_loop: EventLoop<EvaWM> = EventLoop::try_new()?;
    crate::signals::install(&event_loop)?;
    let display: Display<EvaWM> = Display::new()?;
    let display_handle = display.handle();

    let (mut session, session_notifier) =
        LibSeatSession::new().map_err(|err| anyhow!("libseat session failed: {err:?}"))?;
    let seat_name = session.seat();
    tracing::info!(seat = %seat_name, "libseat session acquired");

    let (gpu_path, fd) = open_drm_device(&mut session)?;
    tracing::info!(path = %gpu_path.display(), "opened DRM device");
    let fd = DrmDeviceFd::new(DeviceFd::from(fd));

    let (drm, drm_notifier) =
        DrmDevice::new(fd.clone(), true).map_err(|err| anyhow!("drm device: {err:?}"))?;
    tracing::debug!("DRM device initialized");
    let gbm = GbmDevice::new(fd).map_err(|err| anyhow!("gbm device: {err:?}"))?;
    tracing::debug!("GBM device initialized");

    // SAFETY: `gbm` is a valid GBM device whose cloned handle is owned by the
    // returned `EGLDisplay` for its entire lifetime.
    let egl_display =
        unsafe { EGLDisplay::new(gbm.clone()) }.map_err(|err| anyhow!("EGL display: {err:?}"))?;
    let egl_context = EGLContext::new_with_priority(&egl_display, ContextPriority::High)
        .map_err(|err| anyhow!("EGL context: {err:?}"))?;
    // SAFETY: `egl_context` was created on this thread and is not current on
    // any other thread while probing GLES capabilities.
    let capabilities = unsafe { GlesRenderer::supported_capabilities(&egl_context) }
        .map_err(|err| anyhow!("GLES capabilities: {err:?}"))?;
    // SAFETY: `egl_context` is exclusively owned by this single-threaded
    // compositor and `capabilities` was queried from the same context above.
    let mut renderer = unsafe { GlesRenderer::with_capabilities(egl_context, capabilities) }
        .map_err(|err| anyhow!("GLES renderer: {err:?}"))?;
    tracing::debug!("EGL/GLES renderer initialized");

    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let exporter = GbmFramebufferExporter::new(gbm.clone(), NodeFilter::None);

    // XRGB8888 is the most widely accepted primary-plane format. Alpha
    // formats can be rejected by some CRTC/plane combinations.
    let color_formats = [Fourcc::Xrgb8888];
    let all_render_formats = renderer
        .egl_context()
        .dmabuf_render_formats()
        .iter()
        .copied()
        .collect::<Vec<_>>();

    // Prefer implicit modifiers (`DRM_FORMAT_MOD_INVALID`) so the driver picks
    // a scanout-compatible layout without tripping modifier validation errors.
    // If the driver exposes no implicit render format, use everything.
    let implicit_formats = all_render_formats
        .iter()
        .filter(|format| format.modifier == Modifier::Invalid)
        .copied()
        .collect::<Vec<_>>();
    let render_formats = if implicit_formats.is_empty() {
        all_render_formats
    } else {
        implicit_formats
    };

    let mut manager = DrmOutputManager::new(
        drm,
        allocator,
        exporter,
        Some(gbm),
        color_formats,
        render_formats,
    );

    let mut scanner: DrmScanner<SimpleCrtcMapper> = DrmScanner::new();
    let scan = scanner
        .scan_connectors(manager.device())
        .map_err(|err| anyhow!("connector scan: {err}"))?;

    let wanted_connector = std::env::var("EVAWM_DRM_CONNECTOR").ok();
    struct Candidate {
        connector: smithay::reexports::drm::control::connector::Info,
        crtc: crtc::Handle,
        name: String,
        config: Option<OutputConfig>,
        drm_mode: smithay::reexports::drm::control::Mode,
        transform: Transform,
        logical_size: smithay::utils::Size<i32, smithay::utils::Logical>,
    }

    let mut candidates = Vec::new();
    for event in scan.iter() {
        let DrmScanEvent::Connected {
            connector,
            crtc: Some(mapped_crtc),
        } = event
        else {
            continue;
        };
        let name = format!(
            "{}-{}",
            connector.interface().as_str(),
            connector.interface_id()
        );
        if let Some(output_config) = config.output_config(&name)
            && output_config.off
        {
            continue;
        }
        if wanted_connector
            .as_deref()
            .is_some_and(|wanted| wanted != name)
        {
            continue;
        }

        let output_config = config.output_config(&name).cloned();
        let Some(drm_mode) = select_output_mode(&connector, output_config.as_ref()) else {
            tracing::warn!(output = %name, "connected connector has no modes; skipping");
            continue;
        };
        let wl_mode = WlMode::from(drm_mode);
        let transform = output_config
            .as_ref()
            .and_then(|output| output.transform)
            .unwrap_or(Transform::Normal);
        let logical_size = transform.transform_size(wl_mode.size.to_logical(1));
        candidates.push(Candidate {
            connector,
            crtc: mapped_crtc,
            name,
            config: output_config,
            drm_mode,
            transform,
            logical_size,
        });
    }

    if candidates.is_empty() {
        return Err(anyhow!(
            "no enabled connected DRM connector (EVAWM_DRM_CONNECTOR={:?})",
            wanted_connector
        ));
    }

    // Keep connector ordering stable across boots.
    candidates.sort_by(|a, b| a.name.cmp(&b.name));
    let sizes: Vec<_> = candidates
        .iter()
        .map(|candidate| candidate.logical_size)
        .collect();
    let configured_positions: Vec<_> = candidates
        .iter()
        .map(|candidate| candidate.config.as_ref().and_then(|config| config.position))
        .collect();
    let positions = auto_output_positions(&sizes, &configured_positions);

    let mut session_outputs = Vec::new();
    let mut output_states = Vec::new();
    for (candidate, position) in candidates.into_iter().zip(positions) {
        let wl_mode = WlMode::from(candidate.drm_mode);
        let physical_size = candidate
            .connector
            .size()
            .map(|(w, h)| (w as i32, h as i32))
            .unwrap_or((0, 0));
        let output = Output::new(
            candidate.name.clone(),
            PhysicalProperties {
                size: physical_size.into(),
                subpixel: candidate.connector.subpixel().into(),
                make: "EvaWM".into(),
                model: "DRM".into(),
                serial_number: "unknown".into(),
            },
        );
        output.set_preferred(wl_mode);
        output.change_current_state(
            Some(wl_mode),
            Some(candidate.transform),
            None,
            Some(position),
        );

        let drm_output = match manager
            .lock()
            .initialize_output::<GlesRenderer, OutputRenderElements>(
                candidate.crtc,
                candidate.drm_mode,
                &[candidate.connector.handle()],
                &output,
                None,
                &mut renderer,
                &DrmOutputRenderElements::<GlesRenderer, OutputRenderElements>::default(),
            ) {
            Ok(drm_output) => drm_output,
            Err(err) => {
                tracing::warn!(
                    output = %candidate.name,
                    ?err,
                    "failed to initialize output; skipping"
                );
                continue;
            }
        };

        let _ = output.create_global::<EvaWM>(&display_handle);
        let swapchain_format = drm_output.format();
        tracing::info!(
            output = %candidate.name,
            crtc = ?candidate.crtc,
            width = wl_mode.size.w,
            height = wl_mode.size.h,
            refresh_mhz = wl_mode.refresh,
            transform = ?candidate.transform,
            x = position.x,
            y = position.y,
            format = ?swapchain_format,
            "DRM output initialized"
        );

        output_states.push(OutputState::new(output.clone(), &config));
        session_outputs.push(SessionOutput {
            drm_output,
            output: output.clone(),
            crtc: candidate.crtc,
            frame_pending: false,
            pending_feedback: None,
            gamma_ramp: None,
        });
    }

    if session_outputs.is_empty() {
        return Err(anyhow!("no DRM output could be initialized"));
    }

    let render_node = EGLDevice::device_for_display(renderer.egl_context().display())
        .and_then(|device| device.try_get_render_node())
        .ok()
        .flatten();
    let backend = SessionBackend {
        session: session.clone(),
        renderer,
        render_node,
        manager,
        outputs: session_outputs,
    };

    let mut state = EvaWM::new(
        display,
        &mut event_loop,
        crate::state::Backend::Session(Box::new(backend)),
        output_states,
        false,
        config,
        config_path,
    );
    state.init_dmabuf();
    state.start_xwayland();
    let x11_display = state.x11_display().map(str::to_owned);
    let evawm_socket = state
        .ipc_server
        .as_ref()
        .map(|server| server.path().to_string_lossy().into_owned());
    let session_env_start = std::time::Instant::now();
    session_env_guard.apply(
        &state.socket_name.to_string_lossy(),
        "evawm",
        "wayland",
        x11_display.as_deref(),
        evawm_socket.as_deref(),
    );
    tracing::info!(
        elapsed_ms = session_env_start.elapsed().as_millis() as u64,
        "updated session environment and queued portal switch"
    );
    state.run_startup();

    state.request_redraw_all();
    tracing::info!(
        wayland_display = %state.socket_name.to_string_lossy(),
        x11_display = x11_display.as_deref().unwrap_or("disabled"),
        "entering event loop"
    );

    // DRM vblank / errors.
    event_loop
        .handle()
        .insert_source(
            drm_notifier,
            |event, metadata, state: &mut EvaWM| match event {
                DrmEvent::VBlank(crtc) => {
                    let index = state.session_mut().and_then(|session| {
                        session.frame_submitted(crtc, metadata.as_ref());
                        session.output_index_for_crtc(crtc)
                    });
                    // Render only the output that just completed a frame. An idle
                    // output has no frame queued, so it stays completely silent.
                    if let Some(index) = index
                        && state
                            .outputs
                            .get(index)
                            .is_some_and(|output| output.needs_redraw)
                    {
                        state.request_redraw_output(index);
                    }
                }
                DrmEvent::Error(err) => tracing::error!("DRM error: {err:?}"),
            },
        )
        .map_err(|err| anyhow!("failed to insert DRM source: {err}"))?;

    // Real input.
    let mut libinput_context =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());
    libinput_context
        .udev_assign_seat(&seat_name)
        .map_err(|_| anyhow!("failed to assign libinput seat"))?;
    let libinput_backend = LibinputInputBackend::new(libinput_context.clone());

    event_loop
        .handle()
        .insert_source(libinput_backend, |event, _, state: &mut EvaWM| {
            match &event {
                smithay::backend::input::InputEvent::DeviceAdded { device } => {
                    state.add_libinput_device(device.clone());
                }
                smithay::backend::input::InputEvent::DeviceRemoved { device } => {
                    state.remove_libinput_device(device);
                }
                _ => {}
            }
            state.process_input_event(event);
        })
        .map_err(|err| anyhow!("failed to insert libinput source: {err}"))?;

    // Seat pause/resume.
    event_loop
        .handle()
        .insert_source(
            session_notifier,
            move |event, _, state: &mut EvaWM| match event {
                SessionEvent::PauseSession => {
                    state.cancel_interaction_state();
                    state.session_paused = true;
                    state.session_activation_generation =
                        state.session_activation_generation.wrapping_add(1);
                    state.session_activation_retries = 0;
                    state.session_activation_retry_armed = false;
                    for output in &mut state.outputs {
                        output.needs_redraw = false;
                        output.render_scheduled = false;
                        output.render_retry_armed = false;
                    }
                    if let Some(session) = state.session_mut() {
                        for output in &mut session.outputs {
                            output.frame_pending = false;
                            output.pending_feedback = None;
                        }
                        // libseat revokes DRM master before delivering PauseSession;
                        // `change_vt` already reset any active gamma LUT beforehand.
                        session.manager.pause();
                    }
                    libinput_context.suspend();
                    tracing::info!("session paused");
                }
                SessionEvent::ActivateSession => {
                    try_activate_session(state, &mut libinput_context);
                }
            },
        )
        .map_err(|err| anyhow!("failed to insert session source: {err}"))?;

    if let Some(run_command) = run {
        state.spawn_shell_command(run_command)?;
    }

    event_loop.run(None, &mut state, |state| {
        let _ = state.display_handle.flush_clients();
    })?;
    // Drop compositor state (DRM/gamma, IPC/Wayland sockets, XWayland) before
    // `session_env_guard` restores systemd/D-Bus environment and portals.
    drop(state);
    drop(session_env_guard);
    Ok(())
}

fn select_output_mode(
    connector: &smithay::reexports::drm::control::connector::Info,
    config: Option<&OutputConfig>,
) -> Option<smithay::reexports::drm::control::Mode> {
    choose_output_mode(
        connector.modes(),
        config.and_then(|output| output.mode.as_ref()),
        &format!(
            "{}-{}",
            connector.interface().as_str(),
            connector.interface_id()
        ),
    )
}

fn choose_output_mode(
    modes: &[smithay::reexports::drm::control::Mode],
    wanted: Option<&OutputModeConfig>,
    output_name: &str,
) -> Option<smithay::reexports::drm::control::Mode> {
    let modes: Vec<_> = modes
        .iter()
        .copied()
        .filter(|mode| {
            let (_, _, htotal) = mode.hsync();
            let (_, _, vtotal) = mode.vsync();
            mode.vrefresh() > 0 && htotal > 0 && vtotal > 0
        })
        .collect();
    if modes.is_empty() {
        return None;
    }

    if let Some(wanted) = wanted {
        let candidates: Vec<_> = modes
            .iter()
            .copied()
            .filter(|mode| {
                let (width, height) = mode.size();
                i32::from(width) == wanted.width && i32::from(height) == wanted.height
            })
            .collect();
        let selected = match wanted.refresh {
            Some(refresh) => candidates.iter().min_by_key(|mode| {
                let mode_refresh = WlMode::from(**mode).refresh;
                (
                    mode_refresh.abs_diff(refresh),
                    std::cmp::Reverse(mode_refresh),
                )
            }),
            None => candidates
                .iter()
                .max_by_key(|mode| WlMode::from(**mode).refresh),
        };
        if let Some(mode) = selected {
            return Some(*mode);
        }
        tracing::warn!(
            ?wanted,
            output = %output_name,
            "configured output mode not found; falling back to preferred mode"
        );
    }

    // Prefer the highest refresh mode at the panel's preferred resolution.
    // Many EDIDs mark 60 Hz as "preferred" even when 120/144/165 Hz modes are
    // available, which makes compositor animations feel much worse.
    let preferred = modes
        .iter()
        .find(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
        .copied();
    let preferred_size = preferred.map(|mode| mode.size());
    modes
        .iter()
        .filter(|mode| Some(mode.size()) == preferred_size)
        .max_by_key(|mode| WlMode::from(**mode).refresh)
        .or(preferred.as_ref())
        .copied()
        .or_else(|| modes.first().copied())
}

fn open_drm_device(session: &mut LibSeatSession) -> Result<(PathBuf, OwnedFd)> {
    let mut candidates = Vec::new();

    if let Ok(path) = std::env::var("EVAWM_DRM_DEVICE") {
        candidates.push(PathBuf::from(path));
    }

    if let Ok(Some(path)) = primary_gpu(session.seat()) {
        candidates.push(path);
    }

    if let Ok(paths) = all_gpus(session.seat()) {
        candidates.extend(paths);
    }

    // Keep order, drop duplicates.
    let mut seen = std::collections::HashSet::new();
    candidates.retain(|path| seen.insert(path.clone()));

    if candidates.is_empty() {
        return Err(anyhow!("no DRM devices found for this seat"));
    }

    let mut errors = Vec::new();
    for path in candidates {
        tracing::info!(path = %path.display(), "trying DRM device");
        match session.open(
            &path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        ) {
            Ok(fd) => return Ok((path, fd)),
            Err(err) => {
                tracing::warn!(path = %path.display(), ?err, "failed to open DRM device");
                errors.push(format!("{}: {err:?}", path.display()));
            }
        }
    }

    Err(anyhow!(
        "failed to open any DRM device; tried:\n{}",
        errors.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{identity_gamma_ramp, session_activation_retry_delay};

    #[test]
    fn session_activation_retry_delay_backs_off_and_caps() {
        assert_eq!(
            session_activation_retry_delay(0),
            Duration::from_millis(250)
        );
        assert_eq!(
            session_activation_retry_delay(1),
            Duration::from_millis(500)
        );
        assert_eq!(
            session_activation_retry_delay(2),
            Duration::from_millis(1_000)
        );
        assert_eq!(
            session_activation_retry_delay(3),
            Duration::from_millis(2_000)
        );
        assert_eq!(
            session_activation_retry_delay(u8::MAX),
            Duration::from_millis(2_000)
        );
    }

    #[test]
    fn empty_output_mode_list_is_rejected() {
        assert!(super::choose_output_mode(&[], None, "DP-1").is_none());
    }

    #[test]
    fn identity_gamma_ramp_is_monotonic_across_channels() {
        let size = 4;
        let ramp = identity_gamma_ramp(size);
        assert_eq!(ramp.len(), size * 3);
        for channel in ramp.chunks_exact(size) {
            assert_eq!(channel, &[0, 21_845, 43_690, 65_535]);
        }
    }

    #[test]
    fn tiny_identity_gamma_ramps_are_zeroed() {
        assert!(identity_gamma_ramp(0).is_empty());
        assert_eq!(identity_gamma_ramp(1), vec![0; 3]);
    }
}
