//! XWayland support through the external `xwayland-satellite` helper.
//!
//! EvaWM pre-binds the X11 sockets and display lock, then waits for the first
//! X11 client before spawning `xwayland-satellite`. The satellite inherits the
//! listening sockets with `-listenfd`, so the connection that triggered
//! activation is served normally. If the satellite exits, the listeners are
//! re-armed and the next X11 connection starts a fresh satellite.
//!
//! The satellite owns the X server and translates X11 windows into ordinary
//! Wayland toplevels, so EvaWM's existing scroll/canvas/workspace/window code
//! needs no X11-specific path. EvaWM only chooses a free X display and exports
//! `DISPLAY` to the session environment.

use std::{
    ffi::OsStr,
    fs::{DirBuilder, OpenOptions},
    io::{self, Write},
    os::{
        fd::{AsRawFd, BorrowedFd},
        linux::net::SocketAddrExt,
        unix::{
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{SocketAddr, UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

use smithay::reexports::{
    calloop::{
        Interest, LoopHandle, Mode, PostAction, RegistrationToken,
        generic::Generic,
        timer::{TimeoutAction, Timer},
    },
    rustix::{
        io::{FdFlags, fcntl_setfd},
        process::{Pid, PidfdFlags, getuid, pidfd_open},
    },
};

use crate::config::XwaylandConfig;
use crate::state::EvaWM;

const PROGRAM: &str = "xwayland-satellite";
const MAX_DISPLAY: u32 = 32;
const TMP_DIR: &str = "/tmp";
const X11_TMP_UNIX_DIR: &str = "/tmp/.X11-unix";

pub struct XwaylandServer {
    pub display: String,
    unix_listener: UnixListener,
    abstract_listener: Option<UnixListener>,
    /// The on-demand satellite, absent while no X11 client is running.
    child: Option<Child>,
    unix_token: Option<RegistrationToken>,
    abstract_token: Option<RegistrationToken>,
    /// Event-loop watcher for the live satellite process (pidfd or fallback timer).
    child_token: Option<RegistrationToken>,
    _unix_guard: Unlink,
    _lock_guard: Unlink,
}

impl XwaylandServer {
    /// Pre-bind the X11 sockets on the first free display near
    /// `wayland_socket`. The satellite is not spawned until a client connects.
    pub fn start(config: XwaylandConfig, wayland_socket: &OsStr) -> Option<Self> {
        if !config.enabled {
            tracing::info!("xwayland disabled by config");
            return None;
        }
        if !probe_satellite() {
            return None;
        }
        if let Err(err) = ensure_x11_unix_dir() {
            tracing::warn!("cannot prepare the X11 socket directory: {err}");
            return None;
        }

        let preferred = preferred_display(wayland_socket);
        for number in candidate_displays(preferred) {
            match Self::bind(number) {
                Ok(server) => {
                    tracing::info!(
                        display = %server.display,
                        "xwayland listeners ready (starting on demand)"
                    );
                    return Some(server);
                }
                Err(err) => {
                    tracing::debug!(display = number, "cannot use X11 display: {err}");
                }
            }
        }

        tracing::warn!("no free X11 display for xwayland-satellite");
        None
    }

    fn bind(number: u32) -> io::Result<Self> {
        let lock_path = lock_path(number);
        acquire_lock(&lock_path, number)?;
        let lock_guard = Unlink(lock_path);

        // The abstract socket disappears when its owner exits, so an existing
        // one means a live X server even if its lock file is stale.
        let abstract_listener = bind_abstract(number)?;

        let unix_path = socket_path(number);
        let unix_guard = Unlink(unix_path.clone());
        // We hold the display lock, so a leftover socket file is stale.
        let _ = std::fs::remove_file(&unix_path);
        let unix_listener = UnixListener::bind(&unix_path)?;

        Ok(Self {
            display: format!(":{number}"),
            unix_listener,
            abstract_listener: Some(abstract_listener),
            child: None,
            unix_token: None,
            abstract_token: None,
            child_token: None,
            _unix_guard: unix_guard,
            _lock_guard: lock_guard,
        })
    }

    fn is_running(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
    }

    /// Register both X11 listeners with the event loop. Pending connections
    /// from a previous failed satellite are discarded first: they would
    /// otherwise keep a level-triggered listener permanently ready.
    fn arm(&mut self, handle: &LoopHandle<'static, EvaWM>) -> io::Result<()> {
        self.disarm_listeners(handle);

        let result = (|| -> io::Result<()> {
            let listener = self.unix_listener.try_clone()?;
            clear_pending(&listener)?;
            let token = handle
                .insert_source(
                    Generic::new(listener, Interest::READ, Mode::Level),
                    |_, _, state: &mut EvaWM| Ok(on_connection(state, ListenerKind::Unix)),
                )
                .map_err(io::Error::other)?;
            self.unix_token = Some(token);

            let Some(abstract_listener) = &self.abstract_listener else {
                return Ok(());
            };
            let listener = abstract_listener.try_clone()?;
            clear_pending(&listener)?;
            let token = handle
                .insert_source(
                    Generic::new(listener, Interest::READ, Mode::Level),
                    |_, _, state: &mut EvaWM| Ok(on_connection(state, ListenerKind::Abstract)),
                )
                .map_err(io::Error::other)?;
            self.abstract_token = Some(token);
            Ok(())
        })();

        if result.is_err() {
            self.disarm_listeners(handle);
        }
        result
    }

    fn disarm_listeners(&mut self, handle: &LoopHandle<'static, EvaWM>) {
        if let Some(token) = self.abstract_token.take() {
            handle.remove(token);
        }
        if let Some(token) = self.unix_token.take() {
            handle.remove(token);
        }
    }

    fn disarm(&mut self, handle: &LoopHandle<'static, EvaWM>) {
        self.disarm_listeners(handle);
        if let Some(token) = self.child_token.take() {
            handle.remove(token);
        }
    }

    fn watch_child(&mut self, handle: &LoopHandle<'static, EvaWM>, pid: u32) {
        if let Some(token) = self.child_token.take() {
            handle.remove(token);
        }
        if let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw)
            && let Ok(pidfd) = pidfd_open(pid, PidfdFlags::NONBLOCK)
            && let Ok(token) = handle.insert_source(
                Generic::new(pidfd, Interest::READ, Mode::Level),
                |_, _, state: &mut EvaWM| {
                    state.on_xwayland_exit();
                    Ok(PostAction::Remove)
                },
            )
        {
            self.child_token = Some(token);
            return;
        }
        // Fallback when pidfd_open is unavailable: poll only while the child runs.
        if let Ok(token) = handle.insert_source(
            Timer::from_duration(Duration::from_secs(2)),
            |_, _, state: &mut EvaWM| {
                if state.poll_xwayland_child() {
                    TimeoutAction::ToDuration(Duration::from_secs(2))
                } else {
                    TimeoutAction::Drop
                }
            },
        ) {
            self.child_token = Some(token);
        }
    }

    /// Hand the pre-bound listeners to a fresh satellite. Returns `false` if the
    /// satellite failed to start and listeners could not be re-armed.
    fn spawn(&mut self, handle: &LoopHandle<'static, EvaWM>, wayland_socket: &OsStr) -> bool {
        let unix_fd = match self.unix_listener.try_clone() {
            Ok(fd) => fd,
            Err(err) => {
                tracing::warn!("failed to clone the X11 unix socket: {err}");
                if let Err(err) = self.arm(handle) {
                    tracing::error!("failed to re-arm X11 listeners: {err}");
                    return false;
                }
                return true;
            }
        };
        let mut drop_abstract = false;
        let abstract_fd = match self.abstract_listener.as_ref() {
            Some(listener) => match listener.try_clone() {
                Ok(fd) => Some(fd),
                Err(err) => {
                    tracing::warn!(
                        "failed to clone the X11 abstract socket: {err}; \
                         continuing Unix-only"
                    );
                    drop_abstract = true;
                    None
                }
            },
            None => None,
        };
        if drop_abstract {
            // Never leave a bound abstract listener unwatched: clients would
            // connect and hang. Dropping it makes them fall back or fail fast.
            self.abstract_listener = None;
        }

        let unix_raw = unix_fd.as_raw_fd();
        let abstract_raw = abstract_fd.as_ref().map(AsRawFd::as_raw_fd);

        let mut command = Command::new(PROGRAM);
        command
            .arg(&self.display)
            .arg("-listenfd")
            .arg(unix_raw.to_string())
            .env("WAYLAND_DISPLAY", wayland_socket)
            .env_remove("DISPLAY")
            .env_remove("RUST_BACKTRACE")
            .env_remove("RUST_LIB_BACKTRACE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(raw) = abstract_raw {
            command.arg("-listenfd").arg(raw.to_string());
        }

        // SAFETY: the closure only clears CLOEXEC on descriptors that this
        // function keeps open until `spawn` returns.
        unsafe {
            command.pre_exec(move || {
                fcntl_setfd(BorrowedFd::borrow_raw(unix_raw), FdFlags::empty())?;
                if let Some(raw) = abstract_raw {
                    fcntl_setfd(BorrowedFd::borrow_raw(raw), FdFlags::empty())?;
                }
                Ok(())
            });
        }

        match command.spawn() {
            Ok(child) => {
                let pid = child.id();
                tracing::info!(
                    display = %self.display,
                    pid,
                    "xwayland-satellite spawned on demand"
                );
                self.child = Some(child);
                self.watch_child(handle, pid);
                true
            }
            Err(err) => {
                tracing::warn!(
                    display = %self.display,
                    "failed to start xwayland-satellite: {err}; X11 applications will not run"
                );
                // Discard the connection that triggered this attempt so the
                // level-triggered listener cannot spin while spawn is broken.
                if let Err(err) = self.arm(handle) {
                    tracing::error!("failed to re-arm X11 listeners: {err}");
                    return false;
                }
                true
            }
        }
    }
}

impl Drop for XwaylandServer {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

#[derive(Clone, Copy)]
enum ListenerKind {
    Unix,
    Abstract,
}

fn on_connection(state: &mut EvaWM, kind: ListenerKind) -> PostAction {
    let handle = state.handle.clone();
    let wayland_socket = state.socket_name.clone();
    let teardown = {
        let Some(server) = &mut state.xwayland else {
            return PostAction::Remove;
        };

        match kind {
            ListenerKind::Unix => {
                server.unix_token = None;
                if let Some(token) = server.abstract_token.take() {
                    handle.remove(token);
                }
            }
            ListenerKind::Abstract => {
                server.abstract_token = None;
                if let Some(token) = server.unix_token.take() {
                    handle.remove(token);
                }
            }
        }

        if !server.is_running() {
            server.child = None;
            tracing::info!(
                display = %server.display,
                "X11 connection; starting xwayland-satellite"
            );
            !server.spawn(&handle, wayland_socket.as_os_str())
        } else {
            false
        }
    };
    if teardown {
        state.teardown_failed_xwayland();
    }
    PostAction::Remove
}

impl EvaWM {
    /// Prepare the on-demand X11 listeners if configured. Returns the current
    /// `DISPLAY` value; it is available before the satellite starts.
    pub fn start_xwayland(&mut self) -> Option<String> {
        if let Some(server) = &self.xwayland {
            return Some(server.display.clone());
        }

        if !self.config.xwayland.enabled {
            return None;
        }

        let mut server = XwaylandServer::start(self.config.xwayland, self.socket_name.as_os_str());
        let handle = self.handle.clone();
        if let Some(candidate) = &mut server
            && let Err(err) = candidate.arm(&handle)
        {
            tracing::error!("failed to arm X11 listeners: {err}");
            candidate.disarm(&handle);
            server = None;
        }
        self.xwayland = server;
        self.xwayland.as_ref().map(|server| server.display.clone())
    }

    /// Stop XWayland and clear `DISPLAY`. Running X11 clients are closed with
    /// their X server.
    pub fn stop_xwayland(&mut self) {
        if let Some(server) = &mut self.xwayland {
            let handle = self.handle.clone();
            server.disarm(&handle);
        }
        self.xwayland = None;
    }

    pub fn x11_display(&self) -> Option<&str> {
        self.xwayland.as_ref().map(|server| server.display.as_str())
    }

    fn teardown_failed_xwayland(&mut self) {
        // Never leave DISPLAY pointing at listeners that are not watched.
        self.stop_xwayland();
        if self.is_session() {
            crate::session_env::update_display_environment(None);
        }
    }

    fn on_xwayland_exit(&mut self) {
        let handle = self.handle.clone();
        let rearm_failed = {
            let Some(server) = &mut self.xwayland else {
                return;
            };
            server.child_token = None;
            let Some(mut child) = server.child.take() else {
                return;
            };
            match child.wait() {
                Ok(status) => tracing::warn!(
                    display = %server.display,
                    %status,
                    "xwayland-satellite exited; re-arming X11 listeners"
                ),
                Err(err) => tracing::warn!(
                    display = %server.display,
                    "failed to wait on xwayland-satellite: {err}; re-arming X11 listeners"
                ),
            }
            if let Err(err) = server.arm(&handle) {
                tracing::error!("failed to re-arm X11 listeners: {err}");
                true
            } else {
                false
            }
        };
        if rearm_failed {
            self.teardown_failed_xwayland();
        }
    }

    fn poll_xwayland_child(&mut self) -> bool {
        let handle = self.handle.clone();
        let (still_running, rearm_failed) = {
            let Some(server) = &mut self.xwayland else {
                return false;
            };
            let Some(child) = &mut server.child else {
                server.child_token = None;
                return false;
            };
            match child.try_wait() {
                Ok(None) => (true, false),
                Ok(Some(status)) => {
                    tracing::warn!(
                        display = %server.display,
                        %status,
                        "xwayland-satellite exited; re-arming X11 listeners"
                    );
                    server.child = None;
                    server.child_token = None;
                    let failed = server
                        .arm(&handle)
                        .inspect_err(|err| tracing::error!("failed to re-arm X11 listeners: {err}"))
                        .is_err();
                    (false, failed)
                }
                Err(err) => {
                    tracing::warn!(
                        display = %server.display,
                        "failed to check xwayland-satellite: {err}; re-arming X11 listeners"
                    );
                    server.child = None;
                    server.child_token = None;
                    let failed = server
                        .arm(&handle)
                        .inspect_err(|err| tracing::error!("failed to re-arm X11 listeners: {err}"))
                        .is_err();
                    (false, failed)
                }
            }
        };
        if rearm_failed {
            self.teardown_failed_xwayland();
        }
        still_running
    }
}

struct Unlink(PathBuf);

impl Drop for Unlink {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// Adapted from Mutter: /tmp/.X11-unix must be root- or user-owned, writable,
// and sticky before we put an X11 socket in it.
fn ensure_x11_unix_dir() -> io::Result<()> {
    match DirBuilder::new().mode(0o1777).create(X11_TMP_UNIX_DIR) {
        Ok(()) => {
            let mut permissions = std::fs::metadata(X11_TMP_UNIX_DIR)?.permissions();
            permissions.set_mode(0o1777);
            std::fs::set_permissions(X11_TMP_UNIX_DIR, permissions)
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => ensure_x11_unix_perms(),
        Err(err) => Err(err),
    }
}

fn ensure_x11_unix_perms() -> io::Result<()> {
    let x11 = std::fs::symlink_metadata(X11_TMP_UNIX_DIR)?;
    let tmp = std::fs::metadata(TMP_DIR)?;
    let uid = getuid().as_raw();

    if !x11.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the X11 socket path is not a directory",
        ));
    }
    if x11.uid() != tmp.uid() && x11.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "wrong ownership for the X11 directory",
        ));
    }
    if x11.mode() & 0o022 != 0o022 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the X11 directory is not writable",
        ));
    }
    if x11.mode() & 0o1000 != 0o1000 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the X11 directory is missing the sticky bit",
        ));
    }
    Ok(())
}

