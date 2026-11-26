use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use smithay::{
    backend::{
        allocator::Fourcc,
        egl::EGLDevice,
        renderer::{ImportDma, element::memory::MemoryRenderBuffer, gles::GlesRenderer},
    },
    desktop::{
        PopupGrab, PopupManager, Space, Window, WindowSurfaceType, layer_map_for_output,
        utils::send_frames_surface_tree,
    },
    input::{
        Seat, SeatState,
        keyboard::{KeyboardHandle, XkbConfig},
        pointer::{CursorIcon, CursorImageStatus, PointerHandle},
    },
    reexports::{
        calloop::{
            EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction,
            generic::Generic,
            timer::{TimeoutAction, Timer},
        },
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{IsAlive, Logical, Point, Rectangle, Size, Transform},
    wayland::{
        compositor::{
            CompositorClientState, CompositorState, SurfaceAttributes, TraversalAction,
            with_states, with_surface_tree_downward,
        },
        cursor_shape::CursorShapeManagerState,
        dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufState},
        idle_inhibit::IdleInhibitManagerState,
        idle_notify::IdleNotifierState,
        keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState,
        output::OutputManagerState,
        pointer_constraints::PointerConstraintsState,
        presentation::PresentationState,
        relative_pointer::RelativePointerManagerState,
        seat::WaylandFocus,
        selection::{
            data_device::DataDeviceState, ext_data_control,
            primary_selection::PrimarySelectionState, wlr_data_control,
        },
        shell::{
            wlr_layer::WlrLayerShellState,
            xdg::{SurfaceCachedState, XdgShellState, decoration::XdgDecorationState},
        },
        shm::ShmState,
        socket::ListeningSocketSource,
        text_input::TextInputManagerState,
        viewporter::ViewporterState,
        xdg_activation::XdgActivationState,
        xdg_system_bell::XdgSystemBellState,
    },
};
#[cfg(feature = "nested")]
use smithay::{
    backend::{renderer::damage::OutputDamageTracker, winit::WinitGraphicsBackend},
    utils::Physical,
};

mod actions;
mod canvas;
pub(crate) mod close;
mod focus;
mod fullscreen;
mod ipc;
mod output;
mod overview;

pub use output::OutputState;
pub(crate) use output::{auto_output_positions, clamp_pointer_to_output, rect_center};

use crate::animation::Clock;
use crate::config::Config;
use crate::config_watcher::ConfigWatcher;
use crate::cursor::Cursor;
use crate::drag::Drag;
use crate::gamma::GammaControlManagerState;
use crate::layout::Layout;
use crate::window_state::{CommitContext, WindowStates};
use crate::workspace::{Direction, Workspace};

#[cfg(feature = "nested")]
pub const OUTPUT_NAME: &str = "evawm-nested";

#[cfg(feature = "nested")]
pub struct NestedBackend {
    pub graphics: WinitGraphicsBackend<GlesRenderer>,
    pub damage_tracker: OutputDamageTracker,
}

/// Active display backend: either nested winit or a real DRM/libseat session.
/// Both variants are boxed so `EvaWM` stays small and `backend.take()` during
/// rendering moves a pointer rather than copying kilobytes of renderer state.
pub enum Backend {
    #[cfg(feature = "nested")]
    Nested(Box<NestedBackend>),
    Session(Box<crate::session::SessionBackend>),
}

impl Backend {
    pub fn renderer_mut(&mut self) -> &mut GlesRenderer {
        match self {
            #[cfg(feature = "nested")]
            Self::Nested(nested) => nested.graphics.renderer(),
            Self::Session(session) => &mut session.renderer,
        }
    }

    pub fn as_session(&self) -> Option<&crate::session::SessionBackend> {
        match self {
            Self::Session(session) => Some(session),
            #[cfg(feature = "nested")]
            Self::Nested(_) => None,
        }
    }

    pub fn as_session_mut(&mut self) -> Option<&mut crate::session::SessionBackend> {
        match self {
            Self::Session(session) => Some(session),
            #[cfg(feature = "nested")]
            Self::Nested(_) => None,
        }
    }
}

/// Wayland protocol global states registered at startup.
#[allow(dead_code)]
struct ProtocolGlobals {
    xdg_decoration: XdgDecorationState,
    output_manager: OutputManagerState,
    presentation: PresentationState,
    pointer_constraints: PointerConstraintsState,
    relative_pointer: RelativePointerManagerState,
    idle_inhibit: IdleInhibitManagerState,
    cursor_shape: CursorShapeManagerState,
    viewporter: ViewporterState,
    xdg_system_bell: XdgSystemBellState,
    dmabuf: Option<DmabufGlobal>,
}

/// Last pointer motion delivered to a client. Axis/button handlers refresh
/// focus before dispatch, but repeating an unchanged motion every wheel step
/// makes Steam/CEF drop scroll events.
#[derive(Debug, Clone)]
pub struct PointerMotionSnapshot {
    pub surface: Option<WlSurface>,
    pub origin: Point<f64, Logical>,
    pub location: Point<f64, Logical>,
}

pub struct EvaWM {
    pub display_handle: DisplayHandle,
    pub socket_name: OsString,
    pub loop_signal: LoopSignal,
    pub handle: LoopHandle<'static, EvaWM>,
    pub start_time: Instant,

    /// Active backend (`None` only while temporarily taken during a render or
    /// screencopy call that borrows the renderer alongside `&mut EvaWM`).
    pub backend: Option<Backend>,

    /// Per-output state: Output, Space, workspace layouts, and camera.
    pub outputs: Vec<OutputState>,
    pub active_output: usize,

    pub popups: PopupManager,
    pub animation_clock: Clock,
    pub window_states: WindowStates,
    /// Global focus history across modes and workspaces, front = most recent.
    pub mru: Vec<Window>,
    /// Active modifier-held MRU cycle, if any.
    pub mru_cycle: Option<crate::state::focus::MruCycle>,
    /// True while the cycle itself is moving focus, so it does not commit or
    /// reorder MRU prematurely.
    pub mru_cycle_step: bool,

    pub compositor_state: CompositorState,
    pub layer_shell_state: WlrLayerShellState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub seat_state: SeatState<EvaWM>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub ext_data_control_state: ext_data_control::DataControlState,
    pub wlr_data_control_state: wlr_data_control::DataControlState,
    pub xdg_activation_state: XdgActivationState,
    /// Active xdg popup grab, if any.
    pub popup_grab: Option<PopupGrab<EvaWM>>,
    pub gamma_control_manager_state: GammaControlManagerState,
    pub keyboard_shortcuts_inhibit_state: KeyboardShortcutsInhibitState,
    pub foreign_toplevel: crate::handlers::foreign_toplevel::ForeignToplevelState,
    pub workspace_protocol: crate::handlers::workspace::WorkspaceProtocolState,
    pub idle_notifier: IdleNotifierState<EvaWM>,
    /// Refcounted surfaces holding `zwp_idle_inhibitor_v1`.
    #[allow(clippy::mutable_key_type)]
    pub idle_inhibitors: HashMap<WlSurface, usize>,
    globals: ProtocolGlobals,
    pub seat: Seat<EvaWM>,
    pub pointer: PointerHandle<EvaWM>,
    pub keyboard: KeyboardHandle<EvaWM>,

