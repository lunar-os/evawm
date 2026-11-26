//! Systemd user environment, D-Bus activation environment, and portal service
//! lifecycle for real DRM sessions.

use std::{os::unix::process::CommandExt, process::Stdio};

struct PreviousSessionEnv {
    wayland_display: Option<String>,
    current_desktop: Option<String>,
    session_type: Option<String>,
    x11_display: Option<String>,
    evawm_socket: Option<String>,
}

/// Restores the previous systemd/D-Bus session environment on drop if EvaWM
/// updated it during startup.
pub(crate) struct SessionEnvGuard {
    env: PreviousSessionEnv,
    applied: bool,
}

impl SessionEnvGuard {
    pub(crate) fn capture() -> Self {
        Self {
            env: PreviousSessionEnv::capture(),
            applied: false,
        }
    }

    pub(crate) fn apply(
        &mut self,
        wayland_display: &str,
        current_desktop: &str,
        session_type: &str,
        x11_display: Option<&str>,
        evawm_socket: Option<&str>,
    ) {
        apply_session_environment(
            wayland_display,
            Some(current_desktop),
            Some(session_type),
            x11_display,
            evawm_socket,
        );
        self.applied = true;
    }
}

impl Drop for SessionEnvGuard {
    fn drop(&mut self) {
        if self.applied {
            self.env.restore();
        }
    }
}

fn systemd_environment() -> Option<String> {
    let output = std::process::Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn systemd_env_var(environment: &str, name: &str) -> Option<String> {
    environment.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key == name).then(|| value.to_owned())
    })
}

impl PreviousSessionEnv {
    fn capture() -> Self {
        // The portal services inherit the systemd user environment, which can
        // still describe the compositor running on another VT even when EvaWM
        // was launched from a bare TTY. Prefer that over our process
        // environment, then fall back to the process environment.
        let systemd = systemd_environment();
        let get = |name: &str| {
            systemd
                .as_deref()
                .and_then(|environment| systemd_env_var(environment, name))
                .or_else(|| std::env::var(name).ok())
        };
        Self {
            wayland_display: get("WAYLAND_DISPLAY"),
            current_desktop: get("XDG_CURRENT_DESKTOP"),
            session_type: get("XDG_SESSION_TYPE"),
            x11_display: get("DISPLAY"),
            evawm_socket: get("EVAWM_SOCKET"),
        }
    }