fn acquire_lock(path: &Path, number: u32) -> io::Result<()> {
    for attempt in 0..2 {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o444)
            .open(path)
        {
            Ok(mut file) => {
                if let Err(err) = writeln!(file, "{:>10}", std::process::id()) {
                    let _ = std::fs::remove_file(path);
                    return Err(err);
                }
                return Ok(());
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists && attempt == 0 => {
                if stale_lock(path, number) {
                    std::fs::remove_file(path)?;
                    continue;
                }
                return Err(err);
            }
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "display lock is busy",
    ))
}

fn stale_lock(path: &Path, number: u32) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    let trimmed = contents.trim();
    if let Ok(pid) = trimmed.parse::<u32>() {
        if pid == 0 {
            return !display_socket_reachable(number);
        }
        return !Path::new("/proc").join(pid.to_string()).exists();
    }

    // An empty or malformed lock is only stale when no X11 socket can be
    // reached. This avoids stealing a display while a live server is starting.
    !display_socket_reachable(number)
}

fn display_socket_reachable(number: u32) -> bool {
    if UnixStream::connect(socket_path(number)).is_ok() {
        return true;
    }
    let name = format!("/tmp/.X11-unix/X{number}");
    SocketAddr::from_abstract_name(name)
        .ok()
        .and_then(|addr| UnixStream::connect_addr(&addr).ok())
        .is_some()
}

