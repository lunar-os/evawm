use std::time::Duration;

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    input::{
        keyboard::{FilterResult, Keysym},
        pointer::{
            AxisFrame, ButtonEvent, CursorIcon, CursorImageStatus, MotionEvent, RelativeMotionEvent,
        },
    },
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER},
    wayland::{
        keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitorSeat,
        pointer_constraints::{PointerConstraint, with_pointer_constraint},
        seat::WaylandFocus,
    },
};

use crate::{
    config::{
        AccelProfile as ConfigAccelProfile, InputConfig, Modifiers, MouseButton, WheelDirection,
    },
    drag::Drag,
    layout::resize_edges,
    state::EvaWM,
};

type LibinputDevice = smithay::reexports::input::Device;

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

fn is_mod_keysym(keysym: Keysym, nested_mod_alt: bool) -> bool {
    if nested_mod_alt {
        matches!(keysym, Keysym::Alt_L | Keysym::Alt_R)
    } else {
        matches!(keysym, Keysym::Super_L | Keysym::Super_R)
    }
}

/// Match both real Linux evdev codes (`BTN_SIDE` / `BTN_EXTRA`) and the codes
/// winit emits in nested sessions (`BTN_BACK` / `BTN_FORWARD`).
fn mouse_button_matches(button: MouseButton, code: u32) -> bool {
    match button {
        MouseButton::None => false,
        MouseButton::Back => matches!(code, 0x113 | 0x116),
        MouseButton::Forward => matches!(code, 0x114 | 0x115),
        MouseButton::Middle => code == 0x112,
    }
}

/// Input that should power monitors back on.
///
/// Key releases are not activity: the release of the binding that turned the
/// monitors off must not immediately turn them back on. Device additions and
/// removals are also ignored.
fn should_wake_monitors<I: InputBackend>(event: &InputEvent<I>) -> bool {
    match event {
        InputEvent::Keyboard { event } if event.state() == KeyState::Pressed => true,
        InputEvent::PointerButton { event } if event.state() == ButtonState::Pressed => true,
        InputEvent::PointerMotion { .. }
        | InputEvent::PointerMotionAbsolute { .. }
        | InputEvent::PointerAxis { .. }
        | InputEvent::GestureSwipeBegin { .. }
        | InputEvent::GesturePinchBegin { .. }
        | InputEvent::GestureHoldBegin { .. }
        | InputEvent::TouchDown { .. }
        | InputEvent::TouchMotion { .. }
        | InputEvent::TabletToolAxis { .. }
        | InputEvent::TabletToolProximity { .. }
        | InputEvent::TabletToolTip { .. }
        | InputEvent::TabletToolButton { .. } => true,
        _ => false,
    }
}

/// Device hotplug is not user activity; it must not reset idle timers.
fn should_notify_activity<I: InputBackend>(event: &InputEvent<I>) -> bool {
    !matches!(
        event,
        InputEvent::DeviceAdded { .. } | InputEvent::DeviceRemoved { .. }
    )
}

fn vt_from_keysym(keysym: Keysym) -> Option<i32> {
    match keysym {
        Keysym::F1 => Some(1),
        Keysym::F2 => Some(2),
        Keysym::F3 => Some(3),
        Keysym::F4 => Some(4),
        Keysym::F5 => Some(5),
        Keysym::F6 => Some(6),
        Keysym::F7 => Some(7),
        Keysym::F8 => Some(8),
        Keysym::F9 => Some(9),
        Keysym::F10 => Some(10),
        Keysym::F11 => Some(11),
        Keysym::F12 => Some(12),
        other => {
            // XF86Switch_VT_1 .. XF86Switch_VT_12
            let raw = other.raw();
            if (0x1008_FE01..=0x1008_FE0C).contains(&raw) {
                Some((raw - 0x1008_FE00) as i32)
            } else {
                None
            }
        }
    }
}

fn libinput_accel_profile(profile: ConfigAccelProfile) -> smithay::reexports::input::AccelProfile {
    match profile {
        ConfigAccelProfile::Adaptive => smithay::reexports::input::AccelProfile::Adaptive,
        ConfigAccelProfile::Flat => smithay::reexports::input::AccelProfile::Flat,
    }
}

fn apply_accel_profile(device: &mut LibinputDevice, profile: Option<ConfigAccelProfile>) {
    if let Some(profile) = profile {
        let _ = device.config_accel_set_profile(libinput_accel_profile(profile));
    } else if let Some(default) = device.config_accel_default_profile() {
        let _ = device.config_accel_set_profile(default);
    }
}