    fn restore(&self) {
        if let Some(display) = self.wayland_display.as_deref() {
            apply_session_environment(
                display,
                self.current_desktop.as_deref(),
                self.session_type.as_deref(),
                self.x11_display.as_deref(),
                self.evawm_socket.as_deref(),
            );
        } else {
            let _ = std::process::Command::new("systemctl")
                .args([
                    "--user",
                    "unset-environment",
                    "WAYLAND_DISPLAY",
                    "DISPLAY",
                    "XDG_CURRENT_DESKTOP",
                    "XDG_SESSION_TYPE",
                    "EVAWM_SOCKET",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            stop_portal_services();
        }
    }
}

fn apply_session_environment(
    wayland_display: &str,
    current_desktop: Option<&str>,
    session_type: Option<&str>,
    x11_display: Option<&str>,
    evawm_socket: Option<&str>,
) {
    let mut systemd_args = vec![
        "--user".to_string(),
        "set-environment".to_string(),
        format!("WAYLAND_DISPLAY={wayland_display}"),
    ];
    let mut unset_vars = Vec::new();
    let mut dbus_cmd = std::process::Command::new("dbus-update-activation-environment");
    dbus_cmd
        .args(["--systemd", "WAYLAND_DISPLAY"])
        .env("WAYLAND_DISPLAY", wayland_display)
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    if let Some(desktop) = current_desktop {
        systemd_args.push(format!("XDG_CURRENT_DESKTOP={desktop}"));
        dbus_cmd
            .arg("XDG_CURRENT_DESKTOP")
            .env("XDG_CURRENT_DESKTOP", desktop);
    } else {
        unset_vars.push("XDG_CURRENT_DESKTOP");
    }
    if let Some(session_type) = session_type {
        systemd_args.push(format!("XDG_SESSION_TYPE={session_type}"));
        dbus_cmd
            .arg("XDG_SESSION_TYPE")
            .env("XDG_SESSION_TYPE", session_type);
    } else {
        unset_vars.push("XDG_SESSION_TYPE");
    }
    if let Some(display) = x11_display {
        systemd_args.push(format!("DISPLAY={display}"));
        dbus_cmd.arg("DISPLAY").env("DISPLAY", display);
    } else {
        unset_vars.push("DISPLAY");
    }
    if let Some(socket) = evawm_socket {
        systemd_args.push(format!("EVAWM_SOCKET={socket}"));
        dbus_cmd.arg("EVAWM_SOCKET").env("EVAWM_SOCKET", socket);
    } else {
        unset_vars.push("EVAWM_SOCKET");
    }

    let _ = std::process::Command::new("systemctl")
        .args(&systemd_args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !unset_vars.is_empty() {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "unset-environment"])
            .args(&unset_vars)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = dbus_cmd.status();
    restart_portal_services();
}

/// Update only `DISPLAY` in the activation environments after a live XWayland
/// toggle. Runs on a background thread so `systemctl`/D-Bus cannot stall the
/// compositor event loop. Portals are unaffected, so do not restart them.
pub(crate) fn update_display_environment(x11_display: Option<&str>) {
    let display = x11_display.map(str::to_owned);
    let spawned = std::thread::Builder::new()
        .name("evawm env sync".into())
        .spawn(move || match display.as_deref() {
            Some(display) => {
                let _ = std::process::Command::new("systemctl")
                    .args(["--user", "set-environment", &format!("DISPLAY={display}")])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let _ = std::process::Command::new("dbus-update-activation-environment")
                    .args(["--systemd", "DISPLAY"])
                    .env("DISPLAY", display)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            None => unset_systemd_display(),
        });
    if let Err(err) = spawned {
        tracing::warn!("failed to spawn display environment sync thread: {err}");
    }
}

fn unset_systemd_display() {
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "unset-environment", "DISPLAY"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn restart_portal_services() {
    // Do this outside the compositor event loop. A portal frontend with an
    // active ScreenCast session can otherwise wait for its session-close
    // timeout before restarting, which used to leave the new DRM session on a
    // black screen. Reset failed state and stop the capture/file-chooser
    // backends first so the old frontend's session close calls fail
    // immediately instead of waiting for that timeout, then start the new
    // backends (with the environment we just exported) before restarting the
    // frontend so it sees their current capabilities. Other portal backends are
    // left alone so another compositor on another VT can keep using them.
    const SCRIPT: &str = "\
        systemctl --user reset-failed xdg-desktop-portal-wlr.service xdg-desktop-portal-gtk.service xdg-desktop-portal.service\n\
        systemctl --user stop xdg-desktop-portal-wlr.service xdg-desktop-portal-gtk.service\n\
        systemctl --user start xdg-desktop-portal-wlr.service xdg-desktop-portal-gtk.service\n\
        systemctl --user restart xdg-desktop-portal.service";

    spawn_portal_script(SCRIPT, "portal restart helper");
}

fn stop_portal_services() {
    const SCRIPT: &str = "\
        systemctl --user stop xdg-desktop-portal-wlr.service xdg-desktop-portal-gtk.service xdg-desktop-portal.service";

    spawn_portal_script(SCRIPT, "portal stop helper");
}

fn spawn_portal_script(script: &str, what: &str) {
    let mut command = std::process::Command::new("sh");
    command
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    if let Err(err) = crate::spawn::reap(command, what) {
        tracing::warn!("failed to spawn {what}: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::systemd_env_var;

    #[test]
    fn parses_systemd_user_environment() {
        let environment = "WAYLAND_DISPLAY=wayland-1\n\
                           XDG_CURRENT_DESKTOP=niri\n\
                           XDG_SESSION_TYPE=wayland\n\
                           EMPTY=\n";
        assert_eq!(
            systemd_env_var(environment, "WAYLAND_DISPLAY").as_deref(),
            Some("wayland-1")
        );
        assert_eq!(
            systemd_env_var(environment, "XDG_CURRENT_DESKTOP").as_deref(),
            Some("niri")
        );
        assert_eq!(systemd_env_var(environment, "EMPTY").as_deref(), Some(""));
        assert_eq!(systemd_env_var(environment, "MISSING"), None);
    }
}