/// Probe support before promising `DISPLAY` to clients.
fn probe_satellite() -> bool {
    let mut command = Command::new(PROGRAM);
    command
        .args([":0", "--test-listenfd-support"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("DISPLAY")
        .env_remove("RUST_BACKTRACE")
        .env_remove("RUST_LIB_BACKTRACE");

    match command.status() {
        Ok(status) if status.success() => true,
        Ok(_) => {
            tracing::warn!(
                "xwayland-satellite does not support on-demand activation; \
                 X11 applications will not run"
            );
            false
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            tracing::warn!("xwayland-satellite not found; X11 applications will not run");
            false
        }
        Err(err) => {
            tracing::warn!("failed to probe xwayland-satellite: {err}");
            false
        }
    }
}

fn clear_pending(listener: &UnixListener) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let mut accept_error = None;
    loop {
        match listener.accept() {
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) => {
                accept_error = Some(err);
                break;
            }
        }
    }
    listener.set_nonblocking(false)?;
    match accept_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn bind_abstract(number: u32) -> io::Result<UnixListener> {
    let name = format!("/tmp/.X11-unix/X{number}");
    let addr = SocketAddr::from_abstract_name(name)?;
    UnixListener::bind_addr(&addr)
}

fn preferred_display(wayland_socket: &OsStr) -> u32 {
    wayland_socket
        .to_string_lossy()
        .rsplit('-')
        .next()
        .and_then(|suffix| suffix.parse::<u32>().ok())
        .filter(|number| *number < MAX_DISPLAY)
        .unwrap_or(0)
}

fn candidate_displays(preferred: u32) -> impl Iterator<Item = u32> {
    (0..MAX_DISPLAY).map(move |offset| (preferred + offset) % MAX_DISPLAY)
}

fn socket_path(number: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/.X11-unix/X{number}"))
}

fn lock_path(number: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/.X{number}-lock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_number_follows_the_wayland_socket() {
        assert_eq!(preferred_display(OsStr::new("wayland-1")), 1);
        assert_eq!(preferred_display(OsStr::new("wayland-12")), 12);
        assert_eq!(preferred_display(OsStr::new("custom")), 0);
        assert_eq!(preferred_display(OsStr::new("wayland-99")), 0);
    }

    #[test]
    fn live_locks_are_kept_and_stale_locks_are_reclaimed() {
        let dir =
            std::env::temp_dir().join(format!("evawm-xwayland-lock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create test dir");

        let live = dir.join(".X98-lock");
        std::fs::write(&live, format!("{:>10}\n", std::process::id())).expect("write live lock");
        assert!(
            acquire_lock(&live, 98).is_err(),
            "a live PID must keep its lock"
        );

        let stale = dir.join(".X99-lock");
        std::fs::write(&stale, "4294967295\n").expect("write stale lock");
        acquire_lock(&stale, 99).expect("reclaim stale lock");
        assert_eq!(
            std::fs::read_to_string(&stale).unwrap().trim(),
            std::process::id().to_string()
        );

        std::fs::remove_dir_all(&dir).expect("remove test dir");
    }

    #[test]
    fn empty_and_malformed_locks_are_reclaimed_without_a_live_socket() {
        let dir = std::env::temp_dir().join(format!(
            "evawm-xwayland-corrupt-lock-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create test dir");

        let empty = dir.join(".X997-lock");
        std::fs::write(&empty, "").expect("write empty lock");
        acquire_lock(&empty, 997).expect("reclaim empty lock");

        let malformed = dir.join(".X996-lock");
        std::fs::write(&malformed, "not-a-pid\n").expect("write malformed lock");
        acquire_lock(&malformed, 996).expect("reclaim malformed lock");

        std::fs::remove_dir_all(&dir).expect("remove test dir");
    }

    #[test]
    fn candidate_displays_wrap_without_duplicates() {
        let displays: Vec<u32> = candidate_displays(3).collect();
        assert_eq!(displays.len(), MAX_DISPLAY as usize);
        assert_eq!(displays[0], 3);
        assert_eq!(displays[1], 4);
        assert_eq!(displays[MAX_DISPLAY as usize - 1], 2);
        let mut unique = displays.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), displays.len());
    }
}