/// Apply EvaWM's mouse/touchpad policy to one libinput device.
pub(crate) fn apply_libinput_settings(config: &InputConfig, device: &mut LibinputDevice) {
    // Mutter/niri use the tap finger count to distinguish touchpads.
    if device.config_tap_finger_count() > 0 {
        let touchpad = &config.touchpad;
        let _ = device.config_tap_set_enabled(touchpad.tap);
        let _ = device.config_scroll_set_natural_scroll_enabled(touchpad.natural_scroll);
        let _ = device.config_left_handed_set(touchpad.left_handed);
        let _ = device.config_accel_set_speed(touchpad.accel_speed);
        apply_accel_profile(device, touchpad.accel_profile);
        tracing::debug!(
            device = %device.name(),
            tap = touchpad.tap,
            natural_scroll = touchpad.natural_scroll,
            speed = touchpad.accel_speed,
            profile = ?touchpad.accel_profile,
            "touchpad settings applied"
        );
        return;
    }

    if !device.has_capability(smithay::reexports::input::DeviceCapability::Pointer) {
        return;
    }

    let mouse = &config.mouse;
    let _ = device.config_scroll_set_natural_scroll_enabled(mouse.natural_scroll);
    let _ = device.config_left_handed_set(mouse.left_handed);
    let _ = device.config_middle_emulation_set_enabled(mouse.middle_emulation);
    let _ = device.config_accel_set_speed(mouse.accel_speed);
    apply_accel_profile(device, mouse.accel_profile);
    tracing::debug!(
        device = %device.name(),
        natural_scroll = mouse.natural_scroll,
        speed = mouse.accel_speed,
        profile = ?mouse.accel_profile,
        "mouse settings applied"
    );
}

fn same_pointer_motion(
    last: Option<&crate::state::PointerMotionSnapshot>,
    next: &crate::state::PointerMotionSnapshot,
) -> bool {
    last.is_some_and(|last| {
        last.surface == next.surface && last.origin == next.origin && last.location == next.location
    })
}

/// Resolve a confined-pointer move from `current` toward `candidate`.
///
/// If the diagonal step leaves the confined area, fall back to sliding along X
/// or Y individually so hitting a window or region edge does not freeze
/// motion parallel to that edge.
fn confined_target(
    current: Point<f64, Logical>,
    candidate: Point<f64, Logical>,
    mut allowed: impl FnMut(Point<f64, Logical>) -> bool,
) -> Option<Point<f64, Logical>> {
    if allowed(candidate) {
        return Some(candidate);
    }
    let slide_x = Point::from((candidate.x, current.y));
    if slide_x.x != current.x && allowed(slide_x) {
        return Some(slide_x);
    }
    let slide_y = Point::from((current.x, candidate.y));
    if slide_y.y != current.y && allowed(slide_y) {
        return Some(slide_y);
    }
    None
}

impl EvaWM {
    pub(crate) fn add_libinput_device(&mut self, mut device: LibinputDevice) {
        tracing::debug!(device = %device.name(), "libinput device added");
        apply_libinput_settings(&self.config.input, &mut device);
        self.libinput_devices.insert(device);
    }

    pub(crate) fn remove_libinput_device(&mut self, device: &LibinputDevice) {
        tracing::debug!(device = %device.name(), "libinput device removed");
        let removed_pointer =
            device.has_capability(smithay::reexports::input::DeviceCapability::Pointer);
        let removed_keyboard =
            device.has_capability(smithay::reexports::input::DeviceCapability::Keyboard);
        self.libinput_devices.remove(device);

        let pointer_remains = self.libinput_devices.iter().any(|device| {
            device.has_capability(smithay::reexports::input::DeviceCapability::Pointer)
        });
        let keyboard_remains = self.libinput_devices.iter().any(|device| {
            device.has_capability(smithay::reexports::input::DeviceCapability::Keyboard)
        });

        if removed_pointer && !pointer_remains {
            self.cancel_interaction_state();
        } else if removed_keyboard && !keyboard_remains {
            self.stop_bind_repeat();
        }
    }

    pub(crate) fn reapply_libinput_settings(&mut self) {
        let input = &self.config.input;
        for mut device in self.libinput_devices.iter().cloned() {
            apply_libinput_settings(input, &mut device);
        }
    }

    fn stop_bind_repeat(&mut self) {
        if let Some(token) = self.bind_repeat_token.take() {
            self.handle.remove(token);
        }
        self.bind_repeat_action = None;
    }

    /// Drop transient pointer/keyboard grab state at a session or layout
    /// boundary. Safe to call when nothing is active.
    pub(crate) fn cancel_interaction_state(&mut self) {
        if self.dnd_active {
            let serial = SERIAL_COUNTER.next_serial();
            let time = self.start_time.elapsed().as_millis() as u32;
            let pointer = self.pointer.clone();
            pointer.unset_grab(self, serial, time);
        }
        self.dnd_active = false;
        self.dnd_icon = None;
        self.cancel_drag();
        self.pan_held = false;
        self.extra_mod_held = false;
        self.mru_cycle = None;
        self.mru_cycle_step = false;
        self.stop_bind_repeat();
        if let Some(status) = self.drag_cursor.take() {
            self.set_cursor_image(status);
        }
        self.request_redraw();
    }

    fn start_bind_repeat(&mut self, action: crate::config::Action) {
        self.stop_bind_repeat();
        let delay = self.config.input.repeat_delay;
        let rate = self.config.input.repeat_rate;
        if rate == 0 || !action.is_repeatable() {
            return;
        }

        let rate = Duration::from_secs_f64(1.0 / f64::from(rate));
        let timer = smithay::reexports::calloop::timer::Timer::from_duration(
            Duration::from_millis(u64::from(delay)),
        );
        let inserted = self
            .handle
            .clone()
            .insert_source(timer, move |_, _, state: &mut EvaWM| {
                let Some(action) = state.bind_repeat_action.clone() else {
                    return smithay::reexports::calloop::timer::TimeoutAction::Drop;
                };
                state.execute_action(action);
                smithay::reexports::calloop::timer::TimeoutAction::ToDuration(rate)
            });
        match inserted {
            Ok(token) => {
                self.bind_repeat_action = Some(action);
                self.bind_repeat_token = Some(token);
            }
            Err(err) => tracing::warn!("failed to start bind repeat: {err}"),
        }
    }