    pub cursor: Cursor,
    pub cursor_status: CursorImageStatus,
    /// Cursor status to restore when a compositor move/resize grab ends.
    pub drag_cursor: Option<CursorImageStatus>,
    pub cursor_icon: Option<CursorIcon>,
    pub cursor_scale: u32,
    pub cursor_cache: HashMap<(CursorIcon, u32), (MemoryRenderBuffer, Point<i32, Logical>)>,
    pub cursor_buffer: MemoryRenderBuffer,
    pub cursor_hotspot: Point<i32, Logical>,
    /// Pointer position in the compositor-global logical coordinate space.
    pub pointer_global: Point<f64, Logical>,
    /// Output currently under the pointer.
    pub pointer_output: usize,
    /// Pointer position local to `pointer_output`.
    pub pointer_pos: Point<f64, Logical>,
    /// Previous raw position from `PointerMotionAbsolute` (nested winit).
    /// Used to compute incremental relative deltas even while a pointer lock
    /// keeps `pointer_global` frozen.
    pub last_absolute_pointer: Option<Point<f64, Logical>>,
    /// Last motion sent to the focused client surface.
    pub pointer_motion: Option<PointerMotionSnapshot>,
    /// Physical libinput devices, kept so settings can be reapplied on reload.
    pub libinput_devices: HashSet<smithay::reexports::input::Device>,

    pub drag: Option<Drag>,
    /// Active client-initiated Wayland DnD grab.
    pub dnd_active: bool,
    /// Optional icon surface supplied by the drag source.
    pub dnd_icon: Option<crate::dnd::DndIcon>,
    pub pan_held: bool,
    pub extra_mod_held: bool,
    /// Action repeated while its bind key is held.
    pub bind_repeat_action: Option<crate::config::Action>,
    pub bind_repeat_token: Option<smithay::reexports::calloop::RegistrationToken>,
    pub nested_mod_alt: bool,
    pub config: Config,
    pub config_path: Option<PathBuf>,
    pub config_watcher: Option<ConfigWatcher>,

    pub dmabuf_state: Option<DmabufState>,

    /// Stable per-window IPC identifiers.
    pub window_ids: HashMap<Window, u64>,
    /// Window -> (output, workspace) membership index for the commit and
    /// animation paths. Rebuilt after layout membership changes; lookups fall
    /// back to a scan if an entry is missing. `Window` hashes by stable surface
    /// identity; interior mutability does not affect hash/equality.
    #[allow(clippy::mutable_key_type)]
    pub(crate) window_locations: HashMap<Window, (usize, usize, u64)>,
    /// Surface -> window index for the commit path.
    #[allow(clippy::mutable_key_type)]
    pub(crate) surface_windows: HashMap<WlSurface, Window>,
    /// Xdg toplevels that have not yet committed a non-null buffer (newly
    /// created or unmapped via a null buffer).
    #[allow(clippy::mutable_key_type)]
    pub(crate) unmapped_windows: HashMap<WlSurface, UnmappedToplevel>,
    pub next_window_id: u64,
    /// IPC event subscribers.
    pub ipc_subscribers: Vec<std::sync::mpsc::SyncSender<String>>,
    pub ipc_server: Option<crate::ipc::IpcServer>,
    /// Coalesce nested `notify_ipc()` calls inside a single `execute_action`.
    pub(crate) defer_ipc: bool,
    pub(crate) ipc_pending: bool,
    /// Pre-bound X11 listeners and the on-demand XWayland satellite.
    pub xwayland: Option<crate::xwayland::XwaylandServer>,
    /// True while DRM outputs are powered down via `power-off-monitors`.
    pub monitors_off: bool,
    /// True while Niri-style Overview is open across all outputs.
    pub overview_open: bool,
    /// True between libseat pause and resume (VT switch or suspend).
    pub session_paused: bool,
    /// Consecutive DRM activation failures while waiting for resume.
    pub session_activation_retries: u8,
    /// A session-activation retry timer is already queued.
    pub session_activation_retry_armed: bool,
    /// Invalidates stale activation retry timers across pause cycles.
    pub session_activation_generation: u64,
    /// A timer has been queued to flush a commit-gated canvas configure.
    pub canvas_configure_timer_armed: bool,
    /// Deferred `wlr-screencopy` `copy_with_damage` requests waiting for output
    /// presentation/damage.
    pub pending_screencopies: Vec<crate::screencopy::PendingScreencopy>,
    /// Reusable offscreen texture and damage tracker for `wlr-screencopy`.
    pub(crate) screencopy_cache: crate::screencopy::ScreencopyCache,
}

/// A newly created (or null-buffer unmapped) xdg toplevel waiting for its
/// initial commit-configure-buffer handshake before entering a workspace.
pub struct UnmappedToplevel {
    pub window: Window,
    pub configured: bool,
    /// `Some(None)` = requested fullscreen on the active output;
    /// `Some(Some(index))` = requested fullscreen on a specific output.
    pub wants_fullscreen: Option<Option<usize>>,
}

impl UnmappedToplevel {
    pub fn new(window: Window) -> Self {
        Self {
            window,
            configured: false,
            wants_fullscreen: None,
        }
    }
}

/// Resolve a window's workspace using the membership index, with a scan
/// fallback for windows mapped or moved since the last rebuild.
#[allow(clippy::mutable_key_type)]
pub(crate) fn workspace_location_in(
    outputs: &[OutputState],
    locations: &HashMap<Window, (usize, usize, u64)>,
    window: &Window,
) -> Option<(usize, usize)> {
    if let Some((output_index, workspace_index, workspace_id)) = locations.get(window).copied()
        && outputs
            .get(output_index)
            .and_then(|output| output.workspaces.get(workspace_index))
            .is_some_and(|workspace| workspace.id == workspace_id && workspace.contains(window))
    {
        return Some((output_index, workspace_index));
    }

    outputs
        .iter()
        .enumerate()
        .find_map(|(output_index, output)| {
            output
                .workspaces
                .iter()
                .position(|workspace| workspace.contains(window))
                .map(|workspace| (output_index, workspace))
        })
}

