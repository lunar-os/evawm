//! `zwlr_gamma_control_v1` support.
//!
//! Adapted from driftwm (GPL-3.0-or-later, Copyright (C) 2026 Klim Kostiuk).
//! The protocol sends a packed three-ramp table over a memfd; EvaWM applies it
//! through DRM, restores the identity ramp when the control is destroyed, and
//! reapplies the stored client ramp after VT pause/resume.

use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;

use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use wayland_protocols_wlr::gamma_control::v1::server::{
    zwlr_gamma_control_manager_v1, zwlr_gamma_control_v1,
};
use zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1;
use zwlr_gamma_control_v1::ZwlrGammaControlV1;

use crate::state::EvaWM;

const VERSION: u32 = 1;

pub struct GammaControlManagerState {
    gamma_controls: HashMap<Output, ZwlrGammaControlV1>,
}

pub struct GammaControlState {
    gamma_size: u32,
}

impl GammaControlManagerState {
    pub fn new(display: &DisplayHandle) -> Self {
        display.create_global::<EvaWM, ZwlrGammaControlManagerV1, _>(VERSION, ());
        Self {
            gamma_controls: HashMap::new(),
        }
    }
}

impl GlobalDispatch<ZwlrGammaControlManagerV1, ()> for EvaWM {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        manager: New<ZwlrGammaControlManagerV1>,
        _manager_state: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(manager, ());
    }
}

impl Dispatch<ZwlrGammaControlManagerV1, ()> for EvaWM {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrGammaControlManagerV1,
        request: <ZwlrGammaControlManagerV1 as Resource>::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_gamma_control_manager_v1::Request::GetGammaControl { id, output } => {
                if let Some(output) = Output::from_resource(&output)
                    && !state
                        .gamma_control_manager_state
                        .gamma_controls
                        .contains_key(&output)
                    && let Some(gamma_size) = state.get_gamma_size(&output)
                {
                    let ctrl = data_init.init(id, GammaControlState { gamma_size });
                    ctrl.gamma_size(gamma_size);
                    state
                        .gamma_control_manager_state
                        .gamma_controls
                        .insert(output, ctrl);
                    return;
                }
                data_init
                    .init(id, GammaControlState { gamma_size: 0 })
                    .failed();
            }
            zwlr_gamma_control_manager_v1::Request::Destroy => (),
            _ => (),
        }
    }
}

impl Dispatch<ZwlrGammaControlV1, GammaControlState> for EvaWM {
    #[allow(clippy::mutable_key_type)]
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwlrGammaControlV1,
        request: <ZwlrGammaControlV1 as Resource>::Request,
        data: &GammaControlState,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwlr_gamma_control_v1::Request::SetGamma { fd } => {
                let gamma_controls = &state.gamma_control_manager_state.gamma_controls;
                let Some((output, _)) = gamma_controls.iter().find(|(_, value)| *value == resource)
                else {
                    return;
                };
                let output = output.clone();

                let size = data.gamma_size as usize;
                let expected_len = size * 3 * 2;
                let file = File::from(fd);
                let Ok(meta) = file.metadata() else {
                    tracing::warn!("failed to stat gamma data fd");
                    fail_control(state, resource, &output);
                    return;
                };
                if !meta.is_file() || meta.len() != expected_len as u64 {
                    tracing::warn!(
                        expected = expected_len,
                        actual = meta.len(),
                        is_file = meta.is_file(),
                        "gamma data is not a regular file/memfd of expected size"
                    );
                    fail_control(state, resource, &output);
                    return;
                }
                // Clients may pass a memfd without rewinding its seek offset
                // (the protocol permits mmap from offset 0); read at offset 0.
                let mut bytes = vec![0u8; expected_len];
                if let Err(err) = file.read_exact_at(&mut bytes, 0) {
                    tracing::warn!("failed to read gamma data: {err:?}");
                    fail_control(state, resource, &output);
                    return;
                }
                let mut extra = [0u8; 1];
                if !matches!(file.read_at(&mut extra, expected_len as u64), Ok(0)) {
                    tracing::warn!("gamma data is too large");
                    fail_control(state, resource, &output);
                    return;
                }

                let ramp = bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| u16::from_ne_bytes(*pair))
                    .collect::<Vec<_>>();
                if state.set_gamma(&output, Some(ramp)).is_none() {
                    fail_control(state, resource, &output);
                }
            }
            zwlr_gamma_control_v1::Request::Destroy => (),
            _ => (),
        }
    }

    #[allow(clippy::mutable_key_type)]
    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        resource: &ZwlrGammaControlV1,
        _data: &GammaControlState,
    ) {
        let gamma_controls = &mut state.gamma_control_manager_state.gamma_controls;
        let Some((output, _)) = gamma_controls.iter().find(|(_, value)| *value == resource) else {
            return;
        };
        let output = output.clone();
        gamma_controls.remove(&output);
        let _ = state.set_gamma(&output, None);
    }
}

fn fail_control(state: &mut EvaWM, resource: &ZwlrGammaControlV1, output: &Output) {
    resource.failed();
    state
        .gamma_control_manager_state
        .gamma_controls
        .remove(output);
    let _ = state.set_gamma(output, None);
}

impl EvaWM {
    fn get_gamma_size(&mut self, output: &Output) -> Option<u32> {
        let index = self
            .outputs
            .iter()
            .position(|candidate| candidate.output == *output)?;
        self.session()?.gamma_size(index)
    }

    fn set_gamma(&mut self, output: &Output, ramp: Option<Vec<u16>>) -> Option<()> {
        let index = self
            .outputs
            .iter()
            .position(|candidate| candidate.output == *output)?;
        if self.session_paused {
            return self.session_mut()?.store_gamma_paused(index, ramp);
        }
        self.session_mut()?.set_gamma(index, ramp)
    }
}