    fn is_pan_button(&self, button: u32) -> bool {
        mouse_button_matches(self.config.canvas.pan_button, button)
    }

    fn is_mod_button(&self, button: u32) -> bool {
        mouse_button_matches(self.config.canvas.mod_button, button)
    }

    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        // A VT switch to the current VT may not deliver Pause/Activate. If
        // libseat still reports the session active, recover on the next input.
        if self.session_paused && self.session().is_some_and(|session| session.is_active()) {
            self.session_paused = false;
            if let Some(session) = self.session_mut() {
                session.reactivate_drm();
            }
            self.mark_full_redraw();
        }
        if self.monitors_off && should_wake_monitors(&event) {
            self.wake_monitors();
        }
        if should_notify_activity(&event) {
            self.note_idle_activity();
        }
        if self
            .popup_grab
            .as_ref()
            .is_some_and(|grab| grab.has_ended())
        {
            self.popup_grab = None;
        }
        match event {
            InputEvent::Keyboard { event, .. } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);
                let pressed = event.state() == KeyState::Pressed;
                let keyboard = self.keyboard.clone();
                keyboard.input::<(), _>(
                    self,
                    event.key_code(),
                    event.state(),
                    serial,
                    time,
                    |state, mods, keysym| {
                        if !pressed {
                            state.stop_bind_repeat();
                        }
                        if pressed
                            && mods.ctrl
                            && mods.alt
                            && let Some(vt) = vt_from_keysym(keysym.modified_sym())
                        {
                            state.change_vt(vt);
                            return FilterResult::Intercept(());
                        }
                        if !state.overview_open && state.keyboard_shortcuts_inhibited() {
                            return FilterResult::Forward;
                        }

                        // In nested test mode Alt stands in for Super, because
                        // the host compositor owns Super.
                        let logo = if state.nested_mod_alt {
                            mods.alt
                        } else {
                            mods.logo
                        };
                        let alt = if state.nested_mod_alt {
                            false
                        } else {
                            mods.alt
                        };
                        let modifiers = Modifiers {
                            logo,
                            ctrl: mods.ctrl,
                            alt,
                            shift: mods.shift,
                        };

                        let modified = keysym.modified_sym();
                        let raw = keysym.raw_latin_sym_or_raw_current_sym();
                        if !pressed
                            && (is_mod_keysym(modified, state.nested_mod_alt)
                                || raw.is_some_and(|sym| is_mod_keysym(sym, state.nested_mod_alt)))
                        {
                            state.commit_mru_cycle();
                        }
                        if let Some(action) =
                            state.config.find_bind(modifiers, modified, raw).cloned()
                        {
                            if pressed {
                                state.execute_action(action.clone());
                                state.start_bind_repeat(action);
                            }
                            FilterResult::Intercept(())
                        } else if state.overview_open && !state.keyboard_layer_focused() {
                            if pressed
                                && modifiers == Modifiers::default()
                                && matches!(modified, Keysym::Escape | Keysym::Return)
                            {
                                state.close_overview();
                            }
                            FilterResult::Intercept(())
                        } else {
                            FilterResult::Forward
                        }
                    },
                );
            }

            InputEvent::PointerMotionAbsolute { event, .. } => {
                let Some(rect) = self.outputs[self.pointer_output].global_rect() else {
                    return;
                };
                let size = smithay::utils::Size::from((
                    rect.size.w.round() as i32,
                    rect.size.h.round() as i32,
                ));
                let local = event.position_transformed(size);
                let global = local + rect.loc;
                let previous = self.last_absolute_pointer.unwrap_or(self.pointer_global);
                self.last_absolute_pointer = Some(global);
                let delta = global - previous;
                self.pointer_moved(global, event.time_msec(), Some((delta, delta)));
            }

            // libinput (real sessions) reports relative motion; nested winit
            // reports absolute motion. Both feed the same path.
            InputEvent::PointerMotion { event, .. } => {
                self.last_absolute_pointer = None;
                let global = Point::from((
                    self.pointer_global.x + event.delta_x(),
                    self.pointer_global.y + event.delta_y(),
                ));
                self.pointer_moved(
                    global,
                    event.time_msec(),
                    Some((event.delta(), event.delta_unaccel())),
                );
            }

            InputEvent::PointerButton { event, .. } => {
                let serial = SERIAL_COUNTER.next_serial();
                let button = event.button_code();
                let pressed = event.state() == ButtonState::Pressed;

                if self.is_pan_button(button) {
                    self.pan_held = pressed;
                    return;
                }

                if self.is_mod_button(button) {
                    self.extra_mod_held = pressed;
                    return;
                }

                self.activate_pointer_output();

                if pressed && self.mru_cycle.is_some() {
                    self.commit_mru_cycle();
                }

                let mods = self.keyboard.modifier_state();
                let mod_key = if self.nested_mod_alt {
                    mods.alt
                } else {
                    mods.logo
                };
                let mod_active = mod_key || self.extra_mod_held;
                let mod_shift_pan = mod_key && mods.shift;

                let world = self
                    .output_state()
                    .overview_screen_to_world(self.pointer_pos);
                tracing::trace!(
                    pointer = ?self.pointer_pos,
                    ?world,
                    button,
                    pressed,
                    under = self.space().element_under(world).is_some(),
                    "pointer button"
                );

                let fullscreen = self.active_fullscreen().is_some();
                if pressed
                    && button == BTN_LEFT
                    && (self.pan_held || mod_shift_pan)
                    && self.active_mode() == crate::workspace::Mode::Canvas
                    && !fullscreen
                    && self.layer_above_windows(self.pointer_pos).is_none()
                {
                    let (x, y) = {
                        let output = self.output_state();
                        (output.view_x.value, output.view_y.value)
                    };
                    self.set_drag_cursor(CursorIcon::Grabbing);
                    self.drag = Some(Drag::Pan {
                        button,
                        start: self.pointer_pos,
                        start_view: Point::from((x, y)),
                    });
                    self.clear_pointer_focus(event.time_msec());
                    self.request_redraw();
                    return;
                }
                if pressed
                    && mod_active
                    && self.drag.is_none()
                    && !fullscreen
                    && self.layer_above_windows(self.pointer_pos).is_none()
                    && (button == BTN_LEFT || button == BTN_RIGHT)
                {
                    if let Some(window) = self.window_under(world) {
                        self.focus_window(Some(&window), serial);

                        // Use the current camera when computing drag offsets;
                        // hover focus may have scrolled the view already.
                        let world = self
                            .output_state()
                            .overview_screen_to_world(self.pointer_pos);
                        if button == BTN_LEFT {
                            self.begin_window_move(&window, button);
                        } else if self.active_mode() == crate::workspace::Mode::Canvas {
                            if self.window_is_fixed_size(&window) {
                                return;
                            }
                            let Some(rect) = self.workspaces()[self.active_workspace()]
                                .canvas
                                .rect(&window)
                            else {
                                return;
                            };
                            let edges = resize_edges(world - rect.loc, rect.size.w, rect.size.h);
                            if edges.is_empty() {
                                self.request_redraw();
                                return;
                            }
                            self.set_drag_cursor(edges.cursor_icon());
                            self.begin_mouse_resize(&window);
                            self.window_states.begin_canvas_resize(&window);
                            let output = self.output_state_mut();
                            let x = output.view_x.value;
                            output.view_x.snap(x);
                            self.drag = Some(Drag::CanvasResize {
                                window: window.clone(),
                                source_output: self.active_output,
                                source_workspace: self.active_workspace(),
                                button,
                                start: world,
                                initial: rect,
                                edges,
                            });
                            self.clear_pointer_focus(event.time_msec());
                        } else if let Some((index, tile)) = self.active_layout().tile_index(&window)
                        {
                            if self.window_is_fixed_size(&window) {
                                return;
                            }
                            let Some(tile_rect) = self.active_layout().tile_rect(&window) else {
                                return;
                            };
                            let edges = resize_edges(
                                world - tile_rect.loc,
                                tile_rect.size.w,
                                tile_rect.size.h,
                            );
                            if edges.is_empty() {
                                // Middle third on both axes: niri does not
                                // start an interactive resize.
                                self.request_redraw();
                                return;
                            }
                            self.set_drag_cursor(edges.cursor_icon());
                            self.begin_mouse_resize(&window);
                            // Niri stops view animations when a resize starts.
                            let output = self.output_state_mut();
                            let x = output.view_x.value;
                            output.view_x.snap(x);
                            // Screen space: the grabbed edge must follow the
                            // cursor even if the camera moves during resize.
                            let initial_previous_height = if edges.top && tile > 0 {
                                self.active_layout()
                                    .columns
                                    .get(index)
                                    .and_then(|column| column.tiles.get(tile - 1))
                                    .map(|previous| previous.height)
                            } else {
                                None
                            };
                            self.drag = Some(Drag::ColumnResize {
                                window: window.clone(),
                                source_output: self.active_output,
                                source_workspace: self.active_workspace(),
                                index,
                                tile,
                                button,
                                start: self.pointer_pos,
                                initial_width: tile_rect.size.w,
                                initial_height: tile_rect.size.h,
                                initial_previous_height,
                                edges,
                            });
                            self.clear_pointer_focus(event.time_msec());
                        }

                        self.request_redraw();
                    }
                    return;
                }

                if !pressed && self.drag.as_ref().is_some_and(|d| d.button() == button) {
                    self.end_drag();
                    return;
                }
                if self.drag.is_some() {
                    return;
                }

                if self.overview_open && self.layer_above_windows(self.pointer_pos).is_none() {
                    if pressed && button == BTN_LEFT {
                        self.overview_click_at(serial);
                    }
                    return;
                }

                if pressed {
                    if self.layer_above_windows(self.pointer_pos).is_some() {
                        // Layer-shell clients (bars, launchers) above windows
                        // handle their own clicks; keyboard-interactive
                        // surfaces take focus.
                        self.focus_layer_above_windows_at(self.pointer_pos);
                    } else if let Some(window) = self.window_under(world) {
                        self.focus_window(Some(&window), serial);
                    } else if !self.focus_layer_below_windows_at(self.pointer_pos) {
                        self.focus_window(None, serial);
                    }
                }

                // Always recompute pointer focus right before dispatching a
                // button or axis event. The camera can move between motion
                // events (keyboard focus, layer-shell changes), and a stale
                // focus tuple sends wrong surface-local coordinates.
                self.sync_pointer_focus(event.time_msec());

                let pointer = self.pointer.clone();
                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state: event.state(),
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }

            InputEvent::PointerAxis { event, .. } => {
                if self.drag.is_some() {
                    return;
                }
                self.activate_pointer_output();
                let source = event.source();
                let horizontal = event.amount(Axis::Horizontal).unwrap_or_else(|| {
                    event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.
                });
                let vertical = event.amount(Axis::Vertical).unwrap_or_else(|| {
                    event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.
                });

                if self.overview_open || !self.keyboard_shortcuts_inhibited() {
                    let mods = self.keyboard.modifier_state();
                    let mod_key = if self.nested_mod_alt {
                        mods.alt
                    } else {
                        mods.logo
                    };
                    let modifiers = Modifiers {
                        logo: mod_key || self.extra_mod_held,
                        ctrl: mods.ctrl,
                        alt: if self.nested_mod_alt { false } else { mods.alt },
                        shift: mods.shift,
                    };
                    let wheel_dir = if vertical > 0.0 {
                        Some(WheelDirection::Down)
                    } else if vertical < 0.0 {
                        Some(WheelDirection::Up)
                    } else if horizontal > 0.0 {
                        Some(WheelDirection::Right)
                    } else if horizontal < 0.0 {
                        Some(WheelDirection::Left)
                    } else {
                        None
                    };
                    if let Some(direction) = wheel_dir
                        && let Some(action) =
                            self.config.find_wheel_bind(modifiers, direction).cloned()
                    {
                        self.execute_action(action);
                        return;
                    }
                }

                if self.overview_open && self.layer_above_windows(self.pointer_pos).is_none() {
                    return;
                }
                let horizontal_v120 = event.amount_v120(Axis::Horizontal);
                let vertical_v120 = event.amount_v120(Axis::Vertical);

                let mut frame = AxisFrame::new(event.time_msec()).source(source);
                if horizontal != 0.0 {
                    frame = frame.value(Axis::Horizontal, horizontal);
                    if let Some(discrete) = horizontal_v120 {
                        frame = frame.v120(Axis::Horizontal, discrete as i32);
                    }
                }
                if vertical != 0.0 {
                    frame = frame.value(Axis::Vertical, vertical);
                    if let Some(discrete) = vertical_v120 {
                        frame = frame.v120(Axis::Vertical, discrete as i32);
                    }
                }
                if source == AxisSource::Finger {
                    if event.amount(Axis::Horizontal) == Some(0.0) {
                        frame = frame.stop(Axis::Horizontal);
                    }
                    if event.amount(Axis::Vertical) == Some(0.0) {
                        frame = frame.stop(Axis::Vertical);
                    }
                }

                self.sync_pointer_focus(event.time_msec());

                let pointer = self.pointer.clone();
                pointer.axis(self, frame);
                pointer.frame(self);
            }

            _ => {}
        }
    }

    fn clamp_pointer_to_outputs(
        &self,
        point: Point<f64, Logical>,
        restrict: Option<usize>,
    ) -> Option<(usize, Point<f64, Logical>)> {
        const STACK_OUTPUTS: usize = 8;
        if self.outputs.len() <= STACK_OUTPUTS {
            let mut rects = [None; STACK_OUTPUTS];
            for (slot, output) in rects.iter_mut().zip(&self.outputs) {
                *slot = output.global_rect();
            }
            return crate::state::clamp_pointer_to_output(
                &rects[..self.outputs.len()],
                point,
                restrict,
            );
        }
        let rects: Vec<Option<Rectangle<f64, Logical>>> = self
            .outputs
            .iter()
            .map(|output| output.global_rect())
            .collect();
        crate::state::clamp_pointer_to_output(&rects, point, restrict)
    }

    fn pointer_moved(
        &mut self,
        global: Point<f64, Logical>,
        time: u32,
        relative: Option<(Point<f64, Logical>, Point<f64, Logical>)>,
    ) {
        self.move_pointer(global, time, true, relative);
    }

    /// Move the pointer to the centre of an output without a physical motion
    /// event. Monitor focus actions use this so keyboard focus takes the
    /// cursor with it, matching niri's `move_cursor_to_output`.
    pub(crate) fn warp_pointer_to_output_center(&mut self, output_index: usize, time: u32) {
        // A drag is locked to its starting output; warping would yank the
        // dragged window across the screen. Let the physical pointer finish
        // the drag instead.
        if self.drag.is_some() || self.dnd_active {
            return;
        }
        let Some(rect) = self
            .outputs
            .get(output_index)
            .and_then(crate::state::OutputState::global_rect)
        else {
            return;
        };
        let centre = crate::state::rect_center(rect);
        tracing::debug!(
            output = output_index,
            ?centre,
            "warp pointer to output centre"
        );
        self.last_absolute_pointer = None;
        self.move_pointer(centre, time, false, None);
    }

    fn move_pointer(
        &mut self,
        global: Point<f64, Logical>,
        time: u32,
        follow_mouse: bool,
        relative: Option<(Point<f64, Logical>, Point<f64, Logical>)>,
    ) {
        let pointer = self.pointer.clone();
        let mut confined = None;
        let mut current_under = None;

        if self.drag.is_none() && !self.overview_open && relative.is_some() {
            let under = self.pointer_focus_under(self.pointer_pos);
            let current_world = self.pointer_pos + self.pointer_camera_location();
            let mut locked = false;

            if let Some((surface, origin)) = &under {
                let surface_local = (current_world - *origin).to_i32_round();
                with_pointer_constraint(surface, &pointer, |constraint| {
                    let Some(constraint) = constraint else {
                        return;
                    };
                    let in_region = constraint
                        .region()
                        .is_none_or(|region| region.contains(surface_local));
                    // Activate a newly created constraint as soon as its surface
                    // (and region, if any) is under the pointer. Gamescope
                    // creates its lock before the first enter.
                    if !constraint.is_active() && in_region {
                        constraint.activate();
                    }
                    if !constraint.is_active() || !in_region {
                        return;
                    }
                    match &*constraint {
                        PointerConstraint::Locked(_) => {
                            locked = true;
                        }
                        PointerConstraint::Confined(confine) => {
                            confined = Some((surface.clone(), *origin, confine.region().cloned()));
                        }
                    }
                });
            }

            if locked {
                if let Some((delta, delta_unaccel)) = relative {
                    pointer.relative_motion(
                        self,
                        under,
                        &RelativeMotionEvent {
                            delta,
                            delta_unaccel,
                            utime: u64::from(time) * 1000,
                        },
                    );
                }
                pointer.frame(self);
                return;
            }
            current_under = under;
        }

        let restrict = match self.drag.as_ref() {
            Some(Drag::Move { .. }) => None,
            Some(_) => Some(self.active_output),
            None => None,
        };
        let Some((mut output_index, mut global)) = self.clamp_pointer_to_outputs(global, restrict)
        else {
            return;
        };

        if let Some((focus_surface, focus_origin, region)) = confined {
            let confined_output = self.pointer_output;
            let Some(output_rect) = self.outputs[confined_output].global_rect() else {
                return;
            };
            let camera = self.outputs[confined_output].camera_location();
            let candidate = if output_index == confined_output {
                global
            } else {
                Point::from((
                    global.x.clamp(
                        output_rect.loc.x,
                        (output_rect.loc.x + output_rect.size.w - 1.0).max(output_rect.loc.x),
                    ),
                    global.y.clamp(
                        output_rect.loc.y,
                        (output_rect.loc.y + output_rect.size.h - 1.0).max(output_rect.loc.y),
                    ),
                ))
            };
            let allowed = confined_target(self.pointer_global, candidate, |point| {
                let local = point - output_rect.loc;
                let Some((surface, _)) = self.pointer_focus_under(local) else {
                    return false;
                };
                if surface != focus_surface {
                    return false;
                }
                if let Some(region) = &region {
                    let surface_local = (local + camera - focus_origin).to_i32_round();
                    return region.contains(surface_local);
                }
                true
            });
            match allowed {
                Some(allowed) => {
                    output_index = confined_output;
                    global = allowed;
                }
                None => {
                    if let Some((delta, delta_unaccel)) = relative {
                        pointer.relative_motion(
                            self,
                            current_under,
                            &RelativeMotionEvent {
                                delta,
                                delta_unaccel,
                                utime: u64::from(time) * 1000,
                            },
                        );
                    }
                    pointer.frame(self);
                    return;
                }
            }
        }

        let old_pointer_output = self.pointer_output;
        self.pointer_output = output_index;
        if old_pointer_output != output_index {
            if self.dnd_active {
                self.mark_full_redraw_output(old_pointer_output);
            } else {
                self.request_redraw_output(old_pointer_output);
            }
        }
        self.pointer_global = global;
        let Some(rect) = self.outputs[output_index].global_rect() else {
            return;
        };
        let pos = global - rect.loc;
        self.pointer_pos = pos;
        if self.drag.is_none() && !self.dnd_active {
            self.activate_pointer_output();
        } else if self.dnd_active && self.active_output != output_index {
            // DnD can cross outputs. Make the output under the pointer active
            // so target hit-testing uses that output's active workspace, but
            // do not run normal hover/focus handling.
            let previous = self.active_output;
            self.active_output = output_index;
            self.request_redraw_output(previous);
        }

        // While dragging across outputs, `pos` is local to the output under
        // the pointer. Send the client coordinates in that output's space, not
        // the active output's space.
        let world = if self.drag.is_some() {
            self.outputs[output_index].overview_screen_to_world(pos)
        } else {
            pos + self.pointer_camera_location()
        };
        let serial = SERIAL_COUNTER.next_serial();

        if self.drag.is_some() {
            let motion = MotionEvent {
                location: world,
                serial,
                time,
            };
            pointer.motion(self, None, &motion);
            pointer.frame(self);
            self.pointer_motion = None;
            self.advance_drag(world);
        } else {
            // Focus first: activation can scroll the camera to fit the hovered
            // window. Pointer focus and the client-visible coordinates must be
            // computed after that, with the new camera position.
            if follow_mouse && !self.dnd_active && !self.overview_open {
                self.hover_focus(pos);
            }
            let world = pos + self.pointer_camera_location();
            let under = self.pointer_focus_under(pos);
            let snapshot = crate::state::PointerMotionSnapshot {
                surface: under.as_ref().map(|(surface, _)| surface.clone()),
                origin: under
                    .as_ref()
                    .map_or_else(|| Point::from((0.0, 0.0)), |(_, origin)| *origin),
                location: world,
            };
            let motion = MotionEvent {
                location: world,
                serial,
                time,
            };
            pointer.motion(self, under.clone(), &motion);
            if let Some((delta, delta_unaccel)) = relative {
                pointer.relative_motion(
                    self,
                    under,
                    &RelativeMotionEvent {
                        delta,
                        delta_unaccel,
                        utime: u64::from(time) * 1000,
                    },
                );
            }
            pointer.frame(self);
            self.pointer_motion = Some(snapshot);
            // Cursor movement is part of the damage tracker; do not reset the
            // whole swapchain on every motion event.
            self.request_redraw_output(output_index);
        }
    }

    fn hover_focus(&mut self, local: Point<f64, Logical>) {
        let mode = self.active_mode();
        let enabled = match mode {
            crate::workspace::Mode::Scroll => self.config.input.focus_follows_mouse.scroll,
            crate::workspace::Mode::Canvas => self.config.input.focus_follows_mouse.canvas,
        };
        // A fullscreen overlay owns focus, and pointer motion must not interrupt
        // an active MRU cycle.
        if !enabled || self.active_fullscreen().is_some() || self.mru_cycle.is_some() {
            return;
        }

        // Niri suppresses focus-follows-mouse while the view is moving.
        let output = self.output_state();
        if output.view_x.is_animating() || output.view_y.is_animating() {
            return;
        }

        if self.layer_above_windows(local).is_some() || self.keyboard_layer_focused() {
            return;
        }

        // Never steal focus from an open popup (menus, completion lists,
        // context menus). Popups belong to the window under the cursor and
        // must keep keyboard focus while visible.
        if let Some(focused) = self.keyboard.current_focus()
            && self.popups.find_popup(&focused).is_some()
        {
            return;
        }

        let world = local + self.camera_location();
        let Some(window) = self.window_under(world) else {
            return;
        };

        let surface = window.wl_surface().map(|surface| surface.into_owned());
        if surface.as_ref() == self.keyboard.current_focus().as_ref() {
            return;
        }

        // Focus follows mouse only for windows that are completely on-screen.
        // A partially visible window requires an explicit click, which also
        // fits the camera.
        let output_size = self
            .output_state()
            .global_rect()
            .map(|rect| rect.size)
            .unwrap_or_else(|| smithay::utils::Size::from((1.0, 1.0)));
        let output_rect = Rectangle::from_size(output_size);

        // Use the visual rectangle. A client that has not committed yet may
        // still report a zero-sized surface.
        let window_rect = match mode {
            crate::workspace::Mode::Scroll => self.active_layout().tile_rect(&window).map(|rect| {
                Rectangle::new(
                    rect.loc - self.camera_location(),
                    smithay::utils::Size::from((rect.size.w, rect.size.h)),
                )
            }),
            crate::workspace::Mode::Canvas => self.output_state().workspaces
                [self.active_workspace()]
            .canvas
            .rect(&window)
            .map(|rect| Rectangle::new(rect.loc - self.camera_location(), rect.size)),
        }
        .unwrap_or_else(|| {
            Rectangle::new(
                self.space()
                    .element_location(&window)
                    .unwrap_or_default()
                    .to_f64()
                    - self.camera_location(),
                window.geometry().size.to_f64(),
            )
        });

        // Half-pixel tolerance for integer-rounded output and tile sizes.
        let fully_visible = window_rect.loc.x >= output_rect.loc.x - 0.5
            && window_rect.loc.y >= output_rect.loc.y - 0.5
            && window_rect.loc.x + window_rect.size.w
                <= output_rect.loc.x + output_rect.size.w + 0.5
            && window_rect.loc.y + window_rect.size.h
                <= output_rect.loc.y + output_rect.size.h + 0.5;

        if fully_visible {
            tracing::trace!(
                window = ?window.geometry(),
                ?mode,
                "focus follows mouse"
            );
            let serial = SERIAL_COUNTER.next_serial();
            // Hover focus in canvas changes focus and the border only. It must
            // not raise the window or move the camera; clicking still does.
            self.focus_window_internal(
                Some(&window),
                serial,
                mode == crate::workspace::Mode::Scroll,
            );
        }
    }

    /// Drop client pointer focus immediately when a compositor drag takes over
    /// so no stale pre-drag surface origin lingers before the first motion.
    pub(crate) fn clear_pointer_focus(&mut self, time: u32) {
        let location = self.pointer_pos + self.pointer_camera_location();
        let serial = SERIAL_COUNTER.next_serial();
        let pointer = self.pointer.clone();
        pointer.motion(
            self,
            None,
            &MotionEvent {
                location,
                serial,
                time,
            },
        );
        pointer.frame(self);
        self.pointer_motion = None;
    }

    pub(crate) fn sync_pointer_focus(&mut self, time: u32) {
        let under = self.pointer_focus_under(self.pointer_pos);
        let location = self.pointer_pos + self.pointer_camera_location();
        let snapshot = crate::state::PointerMotionSnapshot {
            surface: under.as_ref().map(|(surface, _)| surface.clone()),
            origin: under
                .as_ref()
                .map_or_else(|| Point::from((0.0, 0.0)), |(_, origin)| *origin),
            location,
        };
        if same_pointer_motion(self.pointer_motion.as_ref(), &snapshot) {
            return;
        }
        tracing::trace!(
            screen = ?self.pointer_pos,
            world = ?location,
            focus_origin = ?under.as_ref().map(|(_, origin)| *origin),
            focused = under.is_some(),
            "pointer focus sync"
        );
        let serial = SERIAL_COUNTER.next_serial();
        let pointer = self.pointer.clone();
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location,
                serial,
                time,
            },
        );
        pointer.frame(self);
        self.pointer_motion = Some(snapshot);
    }

    fn keyboard_shortcuts_inhibited(&self) -> bool {
        let Some(surface) = self.keyboard.current_focus() else {
            return false;
        };
        self.seat
            .keyboard_shortcuts_inhibitor_for_surface(&surface)
            .is_some_and(|inhibitor| inhibitor.is_active())
    }

    pub fn set_cursor_image(&mut self, status: smithay::input::pointer::CursorImageStatus) {
        self.cursor_status = status;
        let output = self.pointer_output;
        self.request_redraw_output(output);
    }

    /// Override the cursor for a compositor grab, remembering the client's
    /// cursor so it can be restored on release.
    pub(crate) fn set_drag_cursor(&mut self, icon: CursorIcon) {
        if self.drag_cursor.is_none() {
            self.drag_cursor = Some(self.cursor_status.clone());
        }
        self.set_cursor_image(CursorImageStatus::Named(icon));
    }
}