impl EvaWM {
    pub fn new(
        display: Display<EvaWM>,
        event_loop: &mut EventLoop<'static, EvaWM>,
        backend: Backend,
        outputs: Vec<OutputState>,
        nested_mod_alt: bool,
        config: Config,
        config_path: Option<PathBuf>,
    ) -> Self {
        let display_handle = display.handle();

        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&display_handle);
        crate::screencopy::init(&display_handle);

        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(
            &display_handle,
            vec![
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Argb8888,
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Xrgb8888,
            ],
        );
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&display_handle);
        let ext_data_control_state = ext_data_control::DataControlState::new::<Self, _>(
            &display_handle,
            Some(&primary_selection_state),
            |_| true,
        );
        let wlr_data_control_state = wlr_data_control::DataControlState::new::<Self, _>(
            &display_handle,
            Some(&primary_selection_state),
            |_| true,
        );
        let xdg_activation_state = XdgActivationState::new::<Self>(&display_handle);
        let presentation_state = PresentationState::new::<Self>(
            &display_handle,
            smithay::utils::Clock::<smithay::utils::Monotonic>::new().id() as u32,
        );
        let gamma_control_manager_state = GammaControlManagerState::new(&display_handle);
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&display_handle);
        let relative_pointer_manager_state =
            RelativePointerManagerState::new::<Self>(&display_handle);
        let keyboard_shortcuts_inhibit_state =
            KeyboardShortcutsInhibitState::new::<Self>(&display_handle);
        let foreign_toplevel =
            crate::handlers::foreign_toplevel::ForeignToplevelState::new(&display_handle);
        let workspace_protocol =
            crate::handlers::workspace::WorkspaceProtocolState::new(&display_handle);
        let idle_notifier = IdleNotifierState::new(&display_handle, event_loop.handle());
        let idle_inhibit_manager_state = IdleInhibitManagerState::new::<Self>(&display_handle);
        let cursor_shape_manager_state = CursorShapeManagerState::new::<Self>(&display_handle);
        let viewporter_state = ViewporterState::new::<Self>(&display_handle);
        let xdg_system_bell_state = XdgSystemBellState::new::<Self>(&display_handle);
        TextInputManagerState::new::<Self>(&display_handle);
        let mut seat_state = SeatState::new();

        let mut seat = seat_state.new_wl_seat(&display_handle, "evawm");
        let repeat_delay = i32::try_from(config.input.repeat_delay).unwrap_or(250);
        let repeat_rate = i32::try_from(config.input.repeat_rate).unwrap_or(25);
        let keyboard = seat
            .add_keyboard(config.input.xkb.to_smithay(), repeat_delay, repeat_rate)
            .or_else(|err| {
                tracing::warn!(
                    ?err,
                    layout = %config.input.xkb.layout,
                    variant = %config.input.xkb.variant,
                    "configured XKB keymap failed; falling back to defaults"
                );
                seat.add_keyboard(XkbConfig::default(), repeat_delay, repeat_rate)
            })
            .expect("failed to add keyboard");
        let pointer = seat.add_pointer();

        let mut cursor = Cursor::load(&config.cursor.theme, config.cursor.size);
        let image = cursor.image(1);
        let cursor_buffer = MemoryRenderBuffer::from_slice(
            &image.pixels_rgba,
            Fourcc::Argb8888,
            (image.width as i32, image.height as i32),
            1,
            Transform::Normal,
            None,
        );
        let cursor_hotspot = Point::from((image.xhot as i32, image.yhot as i32));

        let animation_clock = Clock::new(config.animations.slowdown);

        let loop_signal = event_loop.get_signal();

        // Wayland client socket.
        let socket = ListeningSocketSource::new_auto().expect("failed to create wayland socket");
        let socket_name = socket.socket_name().to_os_string();
        event_loop
            .handle()
            .insert_source(socket, |stream, _, state: &mut EvaWM| {
                match state
                    .display_handle
                    .insert_client(stream, Arc::new(ClientState::default()))
                {
                    Ok(_) => tracing::debug!("client connected"),
                    Err(err) => tracing::warn!("failed to insert client: {err}"),
                }
            })
            .expect("failed to insert socket source");

        // Wayland display dispatch.
        event_loop
            .handle()
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state: &mut EvaWM| {
                    tracing::trace!("dispatching clients");
                    // SAFETY: we never drop the display.
                    if let Err(err) = unsafe { display.get_mut().dispatch_clients(state) } {
                        tracing::error!(?err, "wayland client dispatch failed");
                        state.loop_signal.stop();
                    }
                    let _ = state.display_handle.flush_clients();
                    for output in &mut state.outputs {
                        output.space.refresh();
                    }
                    state.popups.cleanup();
                    Ok(PostAction::Continue)
                },
            )
            .expect("failed to insert display source");

        // Live config reload. The watcher thread parses off the main thread
        // and sends the new config through a calloop channel.
        let config_watcher = config_path.as_ref().and_then(|path| {
            let (sender, receiver) = smithay::reexports::calloop::channel::channel();
            let watcher = ConfigWatcher::new(path.clone(), config.sources.clone(), sender)?;
            if let Err(err) =
                event_loop
                    .handle()
                    .insert_source(receiver, |event, _, state: &mut EvaWM| {
                        if let smithay::reexports::calloop::channel::Event::Msg(result) = event {
                            match result {
                                Ok(config) => state.apply_config(config),
                                Err(err) => {
                                    tracing::warn!(
                                        "config reload failed; keeping old config: {err}"
                                    );
                                }
                            }
                        }
                    })
            {
                tracing::warn!("failed to insert config watcher source: {err}");
                return None;
            }
            Some(watcher)
        });

        // Native IPC. The listener runs off-thread and forwards parsed
        // requests here; responses and subscriber events are written by the
        // connection threads.
        let (ipc_tx, ipc_rx) = smithay::reexports::calloop::channel::channel();
        let ipc_server = match crate::ipc::start_server(&socket_name, ipc_tx) {
            Ok(server) => {
                tracing::info!(socket = %server.path().display(), "IPC listening");
                Some(server)
            }
            Err(err) => {
                tracing::warn!("failed to start IPC socket: {err}");
                None
            }
        };
        event_loop
            .handle()
            .insert_source(ipc_rx, |event, _, state: &mut EvaWM| {
                if let smithay::reexports::calloop::channel::Event::Msg(request) = event {
                    state.handle_ipc_request(request);
                }
            })
            .expect("failed to insert IPC source");

        tracing::info!(
            "listening on WAYLAND_DISPLAY={}",
            socket_name.to_string_lossy()
        );

        let (pointer_global, pointer_pos) = outputs
            .first()
            .and_then(OutputState::global_rect)
            .map(|rect| {
                let global = rect_center(rect);
                (global, global - rect.loc)
            })
            .unwrap_or_default();

        Self {
            display_handle,
            socket_name,
            loop_signal,
            start_time: Instant::now(),

            handle: event_loop.handle(),

            backend: Some(backend),

            outputs,
            active_output: 0,

            popups: PopupManager::default(),
            animation_clock,
            window_states: WindowStates::default(),
            mru: Vec::new(),
            mru_cycle: None,
            mru_cycle_step: false,

            compositor_state,
            layer_shell_state,
            xdg_shell_state,
            shm_state,
            seat_state,
            data_device_state,
            primary_selection_state,
            ext_data_control_state,
            wlr_data_control_state,
            xdg_activation_state,
            popup_grab: None,
            gamma_control_manager_state,
            keyboard_shortcuts_inhibit_state,
            foreign_toplevel,
            workspace_protocol,
            idle_notifier,
            idle_inhibitors: HashMap::new(),
            globals: ProtocolGlobals {
                xdg_decoration: xdg_decoration_state,
                output_manager: output_manager_state,
                presentation: presentation_state,
                pointer_constraints: pointer_constraints_state,
                relative_pointer: relative_pointer_manager_state,
                idle_inhibit: idle_inhibit_manager_state,
                cursor_shape: cursor_shape_manager_state,
                viewporter: viewporter_state,
                xdg_system_bell: xdg_system_bell_state,
                dmabuf: None,
            },
            seat,
            pointer,
            keyboard,

            cursor,
            cursor_status: CursorImageStatus::default_named(),
            drag_cursor: None,
            cursor_icon: None,
            cursor_scale: 1,
            cursor_cache: HashMap::new(),
            cursor_buffer,
            cursor_hotspot,
            pointer_global,
            pointer_output: 0,
            pointer_pos,
            last_absolute_pointer: None,
            pointer_motion: None,
            libinput_devices: HashSet::new(),

            drag: None,
            dnd_active: false,
            dnd_icon: None,
            pan_held: false,
            extra_mod_held: false,
            bind_repeat_action: None,
            bind_repeat_token: None,
            nested_mod_alt,

            config,
            config_path,
            config_watcher,

            dmabuf_state: None,
            ipc_server,

            xwayland: None,
            window_ids: HashMap::new(),
            window_locations: HashMap::new(),
            surface_windows: HashMap::new(),
            unmapped_windows: HashMap::new(),
            next_window_id: 1,
            ipc_subscribers: Vec::new(),
            defer_ipc: false,
            ipc_pending: false,
            monitors_off: false,
            overview_open: false,
            session_paused: false,
            session_activation_retries: 0,
            session_activation_retry_armed: false,
            session_activation_generation: 0,
            canvas_configure_timer_armed: false,
            pending_screencopies: Vec::new(),
            screencopy_cache: crate::screencopy::ScreencopyCache::default(),
        }
    }

    pub fn session(&self) -> Option<&crate::session::SessionBackend> {
        self.backend.as_ref().and_then(Backend::as_session)
    }

    pub fn session_mut(&mut self) -> Option<&mut crate::session::SessionBackend> {
        self.backend.as_mut().and_then(Backend::as_session_mut)
    }

    pub fn is_session(&self) -> bool {
        matches!(self.backend, Some(Backend::Session(_)))
    }

    pub fn is_nested(&self) -> bool {
        #[cfg(feature = "nested")]
        {
            matches!(self.backend, Some(Backend::Nested(_)))
        }
        #[cfg(not(feature = "nested"))]
        {
            false
        }
    }

    /// Apply compositor session environment variables to a child process
    /// without mutating this process's global `environ` while background
    /// threads are running.
    pub fn apply_child_env(&self, command: &mut std::process::Command) {
        command.env("WAYLAND_DISPLAY", &self.socket_name);
        if self.is_session() {
            command.env("XDG_CURRENT_DESKTOP", "evawm");
            command.env("XDG_SESSION_TYPE", "wayland");
        }
        match self.x11_display() {
            Some(display) => {
                command.env("DISPLAY", display);
            }
            None => {
                command.env_remove("DISPLAY");
            }
        }
        match self.ipc_server.as_ref() {
            Some(ipc) => {
                command.env("EVAWM_SOCKET", ipc.path());
            }
            None => {
                command.env_remove("EVAWM_SOCKET");
            }
        }
    }

    /// Spawn a shell command via `sh -c` with the compositor child environment
    /// and background reaping.
    pub fn spawn_shell_command(&self, command: &str) -> anyhow::Result<()> {
        let mut process = std::process::Command::new("sh");
        process.arg("-c").arg(command);
        self.apply_child_env(&mut process);
        crate::spawn::reap(process, "shell command")
            .map_err(|err| anyhow::anyhow!("failed to spawn {command}: {err}"))
    }

    /// Spawn configured startup commands once with the compositor's child
    /// environment and background reaping.
    pub fn run_startup(&self) {
        for command in &self.config.startup {
            let Some((program, args)) = command.split_first() else {
                continue;
            };
            let mut process = std::process::Command::new(program);
            process.args(args);
            self.apply_child_env(&mut process);
            match crate::spawn::reap(process, program) {
                Ok(()) => tracing::info!(program, "started startup command"),
                Err(err) => tracing::warn!("failed to start startup command {program}: {err}"),
            }
        }
    }

    /// Register zwp_linux_dmabuf_v1. Must be called after the renderer exists
    /// and before clients connect.
    pub fn init_dmabuf(&mut self) {
        let Some(renderer) = self.backend.as_mut().map(Backend::renderer_mut) else {
            return;
        };
        let render_node = EGLDevice::device_for_display(renderer.egl_context().display())
            .and_then(|device| device.try_get_render_node())
            .ok()
            .flatten();
        let formats = renderer.dmabuf_formats();

        let mut state = DmabufState::new();
        let global = match render_node {
            Some(node) => {
                match DmabufFeedbackBuilder::new(node.dev_id(), formats.clone()).build() {
                    Ok(feedback) => state.create_global_with_default_feedback::<Self>(
                        &self.display_handle,
                        &feedback,
                    ),
                    Err(err) => {
                        tracing::warn!(
                            ?err,
                            "failed to build dmabuf feedback; falling back to v3 global"
                        );
                        state.create_global::<Self>(&self.display_handle, formats)
                    }
                }
            }
            None => state.create_global::<Self>(&self.display_handle, formats),
        };

        self.dmabuf_state = Some(state);
        self.globals.dmabuf = Some(global);
        tracing::info!("dmabuf global registered");
    }

    pub fn output_state(&self) -> &OutputState {
        &self.outputs[self.active_output]
    }

    pub fn output_state_mut(&mut self) -> &mut OutputState {
        &mut self.outputs[self.active_output]
    }

    pub fn space(&self) -> &Space<Window> {
        &self.output_state().space
    }

    pub fn space_mut(&mut self) -> &mut Space<Window> {
        &mut self.output_state_mut().space
    }

    pub fn workspaces(&self) -> &[Workspace] {
        &self.output_state().workspaces
    }

    pub fn active_workspace(&self) -> usize {
        self.output_state().active_workspace
    }

    pub fn active_layout(&self) -> &Layout {
        self.output_state().active_layout()
    }

    pub fn active_fullscreen(&self) -> Option<&Window> {
        self.output_state().workspaces[self.active_workspace()].fullscreen_window()
    }

    pub fn active_layout_mut(&mut self) -> &mut Layout {
        self.output_state_mut().active_layout_mut()
    }

    pub fn output_in_direction(&self, current: usize, direction: Direction) -> Option<usize> {
        let rects: Vec<Option<Rectangle<f64, Logical>>> =
            self.outputs.iter().map(OutputState::global_rect).collect();
        output::output_in_direction(&rects, current, direction)
    }

    /// Request a full redraw of one output (animations, layout transitions, or
    /// output changes). `render_output` resets buffer ages when `full_redraw > 0`.
    pub fn mark_full_redraw_output(&mut self, index: usize) {
        if let Some(output) = self.outputs.get_mut(index) {
            output.full_redraw = output.full_redraw.max(1);
            output.needs_redraw = true;
        }
        self.request_redraw_output(index);
    }

    /// Request a full redraw of every output. Used for reloads and output
    /// changes that affect all render targets.
    pub fn mark_full_redraw(&mut self) {
        for output in &mut self.outputs {
            output.full_redraw = output.full_redraw.max(1);
            output.needs_redraw = true;
        }
        self.request_redraw_all();
    }

    /// Run a closure with the active GPU renderer while keeping the backend
    /// owned by the compositor. Used to snapshot a tile or capture a frame.
    pub(crate) fn with_renderer<R>(
        &mut self,
        f: impl FnOnce(&mut GlesRenderer, &mut EvaWM) -> R,
    ) -> Option<R> {
        let mut backend = self.backend.take()?;
        let result = f(backend.renderer_mut(), self);
        self.backend = Some(backend);
        Some(result)
    }

    pub(crate) fn window_and_output_for_surface(
        &self,
        surface: &WlSurface,
    ) -> Option<(usize, Window)> {
        if let Some(window) = self.surface_windows.get(surface).cloned()
            && let Some(output) = self.window_output_index(&window)
        {
            return Some((output, window));
        }
        self.outputs
            .iter()
            .enumerate()
            .find_map(|(output_index, output)| {
                output
                    .workspaces
                    .iter()
                    .find_map(|workspace| workspace.window_for_surface(surface))
                    .map(|window| (output_index, window.clone()))
            })
    }

    pub(crate) fn snapshot_window(
        &mut self,
        output_index: usize,
        window: &Window,
    ) -> Option<crate::snapshot::SnapshotRenderElement> {
        self.with_renderer(|renderer, state| {
            crate::render::window::snapshot_tile(renderer, state, output_index, window)
        })
        .flatten()
    }

    pub fn request_redraw(&mut self) {
        self.request_redraw_output(self.active_output);
    }

    pub fn power_off_monitors(&mut self) {
        if let Some(session) = self.session_mut() {
            session.power_off();
            self.monitors_off = true;
            tracing::info!("monitors powered off");
        } else {
            tracing::warn!("power-off-monitors requires a DRM session");
        }
    }

    pub(crate) fn wake_monitors(&mut self) {
        if !self.monitors_off {
            return;
        }
        self.monitors_off = false;
        if let Some(session) = self.session_mut() {
            session.reset_buffers();
            session.reapply_gamma();
        }
        tracing::info!("monitors woke");
        self.mark_full_redraw();
    }

    pub fn request_redraw_all(&mut self) {
        for index in 0..self.outputs.len() {
            self.request_redraw_output(index);
        }
    }

    pub fn request_redraw_output(&mut self, index: usize) {
        if index >= self.outputs.len()
            || self.session_paused
            || self.monitors_off
            || self.session().is_some_and(|session| !session.is_active())
        {
            return;
        }
        self.outputs[index].needs_redraw = true;

        #[cfg(feature = "nested")]
        if let Some(Backend::Nested(nested)) = &self.backend {
            if index == 0 {
                nested.graphics.window().request_redraw();
            }
            return;
        }

        if self.outputs[index].render_scheduled {
            return;
        }
        // While a frame is queued, wait for that output's VBlank. This keeps
        // animation frames paced by the monitor that owns them.
        if self
            .session()
            .is_some_and(|session| session.has_frame_pending(index))
            || self.outputs[index].render_retry_armed
        {
            return;
        }

        self.outputs[index].render_scheduled = true;
        self.handle.clone().insert_idle(move |state: &mut EvaWM| {
            if let Some(output) = state.outputs.get_mut(index) {
                output.render_scheduled = false;
            }
            if state
                .outputs
                .get(index)
                .is_some_and(|output| output.needs_redraw)
                && let Err(err) = crate::render::render_output(state, index)
            {
                if state.is_session() {
                    // DRM can fail for a few frames while a VT/suspend resume
                    // settles. Retry with backoff and keep a slow recovery
                    // probe instead of permanently freezing the output.
                    let Some(output) = state.outputs.get_mut(index) else {
                        return;
                    };
                    output.render_retries = output.render_retries.saturating_add(1);
                    let retries = output.render_retries;
                    output.needs_redraw = true;

                    if retries == 1 {
                        tracing::error!("render failed for output {index}: {err:?}; retrying");
                    } else if retries == 8 {
                        tracing::error!(
                            "render still failing for output {index} after 8 attempts; \
                             continuing low-frequency recovery"
                        );
                    } else {
                        tracing::debug!(
                            "render failed for output {index} (attempt {retries}): {err:?}"
                        );
                    }

                    if !output.render_retry_armed {
                        output.render_retry_armed = true;
                        let handle = state.handle.clone();
                        let delay = render_retry_delay(retries);
                        let _ = handle.insert_source(
                            Timer::from_duration(delay),
                            move |_, _, state: &mut EvaWM| {
                                if let Some(output) = state.outputs.get_mut(index) {
                                    output.render_retry_armed = false;
                                }
                                state.request_redraw_output(index);
                                TimeoutAction::Drop
                            },
                        );
                    }
                } else {
                    tracing::error!("render failed for output {index}: {err:?}");
                    state.loop_signal.stop();
                }
            }
        });
    }

    pub fn camera_location(&self) -> Point<f64, Logical> {
        self.output_state().camera_location()
    }

    pub fn pointer_camera_location(&self) -> Point<f64, Logical> {
        self.outputs
            .get(self.pointer_output)
            .map_or_else(|| Point::from((0.0, 0.0)), OutputState::camera_location)
    }

    pub fn window_under(&self, world: Point<f64, Logical>) -> Option<Window> {
        let output = self.output_state();
        let workspace = &output.workspaces[output.active_workspace];
        if let Some(fullscreen) = workspace.fullscreen_window() {
            let output_point = world - output.camera_location();
            let output_rect = Rectangle::from_size(workspace.scroll.output_size);
            let local = output_point + fullscreen.geometry().loc.to_f64();
            if output_rect.contains(output_point)
                && fullscreen
                    .surface_under(local, WindowSurfaceType::ALL)
                    .is_some()
            {
                return Some(fullscreen.clone());
            }
            return None;
        }
        if workspace.mode == crate::workspace::Mode::Canvas {
            // Check both content and borders per window top-to-bottom so a
            // lower window's content cannot steal clicks through an upper
            // window's border frame.
            let inset = workspace.scroll.frame_inset();
            return output.space.elements().rev().find_map(|window| {
                if self.window_states.is_closing(window) {
                    return None;
                }
                if let Some(origin) = output.window_surface_origin(window)
                    && window
                        .surface_under(world - origin.to_f64(), WindowSurfaceType::ALL)
                        .is_some()
                {
                    return Some(window.clone());
                }
                let rect = workspace.canvas.rect(window)?;
                let bounds = Rectangle::new(
                    rect.loc - Point::from((inset, inset)),
                    Size::from((rect.size.w + 2.0 * inset, rect.size.h + 2.0 * inset)),
                );
                bounds.contains(world).then(|| window.clone())
            });
        }
        let under = output.space.elements().rev().find_map(|window| {
            if self.window_states.is_closing(window) {
                return None;
            }
            let origin = output.window_surface_origin(window)?;
            window
                .surface_under(world - origin.to_f64(), WindowSurfaceType::ALL)
                .map(|_| window.clone())
        });
        under.or_else(|| workspace.scroll.window_at(world))
    }

    pub fn pointer_focus_under(
        &self,
        local: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let output_index = self.pointer_output;
        let output = self.outputs.get(output_index)?;
        let camera = output.camera_location();
        let fullscreen_above_top = self.fullscreen_above_top_layer_on(output_index);
        if let Some((layer, origin)) = output.layer_above_windows_under(local, fullscreen_above_top)
        {
            // Layer surfaces are screen-space. Convert their origin into the
            // same world space used by the pointer event.
            return Some((layer, origin + camera));
        }
        if !self.overview_open
            && let Some(hit) = self.surface_under_on(output_index, local + camera)
        {
            return Some(hit);
        }
        if !fullscreen_above_top
            && let Some((layer, origin)) = output.layer_below_windows_under(local)
        {
            return Some((layer, origin + camera));
        }
        None
    }

    pub fn window_is_fixed_size(&self, window: &Window) -> bool {
        self.workspaces()
            .iter()
            .any(|layout| layout.window_is_fixed_size(window))
    }

    pub(crate) fn window_constraints(window: &Window) -> (Size<i32, Logical>, Size<i32, Logical>) {
        let Some(toplevel) = window.toplevel() else {
            return (Size::from((0, 0)), Size::from((0, 0)));
        };
        with_states(toplevel.wl_surface(), |states| {
            let mut cached = states.cached_state.get::<SurfaceCachedState>();
            let state = cached.current();
            (state.min_size, state.max_size)
        })
    }

    pub fn animation_now(&self) -> f64 {
        self.animation_clock.now()
    }

    pub(crate) fn event_time_msec(&self) -> u32 {
        self.start_time.elapsed().as_millis() as u32
    }

    pub(crate) fn map_output(&mut self) {
        self.output_state_mut().map_output();
    }

    /// A Move drag owns the dragged window's Space location until release.
    /// Layout passes must not remap it back to its source tile, on the same
    /// output or another one, while the button is held.
    pub(crate) fn move_drag_active(&self) -> bool {
        matches!(self.drag, Some(crate::drag::Drag::Move { .. }))
    }

    pub(crate) fn apply_layout(&mut self) {
        self.apply_layout_output(self.active_output);
    }

    pub(crate) fn apply_layout_output(&mut self, output_index: usize) {
        if self.move_drag_active() {
            return;
        }
        let now = self.animation_now();
        let config = self.config.animations.view_movement;
        let animations_off = self.config.animations.off;
        if let Some(output) = self.outputs.get_mut(output_index) {
            output.apply_layout(now, config, animations_off);
        }
    }

    /// Advance the animations owned by one output, immediately before that
    /// output renders. The camera is per-output; windows are ticked only by
    /// the output that owns them, so a loop that renders two outputs does not
    /// advance the same window animation twice.
    ///
    /// Marks the output dirty when an animation is still running. It does not
    /// request a render: the output's VBlank or winit event already owns the
    /// pacing.
    pub fn tick_animations_for_output(&mut self, index: usize, now: f64) {
        let Some(output) = self.outputs.get_mut(index) else {
            return;
        };

        let view_changed = output.view_x.tick(now) | output.view_y.tick(now);
        let overview_changed = output.overview_zoom.tick(now);
        if view_changed || overview_changed {
            if view_changed {
                // `Space` stores the output location; without this the camera
                // values advance but the rendered view never moves.
                output.map_output();
            }
            output.needs_redraw = true;
            output.full_redraw = output.full_redraw.max(1);
        }

        // Windows without an owner are transient (created but not laid out
        // yet). Tick them on whichever output renders so their animations can
        // finish, but do not tick another output's windows here.
        let outputs = &self.outputs;
        #[allow(clippy::mutable_key_type)]
        let locations = &self.window_locations;
        let ticked = self
            .window_states
            .tick_windows_matching(now, |window, closing_output| {
                let owner = closing_output.or_else(|| {
                    workspace_location_in(outputs, locations, window).map(|(output, _)| output)
                });
                owner.is_none() || owner == Some(index)
            });

        if ticked && let Some(output) = self.outputs.get_mut(index) {
            output.needs_redraw = true;
            output.full_redraw = output.full_redraw.max(1);
        }

        self.finish_closed_windows(index);
    }

    pub(crate) fn window_output_index(&self, window: &Window) -> Option<usize> {
        self.workspace_for_window(window)
            .map(|(output, _)| output)
            .or_else(|| {
                self.outputs
                    .iter()
                    .position(|output| output.space.elements().any(|mapped| mapped == window))
            })
    }

    pub(crate) fn output_animations_active(&self, index: usize) -> bool {
        let Some(output) = self.outputs.get(index) else {
            return false;
        };
        output.view_x.is_animating()
            || output.view_y.is_animating()
            || output.overview_zoom.is_animating()
            || self.window_states.active_windows().any(|window| {
                self.window_states.closing_output_for(window) == Some(index)
                    || self.window_output_index(window) == Some(index)
            })
    }

    pub fn begin_mouse_resize(&mut self, window: &Window) {
        self.window_states.begin_mouse_resize(window);
    }

    /// Flush a commit-gated canvas configure after the previous request has
    /// had time to commit. The render path normally flushes first; this timer
    /// covers a client that stops committing without dropping the connection.
    pub(crate) fn schedule_canvas_configure_flush(&mut self) {
        if self.canvas_configure_timer_armed {
            return;
        }
        self.canvas_configure_timer_armed = true;
        let handle = self.handle.clone();
        let _ = handle.insert_source(
            Timer::from_duration(Duration::from_millis(160)),
            |_, _, state: &mut EvaWM| {
                state.canvas_configure_timer_armed = false;
                state.request_redraw();
                TimeoutAction::Drop
            },
        );
    }

    /// Finish an interactive canvas resize after the final commit or timeout.
    pub(crate) fn finish_canvas_resize(&mut self, window: &Window) {
        if !self.window_states.take_canvas_resizing(window) {
            return;
        }
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.states.unset(
                    smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Resizing,
                );
            });
            toplevel.send_pending_configure();
        }
        self.window_states.clear_suppression(window);
        let output = self
            .window_output_index(window)
            .unwrap_or(self.active_output);
        self.request_redraw_output(output);
    }

    pub(crate) fn schedule_canvas_resize_finish(&mut self, window: Window) {
        let generation = self.window_states.canvas_generation(&window);
        let handle = self.handle.clone();
        let _ = handle.insert_source(
            Timer::from_duration(Duration::from_millis(300)),
            move |_, _, state: &mut EvaWM| {
                state.finish_canvas_resize_if_generation(&window, generation);
                TimeoutAction::Drop
            },
        );
    }

    fn finish_canvas_resize_if_generation(&mut self, window: &Window, generation: u64) {
        if self.window_states.canvas_generation(window) != generation {
            return;
        }
        self.finish_canvas_resize(window);
    }

    /// Rebuild the window -> workspace index. Called after membership changes
    /// (map, close, workspace move/cleanup) rather than every commit.
    pub(crate) fn rebuild_window_locations(&mut self) {
        #[allow(clippy::mutable_key_type)]
        let mut locations = HashMap::new();
        #[allow(clippy::mutable_key_type)]
        let mut surfaces = HashMap::new();
        for (output_index, output) in self.outputs.iter().enumerate() {
            for (workspace_index, workspace) in output.workspaces.iter().enumerate() {
                for window in workspace
                    .scroll
                    .windows()
                    .chain(workspace.canvas.z_order.iter())
                {
                    locations.insert(
                        window.clone(),
                        (output_index, workspace_index, workspace.id),
                    );
                    if let Some(toplevel) = window.toplevel() {
                        surfaces.insert(toplevel.wl_surface().clone(), window.clone());
                    }
                }
            }
        }
        self.window_locations = locations;
        self.surface_windows = surfaces;
    }

    pub(crate) fn workspace_for_window(&self, window: &Window) -> Option<(usize, usize)> {
        workspace_location_in(&self.outputs, &self.window_locations, window)
    }

    pub fn on_window_commit(&mut self, window: &Window) {
        let now = self.animation_now();
        let size = window.geometry().size;
        // A Move drag keeps its own preview mapped until release. Do not let
        // a commit-triggered source layout pass remap the window back to its
        // old tile, on the same output or another one.
        let move_drag_active = self.move_drag_active();
        let (min_size, max_size) = Self::window_constraints(window);

        let found = self.workspace_for_window(window);
        let (output_index, workspace) = found.unwrap_or_else(|| {
            let output_index = self
                .window_output_index(window)
                .unwrap_or(self.active_output);
            // The window may be mapped temporarily on another output (drag
            // preview, closing window). Use that output's active workspace,
            // never the active output's workspace index.
            (output_index, self.outputs[output_index].active_workspace)
        });
        let workspace_active =
            found.is_some() && self.outputs[output_index].active_workspace == workspace;
        let output_active = output_index == self.active_output;

        let drag_active = self.drag.is_some();
        let wall_now = Instant::now();
        let (result, needs_apply, mode, constrained_width, constrained_height) = {
            let output = &mut self.outputs[output_index];
            let workspace_layout = &mut output.workspaces[workspace];
            // Fullscreen configures ignore client constraints, so keep the
            // pre-fullscreen constraints to preserve the scroll/canvas
            // geometry underneath the overlay.
            let constraints_changed = found.is_some()
                && !workspace_layout.is_fullscreen(window)
                && workspace_layout.update_window_constraints(window, min_size, max_size);
            let result = self.window_states.on_commit(
                window,
                size,
                now,
                wall_now,
                CommitContext {
                    animations_off: self.config.animations.off,
                    open: self.config.animations.window_open,
                    resize: self.config.animations.window_resize,
                    window_drag_active: self.drag.is_some(),
                },
            );
            let needs_apply = found.is_some()
                && size.w > 0
                && size.h > 0
                && (result.size_changed || constraints_changed);
            let mode = workspace_layout.mode;
            let constrained_width = canvas_axis_constrained(size.w, min_size.w, max_size.w);
            let constrained_height = canvas_axis_constrained(size.h, min_size.h, max_size.h);
            (
                result,
                needs_apply,
                mode,
                constrained_width,
                constrained_height,
            )
        };
        let constrained_canvas = found.is_some()
            && mode == crate::workspace::Mode::Canvas
            && !drag_active
            && !self.outputs[output_index].workspaces[workspace].is_fullscreen(window)
            && (constrained_width || constrained_height);

        if size.w > 0
            && size.h > 0
            && self
                .window_states
                .note_canvas_commit(window, size, wall_now)
        {
            self.finish_canvas_resize(window);
        }

        if result.resized {
            self.mark_full_redraw_output(output_index);
        }
        if found.is_some() && !workspace_active && result.size_changed {
            // Hidden workspaces do not run a layout apply on commit. Forget
            // the cached configure so showing the workspace reasserts the
            // scroll size if the client ignored the last request.
            self.outputs[output_index].workspaces[workspace]
                .scroll
                .forget_configured_size(window);
        }

        // Re-run layout after a size commit so stacks use the new committed
        // heights immediately (and the next pending configure can go out).
        if workspace_active
            && needs_apply
            && mode == crate::workspace::Mode::Scroll
            && !move_drag_active
        {
            // A committed height can move every following tile in a stack.
            // Capture their current visual rectangles and animate the reflow;
            // the committing window already has its own resize animation.
            let before = (output_active && self.drag.is_none())
                .then(|| self.capture_content_world_rects(now));
            let mut view_config = self.config.animations.view_movement;
            if self.config.animations.off {
                view_config.off = true;
            }
            let allow_refit = self.drag.is_none();
            let mut camera_changed = false;
            {
                let output = &mut self.outputs[output_index];
                let workspace_layout = &mut output.workspaces[workspace];
                workspace_layout.apply_windows(&mut output.space);
                // A new fixed-size client often commits after its initial
                // half-width column was mapped. Refit so the camera only
                // scrolls as far as the committed column actually needs.
                if allow_refit {
                    let old_camera = workspace_layout.scroll.camera_x;
                    workspace_layout.scroll.refit();
                    if (workspace_layout.scroll.camera_x - old_camera).abs() > 0.5 {
                        output.view_x.set_target(
                            workspace_layout.scroll.camera_x,
                            now,
                            view_config,
                        );
                        camera_changed = true;
                    }
                }
                output.map_output();
            }
            if output_active && let Some(before) = before {
                self.start_pending_layout_transitions_except(&before, now, true, Some(window));
            }
            if camera_changed {
                self.mark_full_redraw_output(output_index);
            }
        }
        // Constrained clients can ignore the requested canvas size. Match the
        // canvas rectangle to the committed size on each constrained axis so
        // the border does not surround empty space. A commit at a min/max edge
        // can also be a stale ack of the previous configure (seen with Zen at
        // its minimum size); while a newer canvas configure is pending, wait
        // for its ack instead of shrinking the rect.
        if workspace_active
            && constrained_canvas
            && size.w > 0
            && size.h > 0
            && !self.window_states.canvas_configure_pending(window)
        {
            let updated_size = {
                let output = &mut self.outputs[output_index];
                let rect = output.workspaces[workspace].canvas.rect(window);
                if let Some(rect) = rect {
                    let target = Size::from((
                        if constrained_width {
                            f64::from(size.w)
                        } else {
                            rect.size.w
                        },
                        if constrained_height {
                            f64::from(size.h)
                        } else {
                            rect.size.h
                        },
                    ));
                    let changed = (rect.size.w - target.w).abs() > 0.5
                        || (rect.size.h - target.h).abs() > 0.5;
                    if changed {
                        let center = rect.loc + Point::from((rect.size.w / 2.0, rect.size.h / 2.0));
                        let loc = center - Point::from((target.w / 2.0, target.h / 2.0));
                        let updated = Rectangle::new(loc, target);
                        output.workspaces[workspace]
                            .canvas
                            .set_rect(window, updated);
                        output.workspaces[workspace].apply_windows(&mut output.space);
                        output.map_output();
                        Some(crate::workspace::canvas_pixel_size(updated))
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some(target_size) = updated_size {
                self.configure_canvas_window(window, target_size);
                self.mark_full_redraw_output(output_index);
            }
        }

        self.request_redraw_output(output_index);
        self.refresh_foreign_toplevel_window(window);
    }

    pub fn remove_window_animation(&mut self, window: &Window) {
        self.window_states.remove(window);
    }

    #[cfg(feature = "nested")]
    pub fn resize_output(&mut self, size: Size<i32, Physical>) {
        self.output_state_mut().resize_output(size);
        // The layer exclusive zone may have been calculated for the old
        // output size; refresh it before laying out tiles.
        self.output_state_mut().update_working_area();
        self.apply_layout();
        // The layout pass skips the fullscreen tile, so reassert the overlay
        // at the new output size after a mode change.
        if let Some(window) = self.active_fullscreen().cloned() {
            self.configure_fullscreen_overlay(&window);
        }
        let output = self.active_output;
        self.mark_full_redraw_output(output);
    }

    pub fn update_working_area_output(&mut self, output_index: usize) {
        let now = self.animation_now();
        let config = self.config.animations.view_movement;
        let animations_off = self.config.animations.off;
        let move_drag_active = self.move_drag_active();

        let Some(output) = self.outputs.get_mut(output_index) else {
            return;
        };
        let active = output.active_workspace;
        let before = output.workspaces[active].scroll.working_area;
        output.update_working_area();
        let changed = output.workspaces[active].scroll.working_area != before;

        if !move_drag_active && changed {
            // A layer commit only needs a redraw unless its exclusive zone
            // changed. Avoid reconfiguring every window on ordinary bar ticks.
            output.apply_layout(now, config, animations_off);
        }
        self.request_redraw_output(output_index);
    }

    pub fn change_vt(&mut self, vt: i32) {
        if !self.is_session() {
            return;
        }
        // Leave `session_paused` to libseat's PauseSession event (or the
        // `!session.is_active()` guard in `render_output`) so switching to the
        // current VT does not stall rendering and outgoing key releases do not
        // race the asynchronous VT switch.
        self.cancel_interaction_state();
        if let Some(session) = self.session_mut() {
            let _ = session.change_vt(vt);
        }
    }

    fn current_content_world_rect(
        &self,
        window: &Window,
        now: f64,
    ) -> Option<Rectangle<f64, Logical>> {
        self.output_state()
            .current_content_world_rect(window, now, &self.window_states)
    }

    pub(crate) fn capture_content_world_rects(
        &self,
        now: f64,
    ) -> Vec<(Window, Rectangle<f64, Logical>)> {
        let output = self.output_state();
        output.workspaces[output.active_workspace]
            .windows()
            .filter_map(|window| {
                self.current_content_world_rect(window, now)
                    .map(|rect| (window.clone(), rect))
            })
            .collect()
    }

    /// Visit surfaces on this output that should receive frame callbacks
    /// without allocating a temporary surface list on every frame.
    ///
    /// This mirrors the visibility rules used for rendering: windows that do
    /// not overlap the active viewport are skipped, and a fullscreen window
    /// always receives callbacks.
    pub(crate) fn for_each_frame_callback_surface(
        &self,
        output_index: usize,
        mut f: impl FnMut(&WlSurface),
    ) {
        let Some(output_state) = self.outputs.get(output_index) else {
            return;
        };
        let active_workspace = output_state.active_workspace;
        let viewport = output_state.visible_world_viewport();
        let fullscreen = output_state.workspaces[active_workspace].fullscreen_window();

        for window in output_state.space.elements() {
            if fullscreen != Some(window) {
                let Some(location) = output_state.space.element_location(window) else {
                    continue;
                };
                let size = window.geometry().size;
                if size.w <= 0 || size.h <= 0 {
                    continue;
                }
                let rect = smithay::utils::Rectangle::new(location.to_f64(), size.to_f64());
                if !rect.overlaps(viewport) {
                    continue;
                }
            }
            let Some(surface) = window.wl_surface() else {
                continue;
            };
            for (popup, _) in PopupManager::popups_for_surface(&surface) {
                f(popup.wl_surface());
            }
            f(&surface);
        }

        let map = layer_map_for_output(&output_state.output);
        for layer in map.layers() {
            let surface = layer.wl_surface();
            for (popup, _) in PopupManager::popups_for_surface(surface) {
                f(popup.wl_surface());
            }
            f(surface);
        }

        if self.pointer_output == output_index {
            if let Some(icon) = &self.dnd_icon {
                f(&icon.surface);
            }
            if let CursorImageStatus::Surface(surface) = &self.cursor_status
                && surface.alive()
            {
                f(surface);
            }
        }
    }

    fn surface_has_pending_frame_callbacks(surface: &WlSurface) -> bool {
        let pending = std::cell::Cell::new(false);
        with_surface_tree_downward(
            surface,
            (),
            |_, _, _| {
                if pending.get() {
                    TraversalAction::SkipChildren
                } else {
                    TraversalAction::DoChildren(())
                }
            },
            |_, states, _| {
                if !pending.get()
                    && !states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .frame_callbacks
                        .is_empty()
                {
                    pending.set(true);
                }
            },
            |_, _, _| !pending.get(),
        );
        pending.get()
    }

    fn has_pending_frame_callbacks(&self, output_index: usize) -> bool {
        let mut pending = false;
        self.for_each_frame_callback_surface(output_index, |surface| {
            if !pending && Self::surface_has_pending_frame_callbacks(surface) {
                pending = true;
            }
        });
        pending
    }

    fn frame_callback_interval(&self, output_index: usize) -> Duration {
        let refresh = self
            .outputs
            .get(output_index)
            .and_then(|output| output.output.current_mode())
            .map(|mode| mode.refresh)
            .unwrap_or(60_000);
        if refresh <= 0 {
            return Duration::from_millis(16);
        }
        Duration::from_secs_f64(1000.0 / f64::from(refresh))
            .clamp(Duration::from_millis(1), Duration::from_millis(100))
    }

    /// Deliver frame callbacks after a render.
    ///
    /// A presented frame always delivers callbacks. A skipped frame still has
    /// to deliver them, but paced to the output refresh rate: clients such as
    /// Firefox drive their own vsync from a surface that produces no damage,
    /// and starving them stalls the whole browser. Pacing avoids the busy loop
    /// that unconditional delivery would create.
    pub fn handle_frame_callbacks(&mut self, output_index: usize, presented: bool) {
        let now = Instant::now();
        let interval = self.frame_callback_interval(output_index);

        if presented {
            self.send_frames(output_index);
            if let Some(output) = self.outputs.get_mut(output_index) {
                output.last_present = Some(now);
                output.last_frame_callback = Some(now);
            }
            return;
        }

        if !self.has_pending_frame_callbacks(output_index) {
            return;
        }

        // While presentation is flowing, the presented frame owns callbacks.
        // Wake after the refresh anyway so a stalled client still gets a
        // fallback once presentation stops.
        let since_present = self
            .outputs
            .get(output_index)
            .and_then(|output| output.last_present)
            .map_or(interval, |last| now.saturating_duration_since(last));
        if since_present < interval {
            self.schedule_frame_callback_tick(output_index, interval - since_present);
            return;
        }

        // Presentation has stopped for a full refresh. Deliver at most one
        // fallback per refresh while a callback remains pending.
        let since_callback = self
            .outputs
            .get(output_index)
            .and_then(|output| output.last_frame_callback)
            .map_or(interval, |last| now.saturating_duration_since(last));
        if since_callback < interval {
            self.schedule_frame_callback_tick(output_index, interval - since_callback);
            return;
        }

        self.send_frames(output_index);
        if let Some(output) = self.outputs.get_mut(output_index) {
            output.last_frame_callback = Some(now);
        }
    }

    fn schedule_frame_callback_tick(&mut self, output_index: usize, delay: Duration) {
        let Some(output) = self.outputs.get_mut(output_index) else {
            return;
        };
        if output.frame_callback_tick {
            return;
        }
        output.frame_callback_tick = true;
        let handle = self.handle.clone();
        let _ = handle.insert_source(
            Timer::from_duration(delay.max(Duration::from_millis(1))),
            move |_, _, state: &mut EvaWM| {
                if let Some(output) = state.outputs.get_mut(output_index) {
                    output.frame_callback_tick = false;
                }
                if state.has_pending_frame_callbacks(output_index) {
                    state.request_redraw_output(output_index);
                }
                TimeoutAction::Drop
            },
        );
    }

    pub fn send_frames(&self, output_index: usize) {
        let Some(output_state) = self.outputs.get(output_index) else {
            return;
        };
        let time = self.start_time.elapsed();
        let output = &output_state.output;
        self.for_each_frame_callback_surface(output_index, |surface| {
            send_frames_surface_tree(surface, output, time, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        });
    }
}

/// Delay before the next render retry. Starts short for transient DRM
/// failures and caps at 2 s for a persistent recovery probe.
fn render_retry_delay(retries: u8) -> Duration {
    let shift = u32::from(retries.min(5));
    Duration::from_millis((100u64 << shift).min(2_000))
}

/// True when a committed client size is actually sitting on a min/max
/// constraint. A large max size (Firefox/Zen advertises 16344x16344) must not
/// be treated as constrained at ordinary window sizes.
fn canvas_axis_constrained(size: i32, min: i32, max: i32) -> bool {
    let size = f64::from(size);
    (max > 0 && size >= f64::from(max) - 1.0) || (min > 0 && size <= f64::from(min) + 1.0)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{canvas_axis_constrained, render_retry_delay};

    #[test]
    fn render_retry_delay_backs_off_and_caps() {
        assert_eq!(render_retry_delay(0), Duration::from_millis(100));
        assert_eq!(render_retry_delay(1), Duration::from_millis(200));
        assert_eq!(render_retry_delay(2), Duration::from_millis(400));
        assert_eq!(render_retry_delay(3), Duration::from_millis(800));
        assert_eq!(render_retry_delay(4), Duration::from_millis(1_600));
        assert_eq!(render_retry_delay(5), Duration::from_millis(2_000));
        assert_eq!(render_retry_delay(u8::MAX), Duration::from_millis(2_000));
    }

    #[test]
    fn huge_max_sizes_are_not_treated_as_constrained() {
        assert!(!canvas_axis_constrained(918, 500, 16344));
        assert!(!canvas_axis_constrained(1870, 500, 16344));
    }

    #[test]
    fn real_min_and_max_hits_are_constrained() {
        assert!(canvas_axis_constrained(500, 500, 0));
        assert!(canvas_axis_constrained(500, 500, 16344));
        assert!(canvas_axis_constrained(400, 0, 400));
        assert!(!canvas_axis_constrained(398, 0, 400));
    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