#[cfg(test)]
mod tests {
    use super::{confined_target, mouse_button_matches, same_pointer_motion, vt_from_keysym};
    use crate::config::MouseButton;
    use smithay::input::keyboard::Keysym;
    use smithay::utils::{Point, Rectangle, Size};

    #[test]
    fn function_keys_map_to_vts() {
        assert_eq!(vt_from_keysym(Keysym::F1), Some(1));
        assert_eq!(vt_from_keysym(Keysym::F12), Some(12));
        assert_eq!(vt_from_keysym(Keysym::A), None);
    }

    #[test]
    fn xf86_vt_keysyms_map_to_vts() {
        assert_eq!(vt_from_keysym(Keysym::new(0x1008_FE01)), Some(1));
        assert_eq!(vt_from_keysym(Keysym::new(0x1008_FE0C)), Some(12));
    }

    #[test]
    fn duplicate_pointer_motion_is_detected() {
        use crate::state::PointerMotionSnapshot;
        let first = PointerMotionSnapshot {
            surface: None,
            origin: Point::from((0.0, 0.0)),
            location: Point::from((10.0, 20.0)),
        };
        assert!(same_pointer_motion(Some(&first), &first));
        let moved = PointerMotionSnapshot {
            location: Point::from((11.0, 20.0)),
            ..first.clone()
        };
        assert!(!same_pointer_motion(Some(&first), &moved));
        assert!(!same_pointer_motion(None, &first));
    }

    #[test]
    fn mouse_buttons_match_native_and_nested_codes() {
        assert!(!mouse_button_matches(MouseButton::None, 0x112));
        assert!(!mouse_button_matches(MouseButton::None, 0x113));
        assert!(mouse_button_matches(MouseButton::Back, 0x113));
        assert!(mouse_button_matches(MouseButton::Back, 0x116));
        assert!(mouse_button_matches(MouseButton::Forward, 0x114));
        assert!(mouse_button_matches(MouseButton::Forward, 0x115));
        assert!(mouse_button_matches(MouseButton::Middle, 0x112));
        assert!(!mouse_button_matches(MouseButton::Back, 0x115));
        assert!(!mouse_button_matches(MouseButton::Forward, 0x116));
    }

    #[test]
    fn confined_target_allows_inside_slides_on_edge_and_blocks_corner() {
        let bounds = Rectangle::new(Point::from((0.0, 0.0)), Size::from((100.0, 100.0)));
        let inside = |point: Point<f64, _>| bounds.contains(point);

        // Move wholly inside.
        assert_eq!(
            confined_target(Point::from((50.0, 50.0)), Point::from((60.0, 70.0)), inside),
            Some(Point::from((60.0, 70.0)))
        );
        // Hit top edge while moving right -> slide along X.
        assert_eq!(
            confined_target(Point::from((50.0, 1.0)), Point::from((65.0, -5.0)), inside),
            Some(Point::from((65.0, 1.0)))
        );
        // Hit left edge while moving down -> slide along Y.
        assert_eq!(
            confined_target(Point::from((1.0, 50.0)), Point::from((-4.0, 65.0)), inside),
            Some(Point::from((1.0, 65.0)))
        );
        // Push outward into corner -> blocked completely.
        assert_eq!(
            confined_target(Point::from((1.0, 1.0)), Point::from((-5.0, -5.0)), inside),
            None
        );
    }
}
